//! S3-compatible buckets snapshot archives move through. Secret keys are sealed in the vault.

use super::{FarmManager, NotFound, bad_request, conflict, snapshots::busy};
use crate::{
    model::now,
    s3::{Bucket, BucketObject, BucketSecret, Client, RegisterBucket, StoredBucket},
};
use anyhow::{Result, anyhow};

/// Objects listed for an import; the rest of a large bucket is left out.
const LIST_LIMIT: usize = 5000;

impl FarmManager {
    pub fn buckets(&self) -> Result<Vec<Bucket>> {
        Ok(self
            .0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?
            .state
            .buckets
            .values()
            .map(|stored| stored.bucket.clone())
            .collect())
    }
    fn stored_bucket(&self, id: &str) -> Result<StoredBucket> {
        self.0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?
            .state
            .buckets
            .get(id)
            .cloned()
            .ok_or_else(|| NotFound(format!("bucket '{id}'")).into())
    }
    pub fn bucket(&self, id: &str) -> Result<Bucket> {
        Ok(self.stored_bucket(id)?.bucket)
    }
    /// The bucket and a client signing with its unsealed key.
    pub(super) fn s3_client(&self, id: &str) -> Result<(Bucket, Client)> {
        let stored = self.stored_bucket(id)?;
        let secret: BucketSecret = self.0.vault.unseal(&stored.secret, &stored.aad())?;
        let client = Client::new(&stored.bucket, &secret.secret_access_key)?;
        Ok((stored.bucket, client))
    }

    /// The bucket `request` describes and the secret to use with it. Without a secret, the
    /// registered bucket's is reused, as long as it would be sent to the same place.
    fn resolve_bucket(&self, request: &RegisterBucket) -> Result<(Bucket, BucketSecret)> {
        let existing = self.stored_bucket(&request.id).ok();
        let bucket = request
            .bucket(existing.as_ref().map_or_else(now, |s| s.bucket.created_at))
            .map_err(|e| bad_request(format!("{e:#}")))?;
        let secret = match (&request.secret_access_key, existing) {
            (Some(secret), _) => BucketSecret {
                secret_access_key: secret.clone(),
            },
            (None, Some(stored))
                if stored.bucket.endpoint == bucket.endpoint
                    && stored.bucket.name == bucket.name
                    && stored.bucket.access_key_id == bucket.access_key_id =>
            {
                self.0.vault.unseal(&stored.secret, &stored.aad())?
            }
            (None, Some(_)) => {
                return Err(bad_request(
                    "enter the secret access key again to change the endpoint, bucket or access key",
                ));
            }
            (None, None) => return Err(bad_request("secret access key is required")),
        };
        Ok((bucket, secret))
    }
    fn seal_bucket(&self, bucket: Bucket, secret: &BucketSecret) -> Result<StoredBucket> {
        let mut stored = StoredBucket {
            bucket,
            credential_ref: uuid::Uuid::new_v4().to_string(),
            secret: crate::remote::SealedCredentials {
                nonce: String::new(),
                ciphertext: String::new(),
            },
        };
        stored.secret = self.0.vault.seal(secret, &stored.aad())?;
        Ok(stored)
    }

    pub fn register_bucket(&self, request: RegisterBucket) -> Result<Bucket> {
        if self.stored_bucket(&request.id).is_ok() {
            return Err(conflict(format!("bucket '{}' already exists", request.id)));
        }
        let (bucket, secret) = self.resolve_bucket(&request)?;
        let stored = self.seal_bucket(bucket, &secret)?;
        self.update(|state| {
            self.ensure_accepting()?;
            if state.buckets.contains_key(&stored.bucket.id) {
                return Err(conflict(format!(
                    "bucket '{}' already exists",
                    stored.bucket.id
                )));
            }
            let bucket = stored.bucket.clone();
            state.buckets.insert(bucket.id.clone(), stored);
            Ok(bucket)
        })
    }
    pub fn update_bucket(&self, request: RegisterBucket) -> Result<Bucket> {
        self.stored_bucket(&request.id)?;
        let (bucket, secret) = self.resolve_bucket(&request)?;
        let stored = self.seal_bucket(bucket, &secret)?;
        self.update(|state| {
            self.ensure_accepting()?;
            ensure_unused(state, &stored.bucket.id)?;
            let slot = state
                .buckets
                .get_mut(&stored.bucket.id)
                .ok_or_else(|| NotFound(format!("bucket '{}'", stored.bucket.id)))?;
            *slot = stored.clone();
            Ok(stored.bucket)
        })
    }
    /// Removes the registration; objects already in the bucket stay there.
    pub fn unregister_bucket(&self, id: &str) -> Result<()> {
        self.update(|state| {
            self.ensure_accepting()?;
            ensure_unused(state, id)?;
            state
                .buckets
                .remove(id)
                .ok_or_else(|| NotFound(format!("bucket '{id}'")))?;
            // Their upload IDs are useless without the bucket's key.
            state.uploads.retain(|_, upload| upload.bucket != id);
            Ok(())
        })
    }

    /// Check settings before saving them; a missing secret reuses the registered one.
    pub async fn test_bucket(&self, request: RegisterBucket) -> Result<String> {
        let (bucket, secret) = self.resolve_bucket(&request)?;
        let client = Client::new(&bucket, &secret.secret_access_key)
            .map_err(bad)?
            .interactive();
        check(&client, &bucket).await
    }
    pub async fn check_bucket(&self, id: &str) -> Result<String> {
        let (bucket, client) = self.s3_client(id)?;
        check(&client.interactive(), &bucket).await
    }
    /// Snapshot archives under the bucket's prefix, newest first.
    pub async fn bucket_objects(&self, id: &str) -> Result<Vec<BucketObject>> {
        let (bucket, client) = self.s3_client(id)?;
        let mut objects: Vec<_> = client
            .interactive()
            .list(&bucket.prefix, LIST_LIMIT)
            .await
            .map_err(bad)?
            .into_iter()
            .filter(|o| o.key.ends_with(".msb"))
            .map(|o| BucketObject {
                key: o.key,
                size_bytes: o.size,
                modified_at: o.modified_at,
            })
            .collect();
        objects.sort_by_key(|o| std::cmp::Reverse(o.modified_at));
        Ok(objects)
    }

    /// Abort multipart uploads a stopped daemon or a failed abort left behind, so they stop
    /// taking billed space. Ones that cannot be aborted now are tried again at the next start.
    pub(super) fn abort_pending_uploads(&self) {
        let Ok(pending) = self.update(|state| Ok(state.uploads.clone())) else {
            return;
        };
        if pending.is_empty() {
            return;
        }
        let manager = self.clone();
        tokio::spawn(async move {
            for (upload_id, upload) in pending {
                let aborted = async {
                    let (_, client) = manager.s3_client(&upload.bucket)?;
                    client.abort_upload(&upload.key, &upload_id).await
                }
                .await;
                match aborted {
                    Ok(()) => manager.forget_upload(&upload_id),
                    Err(error) => eprintln!(
                        "cannot abort the unfinished upload of {} in bucket {}: {error:#}",
                        upload.key, upload.bucket
                    ),
                }
            }
        });
    }
    pub(super) fn forget_upload(&self, upload_id: &str) {
        if let Err(error) = self.update(|state| {
            state.uploads.remove(upload_id);
            Ok(())
        }) {
            eprintln!("cannot record the end of upload {upload_id}: {error:#}");
        }
    }
}

fn ensure_unused(state: &super::store::State, id: &str) -> Result<()> {
    let in_use = state
        .snapshots
        .values()
        .any(|s| busy(s.status) && s.transfer.as_ref().is_some_and(|t| t.bucket == id));
    if in_use {
        return Err(conflict(format!(
            "bucket '{id}' is in use by a running export or import"
        )));
    }
    Ok(())
}

fn bad(error: anyhow::Error) -> anyhow::Error {
    bad_request(format!("{error:#}"))
}

async fn check(client: &Client, bucket: &Bucket) -> Result<String> {
    client.check(&bucket.prefix).await.map_err(bad)?;
    Ok(format!(
        "listed, wrote and deleted a test object under s3://{}/{}",
        bucket.name, bucket.prefix
    ))
}
