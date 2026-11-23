//! Object storage backends.
//!
//! Every write is a create-only conditional put. Banksia never overwrites or
//! deletes repository objects, so the repository can sit behind storage
//! immutability policies.

use crate::config::StorageConfig;
use anyhow::{Context, Result};
use bytes::Bytes;
use futures::TryStreamExt;
use object_store::{
    ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload,
    aws::{AmazonS3Builder, S3ConditionalPut},
    azure::MicrosoftAzureBuilder,
    local::LocalFileSystem,
    memory::InMemory,
    path::Path as ObjPath,
};
use std::sync::Arc;

/// Outcome of a create-only write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    Created,
    AlreadyExisted,
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<dyn ObjectStore>,
    prefix: String,
    label: String,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("label", &self.label).finish()
    }
}

impl Store {
    pub fn memory() -> Self {
        Self::from_object_store(Arc::new(InMemory::new()), "", "memory")
    }

    pub fn from_object_store(inner: Arc<dyn ObjectStore>, prefix: &str, label: &str) -> Self {
        Self {
            inner,
            prefix: prefix.trim_matches('/').to_string(),
            label: label.to_string(),
        }
    }

    pub fn from_config(cfg: &StorageConfig) -> Result<Self> {
        match cfg {
            StorageConfig::Azure {
                account,
                container,
                prefix,
                access_key,
            } => {
                let mut b = MicrosoftAzureBuilder::from_env()
                    .with_account(account)
                    .with_container_name(container);
                if let Some(k) = access_key {
                    b = b.with_access_key(k.load("storage.access_key")?.expose());
                }
                let s = b.build().context("cannot configure Azure Blob storage")?;
                Ok(Self::from_object_store(
                    Arc::new(s),
                    prefix,
                    &format!("azure://{account}/{container}/{prefix}"),
                ))
            }
            StorageConfig::S3 {
                endpoint,
                bucket,
                region,
                prefix,
                access_key_id,
                secret_access_key,
            } => {
                let s = AmazonS3Builder::new()
                    .with_endpoint(endpoint)
                    .with_bucket_name(bucket)
                    .with_region(region)
                    .with_access_key_id(access_key_id.load("storage.access_key_id")?.expose())
                    .with_secret_access_key(
                        secret_access_key
                            .load("storage.secret_access_key")?
                            .expose(),
                    )
                    // R2 and most S3-compatible stores support If-None-Match.
                    .with_conditional_put(S3ConditionalPut::ETagMatch)
                    .build()
                    .context("cannot configure S3-compatible storage")?;
                Ok(Self::from_object_store(
                    Arc::new(s),
                    prefix,
                    &format!("s3://{bucket}/{prefix}"),
                ))
            }
            StorageConfig::Local { path } => {
                std::fs::create_dir_all(path)
                    .with_context(|| format!("cannot create {}", path.display()))?;
                let s = LocalFileSystem::new_with_prefix(path)
                    .with_context(|| format!("cannot open {}", path.display()))?;
                Ok(Self::from_object_store(
                    Arc::new(s),
                    "",
                    &format!("file://{}", path.display()),
                ))
            }
            StorageConfig::Memory => Ok(Self::memory()),
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    fn full(&self, path: &str) -> ObjPath {
        if self.prefix.is_empty() {
            ObjPath::from(path)
        } else {
            ObjPath::from(format!("{}/{}", self.prefix, path))
        }
    }

    /// Create `path` if it does not exist. An existing object is left alone.
    pub async fn put_create(&self, path: &str, data: Vec<u8>) -> Result<PutOutcome> {
        let opts = PutOptions {
            mode: PutMode::Create,
            ..Default::default()
        };
        match self
            .inner
            .put_opts(&self.full(path), PutPayload::from(data), opts)
            .await
        {
            Ok(_) => Ok(PutOutcome::Created),
            Err(object_store::Error::AlreadyExists { .. }) => Ok(PutOutcome::AlreadyExisted),
            // Some S3-compatible stores report a failed If-None-Match as a precondition error.
            Err(object_store::Error::Precondition { .. }) => Ok(PutOutcome::AlreadyExisted),
            Err(e) => Err(e).with_context(|| format!("cannot write {path} to {}", self.label)),
        }
    }

    pub async fn get(&self, path: &str) -> Result<Option<Bytes>> {
        match self.inner.get(&self.full(path)).await {
            Ok(r) => Ok(Some(
                r.bytes()
                    .await
                    .with_context(|| format!("cannot read {path}"))?,
            )),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("cannot read {path} from {}", self.label)),
        }
    }

    pub async fn exists(&self, path: &str) -> Result<bool> {
        match self.inner.head(&self.full(path)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e).with_context(|| format!("cannot check {path} in {}", self.label)),
        }
    }

    /// List object paths (relative to the store prefix) under `prefix`.
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let full = self.full(prefix.trim_end_matches('/'));
        let strip = if self.prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", self.prefix)
        };
        let metas: Vec<_> = self
            .inner
            .list(Some(&full))
            .try_collect()
            .await
            .with_context(|| format!("cannot list {prefix} in {}", self.label))?;
        let mut out: Vec<String> = metas
            .into_iter()
            .map(|m| {
                let p = m.location.to_string();
                p.strip_prefix(&strip).map(str::to_string).unwrap_or(p)
            })
            .collect();
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn create_only(store: Store) {
        assert_eq!(
            store.put_create("a/b", b"one".to_vec()).await.unwrap(),
            PutOutcome::Created
        );
        assert_eq!(
            store.put_create("a/b", b"two".to_vec()).await.unwrap(),
            PutOutcome::AlreadyExisted
        );
        assert_eq!(store.get("a/b").await.unwrap().unwrap().as_ref(), b"one");
        assert!(store.get("a/c").await.unwrap().is_none());
        store.put_create("a/c/d", b"x".to_vec()).await.unwrap();
        assert_eq!(store.list("a").await.unwrap(), vec!["a/b", "a/c/d"]);
    }

    #[tokio::test]
    async fn memory_is_create_only() {
        create_only(Store::memory()).await;
    }

    #[tokio::test]
    async fn local_is_create_only() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::from_config(&StorageConfig::Local {
            path: dir.path().to_path_buf(),
        })
        .unwrap();
        create_only(s).await;
    }

    #[tokio::test]
    async fn prefix_is_applied_and_stripped() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let s = Store::from_object_store(inner.clone(), "/tenant-a/", "m");
        s.put_create("x/y", b"1".to_vec()).await.unwrap();
        assert!(inner.head(&ObjPath::from("tenant-a/x/y")).await.is_ok());
        assert_eq!(s.list("x").await.unwrap(), vec!["x/y"]);
    }
}
