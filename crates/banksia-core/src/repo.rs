//! The Banksia repository: encrypted, content-addressed and write once.
//!
//! Layout (format version 1):
//!
//! ```text
//! banksia.repo.json                                  plaintext repository config
//! blobs/<2 hex>/<64 hex>                             encrypted content chunks
//! snapshots/<principal tag>/<workload>/<snapshot id> encrypted snapshot manifests
//! audit/<yyyy>/<mm>/<event id>                       encrypted audit events
//! evidence/<run id>/<file>                           encrypted evidence pack files
//! ```

use crate::{
    crypto::{Keys, MasterKey, Sha256Stream},
    store::{PutOutcome, Store},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const FORMAT_VERSION: u32 = 1;
pub const CONFIG_PATH: &str = "banksia.repo.json";
/// 16 x 320 KiB. Also a valid OneDrive upload-session fragment size.
pub const CHUNK_SIZE: usize = 16 * 320 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub format: String,
    pub version: u32,
    pub repo_id: String,
    pub created: String,
    pub chunk_size: usize,
    pub key_check: String,
}

/// Reference to stored content: ordered chunk ids plus whole-content hashes.
impl RepoConfig {
    fn canonical(&self) -> String {
        format!(
            "{}\n{}\n{}\n{}\n{}",
            self.format, self.version, self.repo_id, self.created, self.chunk_size
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ContentRef {
    pub chunks: Vec<String>,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug)]
pub struct Repository {
    store: Store,
    keys: Keys,
    config: RepoConfig,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WriteStats {
    pub chunks_written: u64,
    pub chunks_deduplicated: u64,
    pub bytes_written: u64,
}

impl Repository {
    /// Initialise a new repository. Fails if one already exists.
    pub async fn init(store: Store, master: &MasterKey) -> Result<Self> {
        let keys = master.derive();
        let mut id = [0u8; 16];
        crate::crypto::fill_random(&mut id);
        let mut config = RepoConfig {
            format: "banksia".into(),
            version: FORMAT_VERSION,
            repo_id: hex::encode(id),
            created: crate::time::now_rfc3339(),
            chunk_size: CHUNK_SIZE,
            key_check: String::new(),
        };
        config.key_check = keys.key_check(&config.canonical());
        let body = serde_json::to_vec_pretty(&config)?;
        if store.put_create(CONFIG_PATH, body).await? == PutOutcome::AlreadyExisted {
            bail!("a Banksia repository already exists at {}", store.label());
        }
        Ok(Self {
            store,
            keys,
            config,
        })
    }

    /// Open an existing repository and check the key before doing anything else.
    pub async fn open(store: Store, master: &MasterKey) -> Result<Self> {
        let raw = store.get(CONFIG_PATH).await?.with_context(|| {
            format!(
                "no Banksia repository at {} (run `banksia repo init`)",
                store.label()
            )
        })?;
        let config: RepoConfig =
            serde_json::from_slice(&raw).context("repository config is not valid")?;
        if config.format != "banksia" {
            bail!("not a Banksia repository");
        }
        if config.version != FORMAT_VERSION {
            bail!("unsupported repository format version {}", config.version);
        }
        let keys = master.derive();
        if keys.key_check(&config.canonical()) != config.key_check {
            bail!("the master key does not match this repository, or its config was altered");
        }
        if config.chunk_size == 0 || config.chunk_size > 64 * 1024 * 1024 {
            bail!("repository chunk size is out of range");
        }
        Ok(Self {
            store,
            keys,
            config,
        })
    }

    pub fn keys(&self) -> &Keys {
        &self.keys
    }

    pub fn config(&self) -> &RepoConfig {
        &self.config
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn principal_tag(&self, object_id: &str) -> String {
        self.keys.principal_tag(object_id)
    }

    fn blob_path(id: &str) -> String {
        format!("blobs/{}/{}", &id[..2], id)
    }

    /// Store one chunk. Returns its id and whether it was new.
    pub async fn put_chunk(&self, chunk: &[u8]) -> Result<(String, bool)> {
        let id = self.keys.blob_id(chunk);
        let path = Self::blob_path(&id);
        if self.store.exists(&path).await? {
            return Ok((id, false));
        }
        let sealed = self.keys.seal(&path, chunk)?;
        let outcome = self.store.put_create(&path, sealed).await?;
        Ok((id, outcome == PutOutcome::Created))
    }

    pub async fn get_chunk(&self, id: &str) -> Result<Vec<u8>> {
        if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("invalid blob id");
        }
        let path = Self::blob_path(id);
        let sealed = self
            .store
            .get(&path)
            .await?
            .with_context(|| format!("blob {id} is missing from the repository"))?;
        let plain = self.keys.open(&path, &sealed)?;
        if self.keys.blob_id(&plain) != id {
            bail!("blob {id} content does not match its id");
        }
        Ok(plain)
    }

    /// Store a whole buffer as chunks.
    pub async fn put_content(&self, data: &[u8], stats: &mut WriteStats) -> Result<ContentRef> {
        let mut w = ContentWriter::new(self);
        w.write(data, stats).await?;
        w.finish(stats).await
    }

    /// Read content back and verify size and SHA-256.
    pub async fn read_content(&self, r: &ContentRef) -> Result<Vec<u8>> {
        // Capacity is bounded: the size field is not trusted until verified.
        let mut out = Vec::with_capacity((r.size as usize).min(64 * 1024 * 1024));
        let mut h = Sha256Stream::default();
        for id in &r.chunks {
            let c = self.get_chunk(id).await?;
            h.update(&c);
            out.extend_from_slice(&c);
        }
        if out.len() as u64 != r.size || h.finish_hex() != r.sha256 {
            bail!("restored content does not match the recorded size or hash");
        }
        Ok(out)
    }

    /// Store an encrypted JSON document at a unique path (create only).
    pub async fn put_doc<T: Serialize>(&self, path: &str, doc: &T) -> Result<PutOutcome> {
        let plain = serde_json::to_vec(doc)?;
        let sealed = self.keys.seal(path, &plain)?;
        self.store.put_create(path, sealed).await
    }

    pub async fn get_doc<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>> {
        let Some(sealed) = self.store.get(path).await? else {
            return Ok(None);
        };
        let plain = self.keys.open(path, &sealed)?;
        Ok(Some(
            serde_json::from_slice(&plain).with_context(|| format!("{path} is not valid"))?,
        ))
    }

    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.store.list(prefix).await
    }
}

/// Streams content into fixed-size chunks with bounded memory.
pub struct ContentWriter<'a> {
    repo: &'a Repository,
    buf: Vec<u8>,
    chunks: Vec<String>,
    size: u64,
    sha: Sha256Stream,
}

impl<'a> ContentWriter<'a> {
    pub fn new(repo: &'a Repository) -> Self {
        Self {
            repo,
            buf: Vec::with_capacity(CHUNK_SIZE.min(1 << 20)),
            chunks: Vec::new(),
            size: 0,
            sha: Sha256Stream::default(),
        }
    }

    pub async fn write(&mut self, mut data: &[u8], stats: &mut WriteStats) -> Result<()> {
        self.sha.update(data);
        self.size += data.len() as u64;
        while !data.is_empty() {
            let take = (CHUNK_SIZE - self.buf.len()).min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() == CHUNK_SIZE {
                self.flush(stats).await?;
            }
        }
        Ok(())
    }

    async fn flush(&mut self, stats: &mut WriteStats) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let (id, new) = self.repo.put_chunk(&self.buf).await?;
        if new {
            stats.chunks_written += 1;
            stats.bytes_written += self.buf.len() as u64;
        } else {
            stats.chunks_deduplicated += 1;
        }
        self.chunks.push(id);
        self.buf.clear();
        Ok(())
    }

    pub async fn finish(mut self, stats: &mut WriteStats) -> Result<ContentRef> {
        self.flush(stats).await?;
        Ok(ContentRef {
            chunks: self.chunks,
            size: self.size,
            sha256: self.sha.finish_hex(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn init_open_and_wrong_key() {
        let store = Store::memory();
        let key = MasterKey::generate();
        Repository::init(store.clone(), &key).await.unwrap();
        assert!(
            Repository::init(store.clone(), &key).await.is_err(),
            "double init must fail"
        );
        Repository::open(store.clone(), &key).await.unwrap();
        let err = Repository::open(store, &MasterKey::generate())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not match"));
    }

    #[tokio::test]
    async fn altered_repo_config_is_rejected() {
        use object_store::ObjectStoreExt;
        let inner: std::sync::Arc<dyn object_store::ObjectStore> =
            std::sync::Arc::new(object_store::memory::InMemory::new());
        let store = Store::from_object_store(inner.clone(), "", "m");
        let key = MasterKey::generate();
        let repo = Repository::init(store.clone(), &key).await.unwrap();
        let mut cfg = repo.config().clone();
        cfg.repo_id = "0".repeat(32);
        inner
            .put(
                &CONFIG_PATH.into(),
                serde_json::to_vec(&cfg).unwrap().into(),
            )
            .await
            .unwrap();
        assert!(Repository::open(store, &key).await.is_err());
    }

    #[tokio::test]
    async fn content_round_trip_dedup_and_chunking() {
        let key = MasterKey::generate();
        let repo = Repository::init(Store::memory(), &key).await.unwrap();
        let data: Vec<u8> = (0..(CHUNK_SIZE * 2 + 100))
            .map(|i| (i % 253) as u8)
            .collect();
        let mut s = WriteStats::default();
        let r = repo.put_content(&data, &mut s).await.unwrap();
        assert_eq!(r.chunks.len(), 3);
        assert_eq!(s.chunks_written, 3);
        assert_eq!(repo.read_content(&r).await.unwrap(), data);

        let mut s2 = WriteStats::default();
        let r2 = repo.put_content(&data, &mut s2).await.unwrap();
        assert_eq!(r, r2);
        assert_eq!(s2.chunks_written, 0);
        assert_eq!(s2.chunks_deduplicated, 3);
    }

    #[tokio::test]
    async fn empty_content() {
        let repo = Repository::init(Store::memory(), &MasterKey::generate())
            .await
            .unwrap();
        let r = repo
            .put_content(b"", &mut WriteStats::default())
            .await
            .unwrap();
        assert!(r.chunks.is_empty());
        assert_eq!(repo.read_content(&r).await.unwrap(), b"");
    }

    #[tokio::test]
    async fn docs_are_encrypted_and_bound_to_path() {
        let store = Store::memory();
        let repo = Repository::init(store.clone(), &MasterKey::generate())
            .await
            .unwrap();
        repo.put_doc(
            "snapshots/x/mail/1",
            &serde_json::json!({"subject": "secret plans"}),
        )
        .await
        .unwrap();
        let raw = store.get("snapshots/x/mail/1").await.unwrap().unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("secret plans"));
        let v: serde_json::Value = repo.get_doc("snapshots/x/mail/1").await.unwrap().unwrap();
        assert_eq!(v["subject"], "secret plans");

        // Copy the ciphertext to another path: it must not open.
        store
            .put_create("snapshots/y/mail/1", raw.to_vec())
            .await
            .unwrap();
        assert!(
            repo.get_doc::<serde_json::Value>("snapshots/y/mail/1")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn tampered_blob_is_detected() {
        let store = Store::memory();
        let repo = Repository::init(store.clone(), &MasterKey::generate())
            .await
            .unwrap();
        // A store that already has a different object at the blob path
        // (simulating tampering before the first write) must fail on read.
        let other = repo.keys().seal("blobs/00/elsewhere", b"evil").unwrap();
        let id = repo.keys().blob_id(b"good");
        store
            .put_create(&format!("blobs/{}/{}", &id[..2], id), other)
            .await
            .unwrap();
        assert!(repo.get_chunk(&id).await.is_err());
    }
}
