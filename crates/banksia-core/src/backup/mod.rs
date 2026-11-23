//! Backup runs.

pub mod drive;
pub mod mail;

use crate::{
    checkpoint::Checkpoint,
    config::{UserSelection, Workload},
    repo::{ContentRef, ContentWriter, Repository, WriteStats},
    snapshot::{ItemError, Principal, Snapshot, SnapshotStats},
};
use anyhow::{Context, Result};
use banksia_graph::{GraphClient, GraphError, seg, types::User};
use futures::StreamExt;
use serde::Serialize;
use std::{path::PathBuf, time::Instant};

pub struct BackupContext {
    pub repo: Repository,
    pub graph: GraphClient,
    pub state_dir: PathBuf,
    /// Number of user and workload jobs run at once.
    pub parallel_jobs: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobResult {
    pub user: String,
    pub workload: Workload,
    pub snapshot_id: Option<String>,
    pub partial: bool,
    pub stats: Option<SnapshotStats>,
    pub error: Option<String>,
}

impl JobResult {
    pub fn ok(&self) -> bool {
        self.error.is_none() && !self.partial
    }
}

pub async fn resolve_user(graph: &GraphClient, upn_or_id: &str) -> Result<Principal, GraphError> {
    let u: User = graph
        .get_json(&format!(
            "/users/{}?$select=id,userPrincipalName,displayName",
            seg(upn_or_id)
        ))
        .await?;
    Ok(Principal {
        object_id: u.id,
        upn: u.user_principal_name,
    })
}

/// Back up every configured user and workload.
pub async fn run(ctx: &BackupContext, users: &[UserSelection]) -> Vec<JobResult> {
    let jobs: Vec<(String, Workload)> = users
        .iter()
        .flat_map(|u| u.workloads.iter().map(move |w| (u.id.clone(), *w)))
        .collect();
    futures::stream::iter(jobs)
        .map(|(user, workload)| async move {
            match backup_one(ctx, &user, workload).await {
                Ok(snap) => JobResult {
                    user,
                    workload,
                    snapshot_id: Some(snap.id.clone()),
                    partial: snap.partial,
                    stats: Some(snap.stats),
                    error: None,
                },
                Err(e) => JobResult {
                    user,
                    workload,
                    snapshot_id: None,
                    partial: false,
                    stats: None,
                    error: Some(format!("{e:#}")),
                },
            }
        })
        .buffer_unordered(ctx.parallel_jobs.max(1))
        .collect()
        .await
}

pub async fn backup_one(ctx: &BackupContext, user: &str, workload: Workload) -> Result<Snapshot> {
    let started_at = Instant::now();
    let started = crate::time::now_rfc3339();
    let principal = resolve_user(&ctx.graph, user)
        .await
        .with_context(|| format!("cannot resolve user {user}"))?;
    let tag = ctx.repo.principal_tag(&principal.object_id);
    // One backup per user and workload at a time, across processes.
    let _lock = crate::lock::Lock::acquire(&ctx.state_dir, &format!("backup-{tag}-{workload}"))
        .with_context(|| format!("another backup of {user} {workload} is running"))?;
    let parent = ctx.repo.latest_snapshot(&tag, workload).await?;
    let mut cp = Checkpoint::open(&ctx.state_dir, ctx.repo.keys(), &tag, workload.as_str())?;
    if !cp.is_empty() {
        tracing::info!(user, %workload, items = cp.len(), "resuming from checkpoint");
    }
    let mut run = Run {
        repo: &ctx.repo,
        graph: &ctx.graph,
        stats: SnapshotStats::default(),
        write: WriteStats::default(),
        errors: Vec::new(),
        cp: &mut cp,
    };
    let mut snap = Snapshot {
        version: crate::snapshot::SNAPSHOT_VERSION,
        id: crate::time::sortable_id(),
        workload,
        principal_tag: tag,
        principal: principal.clone(),
        started,
        completed: String::new(),
        parent: parent.as_ref().map(|p| p.id.clone()),
        partial: false,
        stats: SnapshotStats::default(),
        errors: Vec::new(),
        mail: None,
        drive: None,
    };
    match workload {
        Workload::Mail => {
            snap.mail = Some(
                mail::backup(
                    &mut run,
                    &principal,
                    parent.as_ref().and_then(|p| p.mail.as_ref()),
                )
                .await?,
            )
        }
        Workload::Drive => {
            snap.drive = Some(
                drive::backup(
                    &mut run,
                    &principal,
                    parent.as_ref().and_then(|p| p.drive.as_ref()),
                )
                .await?,
            )
        }
    }
    run.cp.flush()?;
    let mut stats = run.stats;
    stats.chunks_written = run.write.chunks_written;
    stats.chunks_deduplicated = run.write.chunks_deduplicated;
    stats.errors = run.errors.len() as u64;
    stats.duration_ms = started_at.elapsed().as_millis() as u64;
    snap.partial = !run.errors.is_empty();
    snap.errors = run.errors;
    snap.stats = stats;
    snap.completed = crate::time::now_rfc3339();
    ctx.repo.save_snapshot(&snap).await?;
    cp.complete()?;
    tracing::info!(user, %workload, snapshot = %snap.id, items = snap.stats.items, downloaded = snap.stats.items_downloaded, partial = snap.partial, "snapshot committed");
    Ok(snap)
}

pub(crate) struct Run<'a> {
    pub repo: &'a Repository,
    pub graph: &'a GraphClient,
    pub stats: SnapshotStats,
    pub write: WriteStats,
    pub errors: Vec<ItemError>,
    pub cp: &'a mut Checkpoint,
}

pub(crate) struct Fetched {
    pub content: ContentRef,
    pub quick_xor: String,
}

impl Run<'_> {
    pub fn error(&mut self, item_id: &str, reason: impl Into<String>) {
        let reason = reason.into();
        tracing::warn!(item = item_id, %reason, "item failed");
        self.errors.push(ItemError {
            item_id: item_id.to_string(),
            reason,
        });
    }

    /// Stream a Graph download into the repository.
    pub async fn fetch(&mut self, path: &str) -> Result<Fetched> {
        let mut dl = self.graph.download(path).await?;
        let mut w = ContentWriter::new(self.repo);
        let mut qx = banksia_quickxor::QuickXor::new();
        while let Some(chunk) = dl.chunk().await? {
            qx.update(&chunk);
            w.write(&chunk, &mut self.write).await?;
        }
        let content = w.finish(&mut self.write).await?;
        self.stats.bytes_downloaded += content.size;
        self.stats.items_downloaded += 1;
        Ok(Fetched {
            content,
            quick_xor: qx.finish_base64(),
        })
    }
}
