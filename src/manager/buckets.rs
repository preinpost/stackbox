//! S3-compatible buckets snapshot archives move through. Secret keys are sealed in the vault.

use super::{NotFound, StackboxManager, bad_request, conflict, snapshots::busy};
use crate::{
    model::now,
    s3::{
        Bucket, BucketObject, BucketSecret, Client, RegisterBucket, StoredBucket,
        catalog::{self, ArchiveMetadata, BucketIndex, TemplateQuery},
        client::Object,
    },
};
use anyhow::{Result, anyhow};
use std::collections::BTreeMap;

/// Objects listed for an import; the rest of a large bucket is left out.
const LIST_LIMIT: usize = 5000;

impl StackboxManager {
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
    /// Snapshot archives under the bucket's prefix, newest first, with their metadata. An
    /// index that is missing or behind the metadata files is written again.
    pub async fn bucket_objects(&self, id: &str) -> Result<Vec<BucketObject>> {
        let (bucket, client) = self.s3_client(id)?;
        let client = client.interactive();
        let listed = client.list(&bucket.prefix, LIST_LIMIT).await.map_err(bad)?;
        let (mut found, stale) = catalog_of(&client, &bucket, &listed, true)
            .await
            .map_err(bad)?;
        if stale {
            let index = BucketIndex::new(found.values().cloned().collect());
            // Listing works with a read-only key too; the index is only a cache.
            if let Err(error) = write_index(&client, &bucket, &index).await {
                eprintln!("cannot update the index of bucket {id}: {error:#}");
            }
        }
        let mut objects: Vec<_> = listed
            .into_iter()
            .filter(|o| o.key.ends_with(".msb"))
            .map(|o| BucketObject {
                metadata: found.remove(&o.key),
                key: o.key,
                size_bytes: o.size,
                modified_at: o.modified_at,
            })
            .collect();
        objects.sort_by_key(|o| std::cmp::Reverse(o.modified_at));
        Ok(objects)
    }
    /// Write the bucket's index again from the metadata files alone.
    pub async fn rebuild_bucket_index(&self, id: &str) -> Result<BucketIndex> {
        let (bucket, client) = self.s3_client(id)?;
        let listed = client.list(&bucket.prefix, LIST_LIMIT).await.map_err(bad)?;
        let (found, _) = catalog_of(&client, &bucket, &listed, false)
            .await
            .map_err(bad)?;
        let index = BucketIndex::new(found.into_values().collect());
        write_index(&client, &bucket, &index).await.map_err(bad)?;
        Ok(index)
    }
    /// Templates in the bucket that `query` matches, newest export first. Archives without
    /// a metadata file are left out, since only downloading them would tell.
    pub async fn find_templates(
        &self,
        id: &str,
        query: &TemplateQuery,
    ) -> Result<Vec<ArchiveMetadata>> {
        let mut found: Vec<_> = self
            .bucket_objects(id)
            .await?
            .into_iter()
            .filter_map(|o| o.metadata)
            .filter(|m| query.matches(m))
            .collect();
        found.sort_by_key(|m| std::cmp::Reverse(m.exported_at));
        Ok(found)
    }

    /// Write an uploaded archive's metadata file, which marks the upload complete, then
    /// add it to the index. A failed index update is only reported: listings repair it.
    pub(super) async fn publish_metadata(
        &self,
        bucket: &Bucket,
        client: &Client,
        metadata: ArchiveMetadata,
    ) -> Result<()> {
        client
            .put(
                &catalog::metadata_key(&metadata.key),
                serde_json::to_vec_pretty(&metadata)?.into(),
            )
            .await?;
        if !metadata.key.starts_with(&bucket.prefix) {
            return Ok(());
        }
        let indexed = async {
            let current = client
                .get(&catalog::index_key(&bucket.prefix))
                .await?
                .and_then(|body| BucketIndex::parse(&body));
            let index = match current {
                Some(index) => index.upsert(metadata),
                None => {
                    let listed = client.list(&bucket.prefix, LIST_LIMIT).await?;
                    let (found, _) = catalog_of(client, bucket, &listed, false).await?;
                    BucketIndex::new(found.into_values().collect())
                }
            };
            write_index(client, bucket, &index).await
        }
        .await;
        if let Err(error) = indexed {
            eprintln!("cannot update the index of bucket {}: {error:#}", bucket.id);
        }
        Ok(())
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

/// Metadata of the archives in `listed` that have a metadata file, by archive key: from
/// the index where its entry still matches the archive's size, otherwise read from the
/// file. Also says whether the index was missing or behind.
async fn catalog_of(
    client: &Client,
    bucket: &Bucket,
    listed: &[Object],
    use_index: bool,
) -> Result<(BTreeMap<String, ArchiveMetadata>, bool)> {
    let index_key = catalog::index_key(&bucket.prefix);
    let keys: BTreeMap<&str, u64> = listed.iter().map(|o| (o.key.as_str(), o.size)).collect();
    let index = match use_index && keys.contains_key(index_key.as_str()) {
        true => client
            .get(&index_key)
            .await?
            .and_then(|body| BucketIndex::parse(&body)),
        false => None,
    };
    let mut stale = index.is_none();
    let mut indexed: BTreeMap<String, ArchiveMetadata> = index
        .map(|index| {
            index
                .entries
                .into_iter()
                .map(|entry| (entry.key.clone(), entry))
                .collect()
        })
        .unwrap_or_default();
    let mut found = BTreeMap::new();
    for (&key, &size) in keys.iter().filter(|(k, _)| k.ends_with(".msb")) {
        let path = catalog::metadata_key(key);
        if path == index_key || !keys.contains_key(path.as_str()) {
            continue;
        }
        let metadata = match indexed.remove(key) {
            Some(entry) if entry.size_bytes == size => Some(entry),
            _ => {
                stale = true;
                read_metadata(client, key).await?
            }
        };
        if let Some(metadata) = metadata.filter(|m| m.size_bytes == size) {
            found.insert(key.to_owned(), metadata);
        }
    }
    // Entries whose archive or metadata file is gone.
    stale |= !indexed.is_empty();
    Ok((found, stale))
}

/// The metadata file next to the archive at `key`, if there is a readable one.
pub(super) async fn read_metadata(client: &Client, key: &str) -> Result<Option<ArchiveMetadata>> {
    let path = catalog::metadata_key(key);
    let Some(body) = client.get(&path).await? else {
        return Ok(None);
    };
    match ArchiveMetadata::parse(&body) {
        Some(mut metadata) => {
            // A copied pair keeps the key it was written with.
            metadata.key = key.into();
            Ok(Some(metadata))
        }
        None => {
            eprintln!("ignoring {path}: not a metadata file this version reads");
            Ok(None)
        }
    }
}

async fn write_index(client: &Client, bucket: &Bucket, index: &BucketIndex) -> Result<()> {
    client
        .put(
            &catalog::index_key(&bucket.prefix),
            serde_json::to_vec_pretty(index)?.into(),
        )
        .await
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
