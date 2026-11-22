//! Item-level restore: plan, approve by hash, apply.
//!
//! Planning only reads the repository. Applying writes into the tenant and is
//! additive: it creates folders, messages and files, never overwrites or
//! deletes. Every action is journalled so an interrupted apply can be rerun.

pub mod apply;
mod journal;

use crate::{
    config::Workload,
    repo::{ContentRef, Repository},
    snapshot::{MailSnapshot, Snapshot},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const PLAN_VERSION: u32 = 1;
pub const RESTORE_ROOT: &str = "Banksia Restore";
/// Minimum number of plan hash characters accepted as approval.
pub const MIN_APPROVAL_PREFIX: usize = 12;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RestorePlan {
    pub version: u32,
    pub plan_id: String,
    pub created: String,
    pub created_by: String,
    pub purpose: String,
    pub repo_id: String,
    pub source: PlanSource,
    pub target: PlanTarget,
    pub mode: RestoreMode,
    pub summary: PlanSummary,
    pub actions: Vec<Action>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanSource {
    pub principal_tag: String,
    pub user: Option<String>,
    pub workload: Workload,
    pub snapshot_id: String,
    pub snapshot_completed: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanTarget {
    /// As given by the operator (UPN or object id).
    pub user: String,
    /// Entra object id resolved at plan time. Apply refuses if it changes.
    pub object_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RestoreMode {
    /// Recreate the original folder structure in the target. Files that
    /// already exist with different content are restored under a new name.
    InPlace,
    /// Restore under a new folder, for example `Banksia Restore/<plan id>`.
    Alternate { root: Vec<String> },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanSummary {
    pub mail_folders: u64,
    pub messages: u64,
    pub drive_folders: u64,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    MailFolder {
        /// Display-name components from the mailbox root.
        path: Vec<String>,
    },
    Message {
        source_id: String,
        folder: Vec<String>,
        internet_message_id: Option<String>,
        subject: Option<String>,
        content: ContentRef,
    },
    DriveFolder {
        path: String,
    },
    File {
        source_id: String,
        path: String,
        size: u64,
        quick_xor_hash: Option<String>,
        content: ContentRef,
    },
}

impl Action {
    pub fn describe(&self) -> String {
        match self {
            Action::MailFolder { path } => format!("mail folder  {}", path.join("/")),
            Action::Message {
                folder, subject, ..
            } => {
                format!(
                    "message      {} :: {}",
                    folder.join("/"),
                    subject.as_deref().unwrap_or("(no subject)")
                )
            }
            Action::DriveFolder { path } => format!("drive folder {path}"),
            Action::File { path, size, .. } => format!("file         {path} ({size} bytes)"),
        }
    }
}

/// What to restore from a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    All,
    MailMessages(Vec<String>),
    /// A folder path such as `Inbox/Projects`, including subfolders.
    MailFolder(String),
    DriveItems(Vec<String>),
    /// A file or folder path, including everything below a folder.
    DrivePath(String),
}

pub struct PlanRequest<'a> {
    pub snapshot: &'a Snapshot,
    pub selection: Selection,
    pub target_user: String,
    pub target_object_id: String,
    pub in_place: bool,
    pub created_by: String,
    pub purpose: String,
}

fn components(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// Display-name components for a mail folder, walking parent ids so names
/// containing `/` are kept intact.
fn mail_components(mail: &MailSnapshot, id: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = mail.folder(id);
    let mut guard = 0;
    while let Some(f) = cur {
        out.push(f.name.clone());
        cur = f.parent_id.as_deref().and_then(|p| mail.folder(p));
        guard += 1;
        if guard > 256 {
            break;
        }
    }
    out.reverse();
    out
}

pub fn build_plan(repo: &Repository, req: PlanRequest<'_>) -> Result<RestorePlan> {
    let snap = req.snapshot;
    let plan_id = crate::time::sortable_id();
    let mode = if req.in_place {
        RestoreMode::InPlace
    } else {
        RestoreMode::Alternate {
            root: vec![RESTORE_ROOT.to_string(), plan_id.clone()],
        }
    };
    let prefix: Vec<String> = match &mode {
        RestoreMode::InPlace => vec![],
        RestoreMode::Alternate { root } => root.clone(),
    };
    let mut summary = PlanSummary::default();
    let mut actions = Vec::new();

    match snap.workload {
        Workload::Mail => {
            let mail = snap.mail.as_ref().context("snapshot has no mail data")?;
            let (folder_ids, message_ids): (BTreeSet<String>, BTreeSet<String>) =
                match &req.selection {
                    Selection::All => (
                        mail.folders.iter().map(|f| f.id.clone()).collect(),
                        mail.messages.iter().map(|m| m.id.clone()).collect(),
                    ),
                    Selection::MailMessages(ids) => {
                        for id in ids {
                            if !mail.messages.iter().any(|m| &m.id == id) {
                                bail!("message {id} is not in snapshot {}", snap.id);
                            }
                        }
                        (BTreeSet::new(), ids.iter().cloned().collect())
                    }
                    Selection::MailFolder(path) => {
                        let want = components(path);
                        let folders: BTreeSet<String> = mail
                            .folders
                            .iter()
                            .filter(|f| {
                                let c = mail_components(mail, &f.id);
                                c.len() >= want.len()
                                    && c[..want.len()]
                                        .iter()
                                        .zip(&want)
                                        .all(|(a, b)| a.eq_ignore_ascii_case(b))
                            })
                            .map(|f| f.id.clone())
                            .collect();
                        if folders.is_empty() {
                            bail!("folder {path} is not in snapshot {}", snap.id);
                        }
                        let msgs = mail
                            .messages
                            .iter()
                            .filter(|m| folders.contains(&m.folder_id))
                            .map(|m| m.id.clone())
                            .collect();
                        (folders, msgs)
                    }
                    _ => bail!("that selection does not apply to a mail snapshot"),
                };
            let mut folder_paths: BTreeSet<Vec<String>> = BTreeSet::new();
            for id in &folder_ids {
                folder_paths.insert([prefix.clone(), mail_components(mail, id)].concat());
            }
            for m in mail.messages.iter().filter(|m| message_ids.contains(&m.id)) {
                folder_paths.insert([prefix.clone(), mail_components(mail, &m.folder_id)].concat());
            }
            // Include every ancestor, then order parents before children.
            let mut all_paths: BTreeSet<Vec<String>> = BTreeSet::new();
            for p in folder_paths {
                for i in 1..=p.len() {
                    all_paths.insert(p[..i].to_vec());
                }
            }
            let mut fp: Vec<Vec<String>> = all_paths.into_iter().collect();
            fp.sort_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(b)));
            for p in fp {
                summary.mail_folders += 1;
                actions.push(Action::MailFolder { path: p });
            }
            for m in mail.messages.iter().filter(|m| message_ids.contains(&m.id)) {
                summary.messages += 1;
                summary.bytes += m.content.size;
                actions.push(Action::Message {
                    source_id: m.id.clone(),
                    folder: [prefix.clone(), mail_components(mail, &m.folder_id)].concat(),
                    internet_message_id: m.internet_message_id.clone(),
                    subject: m.subject.clone(),
                    content: m.content.clone(),
                });
            }
        }
        Workload::Drive => {
            let drive = snap
                .drive
                .as_ref()
                .context("snapshot has no OneDrive data")?;
            let selected: Vec<&crate::snapshot::DriveEntry> = match &req.selection {
                Selection::All => drive.items.iter().filter(|i| !i.path.is_empty()).collect(),
                Selection::DriveItems(ids) => {
                    let mut v = Vec::new();
                    for id in ids {
                        let e =
                            drive.items.iter().find(|i| &i.id == id).with_context(|| {
                                format!("item {id} is not in snapshot {}", snap.id)
                            })?;
                        v.extend(subtree(drive, &e.path));
                    }
                    v
                }
                Selection::DrivePath(path) => {
                    let e = drive
                        .by_path(path)
                        .with_context(|| format!("path {path} is not in snapshot {}", snap.id))?;
                    subtree(drive, &e.path)
                }
                _ => bail!("that selection does not apply to a OneDrive snapshot"),
            };
            let target = |p: &str| -> String {
                let mut c = prefix.clone();
                c.extend(components(p));
                c.join("/")
            };
            let mut folders: BTreeSet<String> = BTreeSet::new();
            for e in &selected {
                let t = target(&e.path);
                let mut c = components(&t);
                if !e.folder {
                    c.pop();
                }
                for i in 1..=c.len() {
                    folders.insert(c[..i].join("/"));
                }
            }
            let mut fl: Vec<String> = folders.into_iter().collect();
            fl.sort_by(|a, b| {
                a.matches('/')
                    .count()
                    .cmp(&b.matches('/').count())
                    .then(a.cmp(b))
            });
            for f in fl {
                summary.drive_folders += 1;
                actions.push(Action::DriveFolder { path: f });
            }
            let mut seen = BTreeSet::new();
            for e in selected.iter().filter(|e| !e.folder) {
                if !seen.insert(e.id.clone()) {
                    continue;
                }
                let content = e
                    .content
                    .clone()
                    .with_context(|| format!("{} has no stored content", e.path))?;
                summary.files += 1;
                summary.bytes += e.size;
                actions.push(Action::File {
                    source_id: e.id.clone(),
                    path: target(&e.path),
                    size: e.size,
                    quick_xor_hash: e.quick_xor_hash.clone(),
                    content,
                });
            }
        }
    }
    if actions.is_empty() {
        bail!("the selection matched nothing");
    }
    Ok(RestorePlan {
        version: PLAN_VERSION,
        plan_id,
        created: crate::time::now_rfc3339(),
        created_by: req.created_by,
        purpose: req.purpose,
        repo_id: repo.config().repo_id.clone(),
        source: PlanSource {
            principal_tag: snap.principal_tag.clone(),
            user: snap.principal.upn.clone(),
            workload: snap.workload,
            snapshot_id: snap.id.clone(),
            snapshot_completed: snap.completed.clone(),
        },
        target: PlanTarget {
            user: req.target_user,
            object_id: req.target_object_id,
        },
        mode,
        summary,
        actions,
    })
}

fn subtree<'a>(
    drive: &'a crate::snapshot::DriveSnapshot,
    path: &str,
) -> Vec<&'a crate::snapshot::DriveEntry> {
    let p = path.to_ascii_lowercase();
    drive
        .items
        .iter()
        .filter(|i| {
            let ip = i.path.to_ascii_lowercase();
            !i.path.is_empty() && (ip == p || p.is_empty() || ip.starts_with(&format!("{p}/")))
        })
        .collect()
}

/// Serialise a plan. The plan hash is the SHA-256 of exactly these bytes.
pub fn encode_plan(plan: &RestorePlan) -> Result<Vec<u8>> {
    let mut b = serde_json::to_vec_pretty(plan)?;
    b.push(b'\n');
    Ok(b)
}

pub fn plan_hash(bytes: &[u8]) -> String {
    crate::crypto::sha256_hex(bytes)
}

/// Parse plan bytes and check the approval against their hash.
pub fn verify_approval(bytes: &[u8], approval: &str) -> Result<(RestorePlan, String)> {
    let hash = plan_hash(bytes);
    let a = approval.trim().to_ascii_lowercase();
    if a.len() < MIN_APPROVAL_PREFIX || !a.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("approval must be at least {MIN_APPROVAL_PREFIX} hex characters of the plan hash");
    }
    if !hash.starts_with(&a) {
        bail!(
            "approval does not match this plan (the plan was changed, or the wrong plan was given)"
        );
    }
    let plan: RestorePlan =
        serde_json::from_slice(bytes).context("plan file is not a valid Banksia restore plan")?;
    if plan.version != PLAN_VERSION {
        bail!("unsupported plan version {}", plan.version);
    }
    Ok((plan, hash))
}
