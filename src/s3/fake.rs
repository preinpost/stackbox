//! An in-memory, path-style S3 server for tests. It does not check signatures.

use axum::{
    Router,
    body::Bytes,
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
pub struct FakeS3 {
    pub objects: Mutex<BTreeMap<String, Vec<u8>>>,
    pub uploads: Mutex<BTreeMap<String, BTreeMap<u32, Vec<u8>>>>,
    /// Fail every part upload with this status.
    pub fail_parts: Mutex<Option<StatusCode>>,
    /// Keys of whole objects written, in the order they appeared.
    pub writes: Mutex<Vec<String>>,
}

impl FakeS3 {
    /// Serve on a loopback port; returns the endpoint URL.
    pub async fn start(self: &Arc<Self>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let fake = self.clone();
        let app = Router::new().fallback(
            move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                let fake = fake.clone();
                async move { fake.handle(method, uri, headers, body) }
            },
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    fn handle(&self, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
        let path = decode(uri.path().trim_start_matches('/'));
        let (_bucket, key) = path.split_once('/').unwrap_or((&path, ""));
        let query: BTreeMap<String, String> = uri
            .query()
            .unwrap_or_default()
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (decode(k), decode(v))
            })
            .collect();
        let missing = || {
            (
                StatusCode::NOT_FOUND,
                "<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>",
            )
                .into_response()
        };
        match (method, key.is_empty()) {
            (Method::GET, true) => {
                let prefix = query.get("prefix").cloned().unwrap_or_default();
                let contents: String = self
                    .objects
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(k, _)| k.starts_with(&prefix))
                    .map(|(k, v)| {
                        format!(
                            "<Contents><Key>{}</Key><LastModified>2024-02-29T12:30:05.000Z</LastModified><Size>{}</Size></Contents>",
                            k.replace('&', "&amp;"),
                            v.len()
                        )
                    })
                    .collect();
                format!("<ListBucketResult><IsTruncated>false</IsTruncated>{contents}</ListBucketResult>")
                    .into_response()
            }
            (Method::HEAD, false) => match self.objects.lock().unwrap().get(key) {
                Some(object) => {
                    [(header::CONTENT_LENGTH, object.len().to_string())].into_response()
                }
                None => StatusCode::NOT_FOUND.into_response(),
            },
            (Method::GET, false) => {
                let Some(object) = self.objects.lock().unwrap().get(key).cloned() else {
                    return missing();
                };
                let start = headers.get(header::RANGE).and_then(|r| {
                    r.to_str()
                        .ok()?
                        .strip_prefix("bytes=")?
                        .strip_suffix('-')?
                        .parse::<usize>()
                        .ok()
                });
                match start {
                    Some(start) => (
                        StatusCode::PARTIAL_CONTENT,
                        [(
                            header::CONTENT_RANGE,
                            format!("bytes {start}-{}/{}", object.len() - 1, object.len()),
                        )],
                        object[start..].to_vec(),
                    )
                        .into_response(),
                    None => object.into_response(),
                }
            }
            (Method::PUT, false) => match (query.get("partNumber"), query.get("uploadId")) {
                (Some(number), Some(id)) => {
                    if let Some(status) = *self.fail_parts.lock().unwrap() {
                        return status.into_response();
                    }
                    let mut uploads = self.uploads.lock().unwrap();
                    let Some(parts) = uploads.get_mut(id) else {
                        return StatusCode::NOT_FOUND.into_response();
                    };
                    parts.insert(number.parse().unwrap(), body.to_vec());
                    [(header::ETAG, format!("\"etag-{number}\""))].into_response()
                }
                _ => {
                    self.writes.lock().unwrap().push(key.into());
                    self.objects
                        .lock()
                        .unwrap()
                        .insert(key.into(), body.to_vec());
                    StatusCode::OK.into_response()
                }
            },
            (Method::POST, false) if query.contains_key("uploads") => {
                let id = uuid::Uuid::new_v4().to_string();
                self.uploads
                    .lock()
                    .unwrap()
                    .insert(id.clone(), BTreeMap::new());
                format!("<InitiateMultipartUploadResult><UploadId>{id}</UploadId></InitiateMultipartUploadResult>")
                    .into_response()
            }
            (Method::POST, false) => {
                let Some(parts) = query
                    .get("uploadId")
                    .and_then(|id| self.uploads.lock().unwrap().remove(id))
                else {
                    return StatusCode::NOT_FOUND.into_response();
                };
                let listed = String::from_utf8_lossy(&body);
                let mut object = Vec::new();
                for (number, part) in parts {
                    assert!(listed.contains(&format!(
                        "<PartNumber>{number}</PartNumber><ETag>\"etag-{number}\"</ETag>"
                    )));
                    object.extend(part);
                }
                self.writes.lock().unwrap().push(key.into());
                self.objects.lock().unwrap().insert(key.into(), object);
                "<CompleteMultipartUploadResult/>".into_response()
            }
            (Method::DELETE, false) => {
                match query.get("uploadId") {
                    Some(id) => {
                        self.uploads.lock().unwrap().remove(id);
                    }
                    None => {
                        self.objects.lock().unwrap().remove(key);
                    }
                }
                StatusCode::NO_CONTENT.into_response()
            }
            _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
        }
    }
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = text
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}
