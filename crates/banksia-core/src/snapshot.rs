//! Snapshot manifests.
//!
//! A snapshot is a complete point-in-time view of one workload for one user.
//! Unchanged items are carried forward from the parent snapshot, so any
//! snapshot can be restored on its own.

use crate::{config::Workload, repo::ContentRef, repo::Repository};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Principal {
    pub object_id: String,
    pub upn: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotStats {
    pub items: u64,
    pub items_downloaded: u64,
    pub items_carried_forward: u64,
    pub bytes_downloaded: u64,
    pub chunks_written: u64,
    pub chunks_deduplicated: u64,
    pub full_resyncs: u64,
    pub errors: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ItemError {
    pub item_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub version: u32,
    pub id: String,
    pub workload: Workload,
    pub principal: Principal,
    pub principal_tag: String,
    pub started: String,
    pub completed: String,
    pub parent: Option<String>,
    /// True when any item failed. Failed items keep their previous version if one exists.
    pub partial: bool,
    pub stats: SnapshotStats,
    pub errors: Vec<ItemError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mail: Option<MailSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive: Option<DriveSnapshot>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MailSnapshot {
    pub folders: Vec<MailFolderEntry>,
    pub messages: Vec<MessageEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MailFolderEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    /// Slash-separated display-name path from the mailbox root, for example `Inbox/Projects`.
    pub path: String,
    pub hidden: bool,
    pub delta_link: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessageEntry {
    pub id: String,
    pub folder_id: String,
    pub change_key: Option<String>,
    pub subject: Option<String>,
    pub from: Option<String>,
    pub received: Option<String>,
    pub internet_message_id: Option<String>,
    pub content: ContentRef,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DriveSnapshot {
    pub drive_id: String,
    pub delta_link: Option<String>,
    pub items: Vec<DriveEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DriveEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    /// Slash-separated path from the drive root. The root itself is `""`.
    pub path: String,
    pub folder: bool,
    pub size: u64,
    pub c_tag: Option<String>,
    pub e_tag: Option<String>,
    pub quick_xor_hash: Option<String>,
    pub modified: Option<String>,
    pub content: Option<ContentRef>,
}

impl MailSnapshot {
    pub fn folder(&self, id: &str) -> Option<&MailFolderEntry> {
        self.folders.iter().find(|f| f.id == id)
    }
    pub fn folder_by_path(&self, path: &str) -> Option<&MailFolderEntry> {
        let p = path.trim_matches('/');
        self.folders.iter().find(|f| f.path.eq_ignore_ascii_case(p))
    }
}

impl DriveSnapshot {
    pub fn by_path(&self, path: &str) -> Option<&DriveEntry> {
        let p = path.trim_matches('/');
        self.items.iter().find(|i| i.path.eq_ignore_ascii_case(p))
    }
}

pub fn snapshot_prefix(tag: &str, workload: Workload) -> String {
    format!("snapshots/{tag}/{workload}/")
}

pub fn snapshot_path(tag: &str, workload: Workload, id: &str) -> String {
    format!("snapshots/{tag}/{workload}/{id}")
}

fn principal_path(tag: &str) -> String {
    format!("principals/{tag}")
}

fn alias_path(upn_tag: &str) -> String {
    format!("aliases/{upn_tag}")
}

/// Summary used for listing without opening manifests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotRef {
    pub principal_tag: String,
    pub workload: Workload,
    pub id: String,
}

impl Repository {
    pub async fn save_snapshot(&self, snap: &Snapshot) -> Result<()> {
        let p = principal_path(&snap.principal_tag);
        self.put_doc(&p, &snap.principal).await?;
        // Write-once alias per UPN, so lookups keep working after a rename.
        if let Some(upn) = &snap.principal.upn {
            let alias = alias_path(
                &self
                    .keys()
                    .id_hex("upn alias", upn.to_ascii_lowercase().as_bytes())[..32],
            );
            self.put_doc(&alias, &snap.principal_tag).await?;
        }
        let path = snapshot_path(&snap.principal_tag, snap.workload, &snap.id);
        if self.put_doc(&path, snap).await? == crate::store::PutOutcome::AlreadyExisted {
            bail!("snapshot {} already exists", snap.id);
        }
        Ok(())
    }

    pub async fn load_snapshot(&self, tag: &str, workload: Workload, id: &str) -> Result<Snapshot> {
        let snap: Snapshot = self
            .get_doc(&snapshot_path(tag, workload, id))
            .await?
            .with_context(|| format!("snapshot {id} not found"))?;
        if snap.version != SNAPSHOT_VERSION {
            bail!("unsupported snapshot version {}", snap.version);
        }
        if snap.principal_tag != tag || snap.workload != workload || snap.id != id {
            bail!("snapshot {id} does not match its location");
        }
        Ok(snap)
    }

    pub async fn list_snapshots(&self, tag: Option<&str>) -> Result<Vec<SnapshotRef>> {
        let prefix = match tag {
            Some(t) => format!("snapshots/{t}/"),
            None => "snapshots/".into(),
        };
        let mut out = Vec::new();
        for p in self.list(&prefix).await? {
            let parts: Vec<&str> = p.split('/').collect();
            if parts.len() != 4 {
                continue;
            }
            let workload = match parts[2] {
                "mail" => Workload::Mail,
                "drive" => Workload::Drive,
                _ => continue,
            };
            out.push(SnapshotRef {
                principal_tag: parts[1].to_string(),
                workload,
                id: parts[3].to_string(),
            });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    pub async fn latest_snapshot(&self, tag: &str, workload: Workload) -> Result<Option<Snapshot>> {
        let refs = self.list_snapshots(Some(tag)).await?;
        match refs.iter().rev().find(|r| r.workload == workload) {
            Some(r) => Ok(Some(self.load_snapshot(tag, workload, &r.id).await?)),
            None => Ok(None),
        }
    }

    pub async fn principal(&self, tag: &str) -> Result<Option<Principal>> {
        self.get_doc(&principal_path(tag)).await
    }

    /// Find the principal tag for a UPN or object id among backed-up users.
    pub async fn find_principal(&self, upn_or_id: &str) -> Result<Option<(String, Principal)>> {
        let direct = self.principal_tag(upn_or_id);
        if let Some(p) = self.principal(&direct).await? {
            return Ok(Some((direct, p)));
        }
        let alias = alias_path(
            &self
                .keys()
                .id_hex("upn alias", upn_or_id.to_ascii_lowercase().as_bytes())[..32],
        );
        if let Some(tag) = self.get_doc::<String>(&alias).await?
            && let Some(p) = self.principal(&tag).await?
        {
            return Ok(Some((tag, p)));
        }
        for p in self.list("principals/").await? {
            let Some(tag) = p.strip_prefix("principals/") else {
                continue;
            };
            if let Some(pr) = self.principal(tag).await?
                && pr
                    .upn
                    .as_deref()
                    .is_some_and(|u| u.eq_ignore_ascii_case(upn_or_id))
            {
                return Ok(Some((tag.to_string(), pr)));
            }
        }
        Ok(None)
    }
}
