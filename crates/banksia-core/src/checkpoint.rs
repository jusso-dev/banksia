//! Local, encrypted checkpoints for interrupted backups.
//!
//! Content is committed to the repository as it is downloaded. The checkpoint
//! remembers which item versions are already stored so a rerun does not
//! download them again. Losing the checkpoint is safe: the next run simply
//! downloads again and deduplicates at the chunk level.

use crate::{crypto::Keys, repo::ContentRef};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Default, Serialize, Deserialize)]
struct Data {
    /// (item id, version marker) -> stored content
    items: BTreeMap<String, ContentRef>,
}

pub struct Checkpoint {
    path: PathBuf,
    aad: String,
    keys: Keys,
    data: Data,
    dirty: usize,
}

impl Checkpoint {
    pub fn open(state_dir: &Path, keys: &Keys, tag: &str, workload: &str) -> Result<Self> {
        let dir = state_dir.join("checkpoints");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let path = dir.join(format!("{tag}-{workload}.bin"));
        let aad = format!("checkpoint/{tag}/{workload}");
        let data = match std::fs::read(&path) {
            Ok(raw) => match keys
                .open(&aad, &raw)
                .ok()
                .and_then(|p| serde_json::from_slice(&p).ok())
            {
                Some(d) => d,
                None => {
                    tracing::warn!(path = %path.display(), "ignoring unreadable checkpoint");
                    Data::default()
                }
            },
            Err(_) => Data::default(),
        };
        Ok(Self {
            path,
            aad,
            keys: keys.clone(),
            data,
            dirty: 0,
        })
    }

    fn key(id: &str, version: &str) -> String {
        format!("{id}\u{1f}{version}")
    }

    pub fn get(&self, id: &str, version: &str) -> Option<&ContentRef> {
        self.data.items.get(&Self::key(id, version))
    }

    pub fn len(&self) -> usize {
        self.data.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.items.is_empty()
    }

    pub fn record(&mut self, id: &str, version: &str, content: ContentRef) -> Result<()> {
        self.data.items.insert(Self::key(id, version), content);
        self.dirty += 1;
        if self.dirty >= 25 {
            self.flush()?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        if self.dirty == 0 {
            return Ok(());
        }
        let plain = serde_json::to_vec(&self.data)?;
        let sealed = self.keys.seal(&self.aad, &plain)?;
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, sealed)?;
        std::fs::rename(&tmp, &self.path)?;
        self.dirty = 0;
        Ok(())
    }

    /// The snapshot was committed; the checkpoint is no longer needed.
    pub fn complete(self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
