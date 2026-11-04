//! S3-compatible buckets that snapshot archives are exported to and imported from.

pub mod catalog;
pub mod client;
#[cfg(test)]
pub(crate) mod fake;

pub use client::{Client, validate_key};

use crate::remote::SealedCredentials;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// A registered bucket. Its secret key is stored encrypted and never returned.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Bucket {
    pub id: String,
    /// `None` for AWS S3, whose endpoint follows from the region.
    pub endpoint: Option<String>,
    pub region: String,
    /// Bucket name in the object store.
    pub name: String,
    /// Key prefix archives go under by default; empty or ending in `/`.
    pub prefix: String,
    /// `https://endpoint/bucket/key` instead of `https://bucket.endpoint/key`.
    pub path_style: bool,
    /// PEM certificates trusted in addition to the system's, for a private CA.
    pub ca_pem: Option<String>,
    pub access_key_id: String,
    pub created_at: u64,
    pub updated_at: u64,
}

/// Register a bucket, or replace a registered one's settings.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterBucket {
    pub id: String,
    /// Origin of an S3-compatible service, such as `https://<account>.r2.cloudflarestorage.com`
    /// or `http://minio:9000`. Omit for AWS S3.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Signing region; defaults to `us-east-1`. R2 uses `auto`.
    #[serde(default)]
    pub region: Option<String>,
    pub name: String,
    #[serde(default)]
    pub prefix: Option<String>,
    /// Defaults to `true` with a custom endpoint and `false` for AWS S3.
    #[serde(default)]
    pub path_style: Option<bool>,
    #[serde(default)]
    pub ca_pem: Option<String>,
    pub access_key_id: String,
    /// Required to register. An update without it keeps the stored key, unless the
    /// endpoint, bucket or access key changes.
    #[serde(default)]
    #[schema(write_only)]
    pub secret_access_key: Option<String>,
}
impl Drop for RegisterBucket {
    fn drop(&mut self) {
        self.secret_access_key.zeroize();
    }
}
impl RegisterBucket {
    /// The bucket these settings describe, normalized.
    pub fn bucket(&self, created_at: u64) -> Result<Bucket> {
        crate::model::validate_id(&self.id).map_err(|_| {
            anyhow::anyhow!(
                "bucket id must be 1-48 lowercase letters, digits or hyphens, starting and ending with a letter or digit"
            )
        })?;
        let region = match self.region.as_deref().map(str::trim) {
            None | Some("") => "us-east-1".to_owned(),
            Some(region) => region.to_owned(),
        };
        ensure!(
            region.len() <= 64
                && region
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'),
            "region must be lowercase letters, digits or hyphens, such as us-east-1 or auto"
        );
        let endpoint = match self.endpoint.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(endpoint) => {
                let url = client::endpoint_url(Some(endpoint), &region)?;
                Some(url.as_str().trim_end_matches('/').to_owned())
            }
        };
        let name = self.name.trim().to_owned();
        ensure!(
            !name.is_empty()
                && name.len() <= 255
                && !name.contains('/')
                && !name.chars().any(|c| c.is_whitespace() || c.is_control()),
            "bucket name must be 1-255 characters without slashes or spaces"
        );
        let prefix = self
            .prefix
            .as_deref()
            .unwrap_or_default()
            .trim()
            .trim_matches('/')
            .to_owned();
        if !prefix.is_empty() {
            ensure!(prefix.len() <= 512, "prefix must be at most 512 bytes");
            validate_key(&prefix).map_err(|e| anyhow::anyhow!("invalid prefix: {e}"))?;
        }
        let ca_pem = match self.ca_pem.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(pem) => {
                ensure!(
                    pem.len() <= 65_536,
                    "CA certificates must be at most 64 KiB"
                );
                client::certificates(pem)?;
                Some(format!("{pem}\n"))
            }
        };
        let access_key_id = self.access_key_id.trim().to_owned();
        ensure!(
            !access_key_id.is_empty()
                && access_key_id.len() <= 256
                && !access_key_id
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control()),
            "access key ID must be 1-256 characters without spaces"
        );
        if let Some(secret) = &self.secret_access_key {
            ensure!(
                !secret.is_empty() && secret.len() <= 256,
                "secret access key must be 1-256 bytes"
            );
        }
        Ok(Bucket {
            id: self.id.clone(),
            path_style: self.path_style.unwrap_or(endpoint.is_some()),
            endpoint,
            region,
            name,
            prefix: if prefix.is_empty() {
                prefix
            } else {
                format!("{prefix}/")
            },
            ca_pem,
            access_key_id,
            created_at,
            updated_at: crate::model::now(),
        })
    }
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct BucketSecret {
    pub secret_access_key: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StoredBucket {
    pub bucket: Bucket,
    pub credential_ref: String,
    pub secret: SealedCredentials,
}
impl StoredBucket {
    /// Binds the secret to where it is sent, so an edited state file cannot redirect it.
    pub fn aad(&self) -> String {
        serde_json::to_string(&(
            "bucket",
            &self.credential_ref,
            &self.bucket.id,
            &self.bucket.endpoint,
            &self.bucket.name,
            &self.bucket.access_key_id,
        ))
        .unwrap()
    }
}

/// A multipart upload that was started but neither completed nor aborted.
#[derive(Clone, Serialize, Deserialize)]
pub struct PendingUpload {
    pub bucket: String,
    pub key: String,
}

#[derive(Serialize, ToSchema)]
pub struct BucketObject {
    pub key: String,
    pub size_bytes: u64,
    pub modified_at: Option<u64>,
    /// From the archive's metadata file; `None` for archives exported before those were
    /// written, which can still be imported.
    pub metadata: Option<catalog::ArchiveMetadata>,
}

#[derive(Serialize, ToSchema)]
pub struct BucketCheck {
    /// What was verified: listing, writing and deleting under the prefix.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> RegisterBucket {
        RegisterBucket {
            id: "minio".into(),
            endpoint: Some(" http://minio:9000/ ".into()),
            region: None,
            name: "farm".into(),
            prefix: Some("/snapshots/dev/".into()),
            path_style: None,
            ca_pem: None,
            access_key_id: "admin".into(),
            secret_access_key: Some("secret".into()),
        }
    }

    #[test]
    fn registrations_are_normalized() {
        let bucket = request().bucket(1).unwrap();
        assert_eq!(bucket.endpoint.as_deref(), Some("http://minio:9000"));
        assert_eq!(bucket.region, "us-east-1");
        assert_eq!(bucket.prefix, "snapshots/dev/");
        assert!(bucket.path_style);
        let mut aws = request();
        aws.endpoint = None;
        aws.prefix = None;
        aws.region = Some("ap-northeast-2".into());
        let aws = aws.bucket(1).unwrap();
        assert!(!aws.path_style && aws.prefix.is_empty() && aws.endpoint.is_none());
        let mut bad = request();
        bad.name = "a/b".into();
        assert!(bad.bucket(1).is_err());
        let mut bad = request();
        bad.ca_pem = Some("-----BEGIN CERTIFICATE-----\nnope".into());
        assert!(bad.bucket(1).is_err());
        let mut bad = request();
        bad.prefix = Some("a/../b".into());
        assert!(bad.bucket(1).is_err());
    }
}
