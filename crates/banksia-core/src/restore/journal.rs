//! Local, encrypted restore journal. Records completed actions per plan.

use crate::crypto::Keys;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Done {
    pub tenant_id: Option<String>,
    pub time: String,
}

pub struct Journal {
    path: PathBuf,
    aad: String,
    keys: Keys,
    pub done: BTreeMap<usize, Done>,
}

impl Journal {
    pub fn open(state_dir: &Path, keys: &Keys, plan_id: &str, plan_hash: &str) -> Result<Self> {
        if !super::valid_plan_id(plan_id) {
            anyhow::bail!("plan id is not valid");
        }
        let dir = state_dir.join("restore-journal");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let path = dir.join(format!("{plan_id}.bin"));
        // Bound to the plan hash: a different plan with the same id cannot reuse it.
        let aad = format!("restore-journal/{plan_id}/{plan_hash}");
        let done = match std::fs::read(&path) {
            Ok(raw) => keys
                .open(&aad, &raw)
                .ok()
                .and_then(|p| serde_json::from_slice(&p).ok())
                .unwrap_or_default(),
            Err(_) => BTreeMap::new(),
        };
        Ok(Self {
            path,
            aad,
            keys: keys.clone(),
            done,
        })
    }

    pub fn record(&mut self, index: usize, tenant_id: Option<String>) -> Result<()> {
        self.done.insert(
            index,
            Done {
                tenant_id,
                time: crate::time::now_rfc3339(),
            },
        );
        let plain = serde_json::to_vec(&self.done)?;
        let sealed = self.keys.seal(&self.aad, &plain)?;
        let mut r = [0u8; 6];
        crate::crypto::fill_random(&mut r);
        let tmp =
            self.path
                .with_extension(format!("{}.{}.tmp", std::process::id(), hex::encode(r)));
        std::fs::write(&tmp, sealed)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}
