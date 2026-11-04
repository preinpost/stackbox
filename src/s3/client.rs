//! A SigV4 client limited to what AWS S3, Cloudflare R2, MinIO and RustFS all implement.
//! Bodies are signed with their SHA-256 instead of streamed as `aws-chunked`, and no
//! flexible-checksum headers are sent, since S3-compatible servers differ on both.

use anyhow::{Context, Result, anyhow, ensure};
use bytes::Bytes;
use reqwest::{Method, Response, StatusCode, Url, header};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use zeroize::Zeroizing;

const MIB: u64 = 1024 * 1024;
const MAX_PARTS: u64 = 10_000;
const ATTEMPTS: u32 = 4;

/// AWS's endpoint for `region` when none is given, otherwise `endpoint` checked to be a bare origin.
pub fn endpoint_url(endpoint: Option<&str>, region: &str) -> Result<Url> {
    let raw = match endpoint {
        Some(endpoint) => endpoint.to_owned(),
        None => format!("https://s3.{region}.amazonaws.com"),
    };
    let url = Url::parse(&raw).with_context(|| format!("invalid endpoint '{raw}'"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "endpoint must start with http:// or https://"
    );
    ensure!(
        url.host_str().is_some()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none(),
        "endpoint must be a scheme and host with an optional port, such as https://minio.example:9000"
    );
    Ok(url)
}

pub fn certificates(pem: &str) -> Result<Vec<reqwest::Certificate>> {
    let certificates = reqwest::Certificate::from_pem_bundle(pem.as_bytes())
        .context("CA certificates must be PEM")?;
    ensure!(
        !certificates.is_empty(),
        "CA certificates must contain at least one PEM certificate"
    );
    Ok(certificates)
}

/// Object keys are signed as sent, so keep out segments a URL parser would rewrite.
pub fn validate_key(key: &str) -> Result<()> {
    ensure!(
        !key.is_empty() && key.len() <= 1024,
        "object key must be 1-1024 bytes"
    );
    ensure!(
        !key.chars().any(char::is_control),
        "object key cannot contain control characters"
    );
    ensure!(
        key.split('/')
            .all(|s| !s.is_empty() && s != "." && s != ".."),
        "object key cannot start or end with /, or have empty, . or .. segments"
    );
    Ok(())
}

/// What an upload or download moved: its size and the hex SHA-256 of its bytes.
#[derive(Debug, PartialEq)]
pub struct Transferred {
    pub size: u64,
    pub sha256: String,
}

pub struct Object {
    pub key: String,
    pub size: u64,
    pub modified_at: Option<u64>,
}

pub struct Client {
    http: reqwest::Client,
    base: Url,
    /// `/<bucket>` in path style; empty when the bucket is part of the host name.
    root: String,
    region: String,
    access_key_id: String,
    secret: Zeroizing<String>,
    /// Size of every part but the last. R2 requires them to be equal.
    min_part: u64,
    attempts: u32,
    /// Cap on each request's time, for answers someone is waiting on.
    limit: Option<Duration>,
}

impl Client {
    pub fn new(bucket: &super::Bucket, secret_access_key: &str) -> Result<Self> {
        let mut base = endpoint_url(bucket.endpoint.as_deref(), &bucket.region)?;
        let root = if bucket.path_style {
            format!("/{}", encode(&bucket.name, false))
        } else {
            let host = format!("{}.{}", bucket.name, base.host_str().unwrap_or_default());
            base.set_host(Some(&host)).with_context(|| {
                format!(
                    "bucket '{}' cannot be a host name; use path-style addressing",
                    bucket.name
                )
            })?;
            String::new()
        };
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(120))
            // A redirect would carry a signature made for another host.
            .redirect(reqwest::redirect::Policy::none());
        if let Some(pem) = &bucket.ca_pem {
            builder = builder.tls_certs_merge(certificates(pem)?);
        }
        Ok(Self {
            http: builder.build()?,
            base,
            root,
            region: bucket.region.clone(),
            access_key_id: bucket.access_key_id.clone(),
            secret: Zeroizing::new(secret_access_key.to_owned()),
            min_part: 64 * MIB,
            attempts: ATTEMPTS,
            limit: None,
        })
    }
    /// One quick attempt per request, so a person checking settings gets the error while
    /// the portal still waits; transfers keep retrying.
    pub fn interactive(mut self) -> Self {
        self.attempts = 1;
        self.limit = Some(Duration::from_secs(20));
        self
    }
    #[cfg(test)]
    pub fn with_min_part(mut self, bytes: u64) -> Self {
        self.min_part = bytes;
        self
    }

    /// Lists, writes and deletes a small object under `prefix`, which is what exports and imports need.
    pub async fn check(&self, prefix: &str) -> Result<()> {
        self.list(prefix, 1).await?;
        let key = format!("{prefix}.farm-check-{}", uuid::Uuid::new_v4());
        let call = Call::new(Method::PUT, Some(&key)).body(Bytes::from_static(b"farm"));
        expect(self.send(call).await?, "write a test object").await?;
        self.delete(&key)
            .await
            .with_context(|| format!("the test object {key} was written but not deleted"))
    }

    pub async fn list(&self, prefix: &str, limit: usize) -> Result<Vec<Object>> {
        let mut objects = Vec::new();
        let mut token = None;
        loop {
            let mut call = Call::new(Method::GET, None)
                .query("list-type", "2")
                .query("prefix", prefix)
                .query("max-keys", (limit - objects.len()).min(1000).to_string());
            if let Some(token) = token {
                call = call.query("continuation-token", token);
            }
            let body = expect(self.send(call).await?, "list objects")
                .await?
                .text()
                .await?;
            for block in blocks(&body, "Contents") {
                objects.push(Object {
                    key: tag(block, "Key").context("listed object has no key")?,
                    size: tag(block, "Size").and_then(|s| s.parse().ok()).unwrap_or(0),
                    modified_at: tag(block, "LastModified").and_then(|t| parse_time(&t)),
                });
            }
            token = tag(&body, "NextContinuationToken");
            if objects.len() >= limit
                || tag(&body, "IsTruncated").as_deref() != Some("true")
                || token.is_none()
            {
                return Ok(objects);
            }
        }
    }

    /// Size of the object, or `None` when there is none.
    pub async fn head(&self, key: &str) -> Result<Option<u64>> {
        let response = self.send(Call::new(Method::HEAD, Some(key))).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = expect(response, "read object metadata").await?;
        // HEAD has no body, so read the header rather than the body length.
        Ok(response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()?.parse().ok()))
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        let response = self.send(Call::new(Method::DELETE, Some(key))).await?;
        if response.status() != StatusCode::NOT_FOUND {
            expect(response, "delete object").await?;
        }
        Ok(())
    }

    /// A small object read whole, or `None` when there is none.
    pub async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        let response = self.send(Call::new(Method::GET, Some(key))).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(expect(response, "read object").await?.bytes().await?))
    }
    /// Write a small object in one request, replacing any there.
    pub async fn put(&self, key: &str, body: Bytes) -> Result<()> {
        let call = Call::new(Method::PUT, Some(key))
            .header("content-type", "application/json".into())
            .body(body);
        expect(self.send(call).await?, "write object").await?;
        Ok(())
    }

    pub async fn start_upload(&self, key: &str) -> Result<String> {
        let call = Call::new(Method::POST, Some(key)).query("uploads", "");
        let body = expect(self.send(call).await?, "start multipart upload")
            .await?
            .text()
            .await?;
        tag(&body, "UploadId").context("multipart upload response has no UploadId")
    }

    /// Upload `path` in equal parts and complete the upload. `progress` gets the bytes sent
    /// so far. The parts are hashed as they are read, so the file is read once.
    pub async fn upload(
        &self,
        path: &Path,
        key: &str,
        upload_id: &str,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<Transferred> {
        let mut hash = Sha256::new();
        let mut file = tokio::fs::File::open(path).await?;
        let size = file.metadata().await?.len();
        let part = part_size(size, self.min_part);
        let mut parts = String::new();
        let mut sent = 0;
        progress(0, size);
        for number in 1..=size.div_ceil(part).max(1) {
            let mut body = vec![0; part.min(size - sent) as usize];
            file.read_exact(&mut body).await?;
            hash.update(&body);
            let length = body.len() as u64;
            let call = Call::new(Method::PUT, Some(key))
                .query("partNumber", number.to_string())
                .query("uploadId", upload_id)
                .body(Bytes::from(body))
                .timeout(Some(Duration::from_secs(1800)));
            let response = expect(self.send(call).await?, &format!("upload part {number}")).await?;
            let etag = response
                .headers()
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok())
                .with_context(|| format!("part {number} response has no ETag"))?;
            parts.push_str(&format!(
                "<Part><PartNumber>{number}</PartNumber><ETag>{}</ETag></Part>",
                escape(etag)
            ));
            sent += length;
            progress(sent, size);
        }
        let call = Call::new(Method::POST, Some(key))
            .query("uploadId", upload_id)
            .body(Bytes::from(format!(
                "<CompleteMultipartUpload>{parts}</CompleteMultipartUpload>"
            )))
            .timeout(Some(Duration::from_secs(900)));
        let response = expect(self.send(call).await?, "complete multipart upload").await?;
        let status = response.status();
        // Completion can fail after the server already answered 200.
        let body = response.text().await?;
        if body.contains("<Error>") {
            return Err(describe(status, &body, "complete multipart upload"));
        }
        Ok(Transferred {
            size,
            sha256: hex(&hash.finalize()),
        })
    }

    pub async fn abort_upload(&self, key: &str, upload_id: &str) -> Result<()> {
        let call = Call::new(Method::DELETE, Some(key)).query("uploadId", upload_id);
        let response = self.send(call).await?;
        if response.status() != StatusCode::NOT_FOUND {
            expect(response, "abort multipart upload").await?;
        }
        Ok(())
    }

    /// Download to `path`, resuming with a range request when the connection drops.
    pub async fn download(
        &self,
        key: &str,
        path: &Path,
        mut progress: impl FnMut(u64, Option<u64>),
    ) -> Result<Transferred> {
        let mut file = tokio::fs::File::create(path).await?;
        // A resumed range continues the hash; a restart from zero starts it over.
        let mut hash = Sha256::new();
        let mut done = 0;
        let mut total = None;
        let mut failures = 0;
        loop {
            let mut call = Call::new(Method::GET, Some(key)).timeout(None);
            if done > 0 {
                call = call.header("range", format!("bytes={done}-"));
            }
            let mut response = expect(self.send(call).await?, "download object").await?;
            if response.status() == StatusCode::PARTIAL_CONTENT {
                total = total.or_else(|| {
                    let range = response.headers().get(header::CONTENT_RANGE)?;
                    range.to_str().ok()?.rsplit_once('/')?.1.parse().ok()
                });
            } else {
                // The whole object: the first request, or a server that ignored the range.
                if done > 0 {
                    file.set_len(0).await?;
                    file.rewind().await?;
                    done = 0;
                    hash = Sha256::new();
                }
                total = response
                    .headers()
                    .get(header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok()?.parse().ok());
            }
            progress(done, total);
            let streamed = async {
                while let Some(chunk) = response.chunk().await? {
                    file.write_all(&chunk).await?;
                    hash.update(&chunk);
                    done += chunk.len() as u64;
                    progress(done, total);
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            match streamed {
                Ok(()) => break,
                Err(error)
                    if failures + 1 < ATTEMPTS
                        && error.downcast_ref::<reqwest::Error>().is_some() =>
                {
                    failures += 1;
                    tokio::time::sleep(Duration::from_secs(1 << failures)).await;
                }
                Err(error) => return Err(error.context("download object")),
            }
        }
        if let Some(total) = total {
            ensure!(
                done == total,
                "download ended after {done} of {total} bytes"
            );
        }
        file.sync_all().await?;
        Ok(Transferred {
            size: done,
            sha256: hex(&hash.finalize()),
        })
    }

    async fn send(&self, call: Call<'_>) -> Result<Response> {
        let path = match call.key {
            Some(key) => format!("{}/{}", self.root, encode(key, true)),
            None if self.root.is_empty() => "/".into(),
            None => self.root.clone(),
        };
        let mut query: Vec<_> = call
            .query
            .iter()
            .map(|(name, value)| (encode(name, false), encode(value, false)))
            .collect();
        query.sort();
        let query = query
            .iter()
            .map(|(n, v)| format!("{n}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let mut url = self.base.clone();
        url.set_path(&path);
        url.set_query((!query.is_empty()).then_some(query.as_str()));
        let host = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
            None => url.host_str().unwrap_or_default().to_owned(),
        };
        let payload = hex(&Sha256::digest(&call.body));
        let mut attempt = 0;
        loop {
            attempt += 1;
            let date = amz_date(SystemTime::now());
            let mut headers: Vec<(String, String)> = call
                .headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.clone()))
                .collect();
            headers.push(("host".into(), host.clone()));
            headers.push(("x-amz-content-sha256".into(), payload.clone()));
            headers.push(("x-amz-date".into(), date.clone()));
            // Sign the path and query as the URL will send them.
            let authorization = self.authorization(
                call.method.as_str(),
                url.path(),
                url.query().unwrap_or_default(),
                &mut headers,
                &payload,
                &date,
            );
            let mut request = self
                .http
                .request(call.method.clone(), url.clone())
                .header(header::AUTHORIZATION, authorization)
                .body(call.body.clone());
            if let Some(timeout) = match (call.timeout, self.limit) {
                (Some(timeout), Some(limit)) => Some(timeout.min(limit)),
                (timeout, limit) => timeout.or(limit),
            } {
                request = request.timeout(timeout);
            }
            for (name, value) in headers.iter().filter(|(name, _)| name != "host") {
                request = request.header(name, value);
            }
            let result = request.send().await;
            let transient = match &result {
                Ok(response) => {
                    response.status().is_server_error()
                        || response.status() == StatusCode::TOO_MANY_REQUESTS
                }
                Err(error) => !error.is_builder(),
            };
            if !transient || attempt >= self.attempts {
                return result.with_context(|| format!("{} {}", call.method, url.path()));
            }
            tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
        }
    }

    /// The SigV4 `Authorization` header. `headers` are lowercase and get sorted.
    fn authorization(
        &self,
        method: &str,
        path: &str,
        query: &str,
        headers: &mut [(String, String)],
        payload: &str,
        date: &str,
    ) -> String {
        headers.sort();
        let canonical_headers: String = headers
            .iter()
            .map(|(name, value)| format!("{name}:{}\n", value.trim()))
            .collect();
        let signed = headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let canonical =
            format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed}\n{payload}");
        let day = &date[..8];
        let scope = format!("{day}/{}/s3/aws4_request", self.region);
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{date}\n{scope}\n{}",
            hex(&Sha256::digest(canonical))
        );
        let secret = Zeroizing::new(format!("AWS4{}", *self.secret));
        let mut key = hmac(secret.as_bytes(), day.as_bytes());
        for part in [self.region.as_str(), "s3", "aws4_request"] {
            key = hmac(&key, part.as_bytes());
        }
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={}",
            self.access_key_id,
            hex(&hmac(&key, to_sign.as_bytes()))
        )
    }
}

struct Call<'a> {
    method: Method,
    key: Option<&'a str>,
    query: Vec<(&'static str, String)>,
    headers: Vec<(&'static str, String)>,
    body: Bytes,
    timeout: Option<Duration>,
}
impl<'a> Call<'a> {
    fn new(method: Method, key: Option<&'a str>) -> Self {
        Self {
            method,
            key,
            query: Vec::new(),
            headers: Vec::new(),
            body: Bytes::new(),
            timeout: Some(Duration::from_secs(120)),
        }
    }
    fn query(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.query.push((name, value.into()));
        self
    }
    fn header(mut self, name: &'static str, value: String) -> Self {
        self.headers.push((name, value));
        self
    }
    fn body(mut self, body: Bytes) -> Self {
        self.body = body;
        self
    }
    fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Equal parts of at least `min`, few enough for the 10,000-part limit.
fn part_size(size: u64, min: u64) -> u64 {
    min.max(size.div_ceil(MAX_PARTS).div_ceil(MIB) * MIB)
}

async fn expect(response: Response, what: &str) -> Result<Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(describe(status, &body, what))
}
fn describe(status: StatusCode, body: &str, what: &str) -> anyhow::Error {
    match (tag(body, "Code"), tag(body, "Message")) {
        (Some(code), Some(message)) => anyhow!("{what}: {code}: {message} (HTTP {status})"),
        (Some(code), None) => anyhow!("{what}: {code} (HTTP {status})"),
        _ => anyhow!("{what}: HTTP {status}"),
    }
}

/// S3 URI encoding: every byte but unreserved characters, and `/` in object paths.
fn encode(value: &str, path: bool) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) || (path && byte == b'/') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(data);
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// Inner text of each `<name>` element. S3 responses are flat enough for this.
fn blocks<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{name}>"), format!("</{name}>"));
    let mut found = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else { break };
        found.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    found
}
fn tag(xml: &str, name: &str) -> Option<String> {
    blocks(xml, name).first().map(|text| unescape(text))
}
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let decoded = rest.find(';').and_then(|end| {
            let c = match &rest[1..end] {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                entity => {
                    let code = match entity.strip_prefix("#x").or(entity.strip_prefix("#X")) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => entity.strip_prefix('#')?.parse().ok()?,
                    };
                    char::from_u32(code)?
                }
            };
            Some((c, end))
        });
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn amz_date(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let (year, month, day) = civil(secs.div_euclid(86_400));
    let rest = secs.rem_euclid(86_400);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}
/// Seconds since the epoch of an ISO 8601 UTC time such as `2024-05-01T12:30:00.000Z`.
fn parse_time(text: &str) -> Option<u64> {
    let (date, time) = text.split_once('T')?;
    let mut date = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let time = time.trim_end_matches('Z').split('.').next()?;
    let mut time = time.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (hour, minute, second) = (time.next()??, time.next()??, time.next()??);
    u64::try_from(days(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second).ok()
}
/// Civil date of a day count since 1970-01-01 (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}
fn days(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example(path_style: bool) -> Client {
        let bucket = super::super::Bucket {
            id: "example".into(),
            endpoint: None,
            region: "us-east-1".into(),
            name: "examplebucket".into(),
            prefix: String::new(),
            path_style,
            ca_pem: None,
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            created_at: 0,
            updated_at: 0,
        };
        Client::new(&bucket, "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY").unwrap()
    }

    impl Client {
        fn bucket_for_test(&self) -> super::super::Bucket {
            super::super::Bucket {
                id: "test".into(),
                endpoint: None,
                region: self.region.clone(),
                name: "examplebucket".into(),
                prefix: String::new(),
                path_style: true,
                ca_pem: None,
                access_key_id: self.access_key_id.clone(),
                created_at: 0,
                updated_at: 0,
            }
        }
    }

    #[test]
    fn signatures_match_the_aws_examples() {
        const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let client = example(false);
        let headers = |extra: &[(&str, &str)]| {
            let mut headers: Vec<(String, String)> = [
                ("host", "examplebucket.s3.amazonaws.com"),
                ("x-amz-content-sha256", EMPTY),
                ("x-amz-date", "20130524T000000Z"),
            ]
            .iter()
            .chain(extra)
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect();
            headers.reverse();
            headers
        };
        // GET Object with a range.
        let authorization = client.authorization(
            "GET",
            "/test.txt",
            "",
            &mut headers(&[("range", "bytes=0-9")]),
            EMPTY,
            "20130524T000000Z",
        );
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        // ListObjects.
        let authorization = client.authorization(
            "GET",
            "/",
            "max-keys=2&prefix=J",
            &mut headers(&[]),
            EMPTY,
            "20130524T000000Z",
        );
        assert!(authorization.ends_with(
            "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        ));
        assert_eq!(
            hex(&hmac(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn addressing_encoding_and_parts() {
        assert_eq!(
            example(false).base.as_str(),
            "https://examplebucket.s3.us-east-1.amazonaws.com/"
        );
        let client = example(true);
        assert_eq!(client.root, "/examplebucket");
        assert_eq!(encode("a b/ü+=.msb", true), "a%20b/%C3%BC%2B%3D.msb");
        assert_eq!(encode("a/b", false), "a%2Fb");
        assert!(endpoint_url(Some("http://minio:9000"), "us-east-1").is_ok());
        for bad in [
            "minio:9000",
            "ftp://minio",
            "https://minio/path",
            "https://u:p@minio",
        ] {
            assert!(endpoint_url(Some(bad), "us-east-1").is_err(), "{bad}");
        }
        for bad in ["", "/a", "a/", "a//b", "a/../b", "./a", "a\nb"] {
            assert!(validate_key(bad).is_err(), "{bad:?}");
        }
        validate_key("farm/dev-clean.msb").unwrap();
        assert_eq!(part_size(0, 64 * MIB), 64 * MIB);
        assert_eq!(part_size(1024 * 1024 * MIB, 64 * MIB), 105 * MIB);
        assert!(certificates("not a certificate").is_err());
    }

    #[tokio::test]
    async fn multipart_uploads_and_resumable_downloads_round_trip() {
        let fake = std::sync::Arc::new(super::super::fake::FakeS3::default());
        let mut bucket = example(true).bucket_for_test();
        bucket.endpoint = Some(fake.start().await);
        let client = Client::new(&bucket, "secret").unwrap().with_min_part(MIB);
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let data: Vec<u8> = (0..(5 * MIB / 2) as u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&source, &data).unwrap();
        let key = "dir/a b+c.msb";
        assert_eq!(client.head(key).await.unwrap(), None);
        let upload_id = client.start_upload(key).await.unwrap();
        let mut reports = Vec::new();
        let uploaded = client
            .upload(&source, key, &upload_id, |done, total| {
                reports.push((done, total))
            })
            .await
            .unwrap();
        let size = data.len() as u64;
        let sha256 = hex(&Sha256::digest(&data));
        assert_eq!(
            uploaded,
            Transferred {
                size,
                sha256: sha256.clone()
            }
        );
        assert_eq!(
            reports,
            [(0, size), (MIB, size), (2 * MIB, size), (size, size)]
        );
        assert_eq!(fake.objects.lock().unwrap()[key], data);
        assert_eq!(client.head(key).await.unwrap(), Some(size));
        let listed = client.list("dir/", 10).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].key.as_str(), listed[0].size), (key, size));
        let copy = dir.path().join("copy");
        let mut last = (0, None);
        assert_eq!(
            client
                .download(key, &copy, |done, total| last = (done, total))
                .await
                .unwrap(),
            Transferred { size, sha256 }
        );
        assert_eq!(last, (size, Some(size)));
        assert_eq!(std::fs::read(&copy).unwrap(), data);
        let error = client
            .download("missing", &copy, |_, _| {})
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("NoSuchKey"));
        client
            .put("dir/a.json", Bytes::from_static(b"{}"))
            .await
            .unwrap();
        assert_eq!(client.get("dir/a.json").await.unwrap().unwrap(), "{}");
        assert_eq!(client.get("dir/none.json").await.unwrap(), None);
        client.delete("dir/a.json").await.unwrap();
        client.delete(key).await.unwrap();
        assert!(fake.objects.lock().unwrap().is_empty());
    }

    /// Against a real service: `FARM_S3_BUCKET`, `FARM_S3_ACCESS_KEY`, `FARM_S3_SECRET_KEY`, and
    /// optionally `FARM_S3_ENDPOINT`, `FARM_S3_REGION`, `FARM_S3_PREFIX`, `FARM_S3_PATH_STYLE`,
    /// `FARM_S3_CA_FILE`. Run with `cargo test s3_live -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn s3_live() {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let request = super::super::RegisterBucket {
            id: "live".into(),
            endpoint: var("FARM_S3_ENDPOINT"),
            region: var("FARM_S3_REGION"),
            name: var("FARM_S3_BUCKET").expect("FARM_S3_BUCKET"),
            prefix: var("FARM_S3_PREFIX"),
            path_style: var("FARM_S3_PATH_STYLE").map(|v| v == "true"),
            ca_pem: var("FARM_S3_CA_FILE").map(|f| std::fs::read_to_string(f).unwrap()),
            access_key_id: var("FARM_S3_ACCESS_KEY").expect("FARM_S3_ACCESS_KEY"),
            secret_access_key: var("FARM_S3_SECRET_KEY"),
        };
        let bucket = request.bucket(0).unwrap();
        let secret = request.secret_access_key.clone().unwrap();
        // The smallest part S3 accepts, so a few MiB take several parts.
        let client = Client::new(&bucket, &secret)
            .unwrap()
            .with_min_part(5 * MIB);
        client.check(&bucket.prefix).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let data: Vec<u8> = (0..12 * MIB as u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        std::fs::write(&source, &data).unwrap();
        let key = format!("{}live test+{}.msb", bucket.prefix, uuid::Uuid::new_v4());
        let size = data.len() as u64;
        let upload_id = client.start_upload(&key).await.unwrap();
        let mut parts = 0;
        client
            .upload(&source, &key, &upload_id, |_, _| parts += 1)
            .await
            .unwrap();
        assert_eq!(parts, 4);
        assert_eq!(client.head(&key).await.unwrap(), Some(size));
        let listed = client.list(&bucket.prefix, 1000).await.unwrap();
        let object = listed.iter().find(|o| o.key == key).expect("listed");
        assert_eq!(object.size, size);
        assert!(object.modified_at.is_some());
        let copy = dir.path().join("copy");
        assert_eq!(
            client.download(&key, &copy, |_, _| {}).await.unwrap().size,
            size
        );
        assert!(std::fs::read(&copy).unwrap() == data);
        client.delete(&key).await.unwrap();
        assert_eq!(client.head(&key).await.unwrap(), None);
        let abandoned = client.start_upload(&key).await.unwrap();
        client.abort_upload(&key, &abandoned).await.unwrap();
        assert_eq!(client.head(&key).await.unwrap(), None);
    }

    #[test]
    fn xml_and_times() {
        let body = "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>a&amp;b&#x20;c.msb</Key><Size>7</Size><LastModified>2024-02-29T12:30:05.000Z</LastModified></Contents><Contents><Key>d</Key></Contents></ListBucketResult>";
        let contents = blocks(body, "Contents");
        assert_eq!(contents.len(), 2);
        assert_eq!(tag(contents[0], "Key").unwrap(), "a&b c.msb");
        assert_eq!(
            parse_time(&tag(contents[0], "LastModified").unwrap()),
            Some(1_709_209_805)
        );
        assert_eq!(unescape("&bogus; &lt;&"), "&bogus; <&");
        assert_eq!(
            amz_date(UNIX_EPOCH + Duration::from_secs(1_709_209_805)),
            "20240229T123005Z"
        );
        assert_eq!(civil(days(1999, 12, 31)), (1999, 12, 31));
        let error = describe(
            StatusCode::FORBIDDEN,
            "<Error><Code>SignatureDoesNotMatch</Code><Message>bad &quot;key&quot;</Message></Error>",
            "list objects",
        );
        assert_eq!(
            error.to_string(),
            "list objects: SignatureDoesNotMatch: bad \"key\" (HTTP 403 Forbidden)"
        );
    }
}
