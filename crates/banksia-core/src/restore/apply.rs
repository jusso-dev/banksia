//! Execute a restore plan against the tenant.

use super::{Action, RestorePlan, journal::Journal};
use crate::{backup::resolve_user, repo::Repository};
use anyhow::{Context, Result, bail};
use banksia_graph::{
    GraphClient, drive_path, odata_str, seg,
    types::{Drive, DriveItem, MailFolder, Message, UploadSession},
};
use base64::Engine;
use serde::Serialize;
use serde_json::json;
use std::{collections::HashMap, path::PathBuf, time::Instant};

/// Graph limits a MIME message create request to about 4 MB after base64.
pub const MAX_MIME_IMPORT: u64 = 3 * 1024 * 1024;

pub struct ApplyContext<'a> {
    pub repo: &'a Repository,
    /// Built from the restore (or restore-test) app registration.
    pub graph: &'a GraphClient,
    pub state_dir: PathBuf,
    /// Restores are refused unless enabled.
    pub enabled: bool,
    /// UPNs or object ids that this apply may write into.
    pub allowed_targets: Vec<String>,
    pub operator: String,
    /// Label for audit events, for example `restore` or `restore_test`.
    pub audit_kind: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Created,
    /// The file existed with different content, so it was restored under a new name.
    CreatedRenamed,
    AlreadyPresent,
    WouldCreate,
    WouldCreateRenamed,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionResult {
    pub index: usize,
    pub action: String,
    pub outcome: Outcome,
    /// Id of the folder, message or drive item in the tenant.
    pub tenant_id: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    pub plan_id: String,
    pub plan_hash: String,
    pub dry_run: bool,
    pub target: String,
    pub started: String,
    pub completed: String,
    pub duration_ms: u64,
    pub created: u64,
    pub already_present: u64,
    pub failed: u64,
    pub results: Vec<ActionResult>,
}

impl ApplyReport {
    pub fn ok(&self) -> bool {
        self.failed == 0
    }
}

/// Policy checks shared by dry run and apply. No tenant writes.
pub async fn check(ctx: &ApplyContext<'_>, plan: &RestorePlan) -> Result<()> {
    if plan.repo_id != ctx.repo.config().repo_id {
        bail!("plan was made for a different repository");
    }
    if !super::valid_plan_id(&plan.plan_id) {
        bail!("plan id is not valid");
    }
    // The plan must be exactly what the planner would build from its source
    // snapshot, so what was approved is what runs.
    let snap = ctx
        .repo
        .load_snapshot(
            &plan.source.principal_tag,
            plan.source.workload,
            &plan.source.snapshot_id,
        )
        .await
        .context("cannot load the plan's source snapshot")?;
    super::validate_against_snapshot(plan, &snap)?;
    let t = &plan.target;
    let allowed = ctx
        .allowed_targets
        .iter()
        .any(|a| a.eq_ignore_ascii_case(&t.user) || a.eq_ignore_ascii_case(&t.object_id));
    if !allowed {
        bail!("target {} is not on the restore allowlist", t.user);
    }
    // Resolve the name the operator gave, so a UPN reassigned to another
    // account since planning is caught.
    let now = resolve_user(ctx.graph, &t.user)
        .await
        .with_context(|| format!("cannot resolve restore target {}", t.user))?;
    if !now.object_id.eq_ignore_ascii_case(&t.object_id) {
        bail!(
            "restore target {} no longer resolves to the object id in the plan",
            t.user
        );
    }
    // Every blob the plan needs must be in the repository.
    for a in &plan.actions {
        let content = match a {
            Action::Message { content, .. } | Action::File { content, .. } => content,
            _ => continue,
        };
        for id in &content.chunks {
            let path = format!("blobs/{}/{}", &id[..2.min(id.len())], id);
            if !ctx.repo.store().exists(&path).await? {
                bail!("blob {id} needed by this plan is missing from the repository");
            }
        }
    }
    Ok(())
}

pub async fn apply(
    ctx: &ApplyContext<'_>,
    plan: &RestorePlan,
    plan_hash: &str,
    dry_run: bool,
) -> Result<ApplyReport> {
    if !dry_run && !ctx.enabled {
        bail!("restores are disabled (set restore.enabled = true)");
    }
    check(ctx, plan).await?;
    let started_at = Instant::now();
    let started = crate::time::now_rfc3339();
    let pref = Some((plan.plan_id.as_str(), plan_hash));
    let kind = |suffix: &str| format!("{}.{suffix}", ctx.audit_kind);
    ctx.repo
        .audit(
            &kind(if dry_run { "dry_run" } else { "apply_started" }),
            &ctx.operator,
            pref,
            json!({
                "target": plan.target.user,
                "target_object_id": plan.target.object_id,
                "source_principal_tag": plan.source.principal_tag,
                "snapshot": plan.source.snapshot_id,
                "workload": plan.source.workload,
                "summary": plan.summary,
            }),
        )
        .await?;

    let _lock = crate::lock::Lock::acquire(&ctx.state_dir, &format!("restore-{}", plan.plan_id))
        .context("this plan is already being applied by another process")?;
    let mut journal = Journal::open(&ctx.state_dir, ctx.repo.keys(), &plan.plan_id, plan_hash)?;
    let mut ex = Executor {
        ctx,
        user: plan.target.object_id.clone(),
        dry_run,
        mail_folders: HashMap::new(),
        drive: None,
        drive_folders: HashMap::new(),
    };
    let mut results = Vec::with_capacity(plan.actions.len());
    for (i, a) in plan.actions.iter().enumerate() {
        if let Some(done) = journal.done.get(&i) {
            ex.remember(a, done.tenant_id.clone());
            results.push(ActionResult {
                index: i,
                action: a.describe(),
                outcome: Outcome::AlreadyPresent,
                tenant_id: done.tenant_id.clone(),
                detail: Some("completed in an earlier run".into()),
            });
            continue;
        }
        let r = match ex.run(a).await {
            Ok((outcome, tenant_id, detail)) => {
                if !dry_run {
                    journal.record(i, tenant_id.clone())?;
                }
                ActionResult {
                    index: i,
                    action: a.describe(),
                    outcome,
                    tenant_id,
                    detail,
                }
            }
            Err(e) => {
                tracing::warn!(index = i, error = %format!("{e:#}"), "restore action failed");
                ActionResult {
                    index: i,
                    action: a.describe(),
                    outcome: Outcome::Failed,
                    tenant_id: None,
                    detail: Some(format!("{e:#}")),
                }
            }
        };
        results.push(r);
    }
    let count =
        |f: &dyn Fn(&Outcome) -> bool| results.iter().filter(|r| f(&r.outcome)).count() as u64;
    let report = ApplyReport {
        plan_id: plan.plan_id.clone(),
        plan_hash: plan_hash.to_string(),
        dry_run,
        target: plan.target.user.clone(),
        started,
        completed: crate::time::now_rfc3339(),
        duration_ms: started_at.elapsed().as_millis() as u64,
        created: count(&|o| {
            matches!(
                o,
                Outcome::Created
                    | Outcome::CreatedRenamed
                    | Outcome::WouldCreate
                    | Outcome::WouldCreateRenamed
            )
        }),
        already_present: count(&|o| *o == Outcome::AlreadyPresent),
        failed: count(&|o| *o == Outcome::Failed),
        results,
    };
    if !dry_run {
        ctx.repo
            .audit(
                &kind("apply_finished"),
                &ctx.operator,
                pref,
                json!({
                    "target": plan.target.user,
                    "created": report.created,
                    "already_present": report.already_present,
                    "failed": report.failed,
                    "duration_ms": report.duration_ms,
                    "failures": report.results.iter().filter(|r| r.outcome == Outcome::Failed)
                        .map(|r| json!({"index": r.index, "detail": r.detail})).collect::<Vec<_>>(),
                }),
            )
            .await?;
    }
    Ok(report)
}

type Step = (Outcome, Option<String>, Option<String>);

/// Whether `candidate` looks like OneDrive's conflict rename of `original`:
/// `name 1.ext` or `name (1).ext`.
fn is_renamed_copy(original: &str, candidate: &str) -> bool {
    let (stem, ext) = match original.rfind('.') {
        Some(i) if i > 0 => (&original[..i], &original[i..]),
        _ => (original, ""),
    };
    let lc = candidate.to_lowercase();
    let (stem, ext) = (stem.to_lowercase(), ext.to_lowercase());
    let Some(mid) = lc.strip_prefix(&stem).and_then(|r| r.strip_suffix(&ext)) else {
        return false;
    };
    let n = mid.strip_prefix(' ').unwrap_or("");
    let n = n
        .strip_prefix('(')
        .and_then(|x| x.strip_suffix(')'))
        .unwrap_or(n);
    !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
}

struct Executor<'a, 'b> {
    ctx: &'a ApplyContext<'b>,
    user: String,
    dry_run: bool,
    /// Mail folder path -> id. `None` means "does not exist yet" in a dry run.
    mail_folders: HashMap<Vec<String>, Option<String>>,
    drive: Option<(String, String)>,
    drive_folders: HashMap<String, Option<String>>,
}

fn lower(path: &[String]) -> Vec<String> {
    path.iter().map(|p| p.to_lowercase()).collect()
}

impl Executor<'_, '_> {
    fn g(&self) -> &GraphClient {
        self.ctx.graph
    }

    fn remember(&mut self, a: &Action, id: Option<String>) {
        match a {
            Action::MailFolder { path } => {
                self.mail_folders.insert(lower(path), id);
            }
            Action::DriveFolder { path } => {
                self.drive_folders.insert(path.to_lowercase(), id);
            }
            _ => {}
        }
    }

    async fn run(&mut self, a: &Action) -> Result<Step> {
        match a {
            Action::MailFolder { path } => self.mail_folder(path).await,
            Action::Message {
                folder,
                internet_message_id,
                content,
                ..
            } => {
                self.message(folder, internet_message_id.as_deref(), content)
                    .await
            }
            Action::DriveFolder { path } => self.drive_folder(path).await,
            Action::File {
                path,
                size,
                quick_xor_hash,
                content,
                ..
            } => {
                self.file(path, *size, quick_xor_hash.as_deref(), content)
                    .await
            }
        }
    }

    // ---------------------------------------------------------- mail --

    async fn find_child_folder(&self, parent: Option<&str>, name: &str) -> Result<Option<String>> {
        let url = match parent {
            None => format!("/users/{}/mailFolders?includeHiddenFolders=true", self.user),
            Some(p) => format!(
                "/users/{}/mailFolders/{p}/childFolders?includeHiddenFolders=true",
                self.user
            ),
        };
        let all: Vec<MailFolder> = self.g().get_all(&url).await?;
        Ok(all
            .into_iter()
            .find(|f| f.display_name.eq_ignore_ascii_case(name))
            .map(|f| f.id))
    }

    async fn mail_folder(&mut self, path: &[String]) -> Result<Step> {
        let key = lower(path);
        let (parent_known, parent_id) = if path.len() == 1 {
            (true, None)
        } else {
            match self.mail_folders.get(&key[..key.len() - 1]) {
                Some(Some(id)) => (true, Some(id.clone())),
                Some(None) => (false, None),
                None => bail!("parent folder of {} was not resolved", path.join("/")),
            }
        };
        let name = path.last().expect("non-empty folder path");
        if !parent_known {
            // Parent does not exist yet (dry run only).
            self.mail_folders.insert(key, None);
            return Ok((Outcome::WouldCreate, None, None));
        }
        if let Some(id) = self.find_child_folder(parent_id.as_deref(), name).await? {
            self.mail_folders.insert(key, Some(id.clone()));
            return Ok((Outcome::AlreadyPresent, Some(id), None));
        }
        if self.dry_run {
            self.mail_folders.insert(key, None);
            return Ok((Outcome::WouldCreate, None, None));
        }
        let url = match &parent_id {
            None => format!("/users/{}/mailFolders", self.user),
            Some(p) => format!("/users/{}/mailFolders/{p}/childFolders", self.user),
        };
        let created: Result<MailFolder, _> = self
            .g()
            .post_json(&url, &json!({ "displayName": name }))
            .await;
        let id = match created {
            Ok(f) => f.id,
            Err(e) if e.is_conflict() => self
                .find_child_folder(parent_id.as_deref(), name)
                .await?
                .context("folder reported as existing but not found")?,
            Err(e) => return Err(e.into()),
        };
        self.mail_folders.insert(key, Some(id.clone()));
        Ok((Outcome::Created, Some(id), None))
    }

    async fn message(
        &mut self,
        folder: &[String],
        imid: Option<&str>,
        content: &crate::repo::ContentRef,
    ) -> Result<Step> {
        if content.size > MAX_MIME_IMPORT {
            bail!(
                "message is {} bytes; MIME import through Graph v1.0 is limited to about {} bytes (see issue #10)",
                content.size,
                MAX_MIME_IMPORT
            );
        }
        let folder_id = match self.mail_folders.get(&lower(folder)) {
            Some(Some(id)) => id.clone(),
            Some(None) => return Ok((Outcome::WouldCreate, None, None)),
            None => bail!("folder {} was not resolved", folder.join("/")),
        };
        if let Some(imid) = imid {
            let url = format!(
                "/users/{}/mailFolders/{folder_id}/messages?$filter={}&$select=id",
                self.user,
                seg(&format!("internetMessageId eq {}", odata_str(imid)))
            );
            let found: Vec<Message> = self.g().get_all(&url).await?;
            if let Some(m) = found.into_iter().next() {
                return Ok((
                    Outcome::AlreadyPresent,
                    Some(m.id),
                    Some("matched by internetMessageId".into()),
                ));
            }
        }
        let detail = imid
            .is_none()
            .then(|| "no internetMessageId; idempotency relies on the journal".to_string());
        if self.dry_run {
            return Ok((Outcome::WouldCreate, None, detail));
        }
        let mime = self.ctx.repo.read_content(content).await?;
        let body = base64::engine::general_purpose::STANDARD.encode(&mime);
        let m: Message = self
            .g()
            .post_bytes(
                &format!("/users/{}/mailFolders/{folder_id}/messages", self.user),
                body.into_bytes().into(),
                "text/plain",
            )
            .await?;
        Ok((Outcome::Created, Some(m.id), detail))
    }

    // --------------------------------------------------------- drive --

    async fn drive_ids(&mut self) -> Result<(String, String)> {
        if let Some(d) = &self.drive {
            return Ok(d.clone());
        }
        let d: Drive = self
            .g()
            .get_json(&format!("/users/{}/drive", self.user))
            .await?;
        let root: DriveItem = self.g().get_json(&format!("/drives/{}/root", d.id)).await?;
        self.drive = Some((d.id.clone(), root.id.clone()));
        Ok((d.id, root.id))
    }

    async fn get_by_path(&mut self, path: &str) -> Result<Option<DriveItem>> {
        let (drive, _) = self.drive_ids().await?;
        match self
            .g()
            .get_json::<DriveItem>(&format!("/drives/{drive}/root:/{}", drive_path(path)))
            .await
        {
            Ok(i) => Ok(Some(i)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn parent_of(&self, path: &str) -> Result<Option<Option<String>>> {
        match path.rfind('/') {
            None => Ok(Some(self.drive.as_ref().map(|d| d.1.clone()))),
            Some(i) => match self.drive_folders.get(&path[..i].to_lowercase()) {
                Some(v) => Ok(Some(v.clone())),
                None => bail!("parent folder of {path} was not resolved"),
            },
        }
    }

    async fn drive_folder(&mut self, path: &str) -> Result<Step> {
        let (drive, _) = self.drive_ids().await?;
        let key = path.to_lowercase();
        let parent = self.parent_of(path)?.flatten();
        let Some(parent) = parent else {
            self.drive_folders.insert(key, None);
            return Ok((Outcome::WouldCreate, None, None));
        };
        if let Some(i) = self.get_by_path(path).await? {
            if i.folder.is_none() {
                bail!("{path} exists in the target and is not a folder");
            }
            self.drive_folders.insert(key, Some(i.id.clone()));
            return Ok((Outcome::AlreadyPresent, Some(i.id), None));
        }
        if self.dry_run {
            self.drive_folders.insert(key, None);
            return Ok((Outcome::WouldCreate, None, None));
        }
        let name = path.rsplit('/').next().expect("name");
        let body =
            json!({ "name": name, "folder": {}, "@microsoft.graph.conflictBehavior": "fail" });
        let id = match self
            .g()
            .post_json::<DriveItem>(&format!("/drives/{drive}/items/{parent}/children"), &body)
            .await
        {
            Ok(i) => i.id,
            Err(e) if e.is_conflict() => {
                let i = self.get_by_path(path).await?.context("folder vanished")?;
                if i.folder.is_none() {
                    bail!("{path} exists in the target and is not a folder");
                }
                i.id
            }
            Err(e) => return Err(e.into()),
        };
        self.drive_folders.insert(key, Some(id.clone()));
        Ok((Outcome::Created, Some(id), None))
    }

    async fn file(
        &mut self,
        path: &str,
        size: u64,
        qx: Option<&str>,
        content: &crate::repo::ContentRef,
    ) -> Result<Step> {
        let (drive, _) = self.drive_ids().await?;
        let parent = self.parent_of(path)?.flatten();
        let Some(parent_id) = parent else {
            return Ok((Outcome::WouldCreate, None, None));
        };
        let matches = |i: &DriveItem| {
            i.file.is_some() && i.size == Some(size) && qx.is_some() && i.quick_xor_hash() == qx
        };
        let existing = self.get_by_path(path).await?;
        let conflict = match &existing {
            None => "fail",
            Some(i) if matches(i) => {
                return Ok((
                    Outcome::AlreadyPresent,
                    Some(i.id.clone()),
                    Some("same size and quickXorHash".into()),
                ));
            }
            Some(_) => {
                // Different content at the path. Never overwrite: restore beside
                // it, unless an earlier run already did.
                let siblings: Vec<DriveItem> = self
                    .g()
                    .get_all(&format!("/drives/{drive}/items/{parent_id}/children"))
                    .await?;
                let name = path.rsplit('/').next().unwrap_or(path);
                if let Some(s) = siblings.iter().find(|s| {
                    matches(s) && s.name.as_deref().is_some_and(|n| is_renamed_copy(name, n))
                }) {
                    return Ok((
                        Outcome::AlreadyPresent,
                        Some(s.id.clone()),
                        Some(format!(
                            "restored earlier as {}",
                            s.name.as_deref().unwrap_or("?")
                        )),
                    ));
                }
                "rename"
            }
        };
        if self.dry_run {
            return Ok(if conflict == "rename" {
                (
                    Outcome::WouldCreateRenamed,
                    None,
                    Some("a different file exists at this path".into()),
                )
            } else {
                (Outcome::WouldCreate, None, None)
            });
        }
        let item = self.upload(&drive, path, size, content, conflict).await?;
        if item.size != Some(size) {
            bail!(
                "uploaded size {:?} does not match the backup ({size})",
                item.size
            );
        }
        if let (Some(want), Some(got)) = (qx, item.quick_xor_hash())
            && want != got
        {
            bail!("uploaded quickXorHash does not match the backup");
        }
        let outcome = if conflict == "rename" {
            Outcome::CreatedRenamed
        } else {
            Outcome::Created
        };
        let detail = (conflict == "rename")
            .then(|| format!("restored as {}", item.name.as_deref().unwrap_or("?")));
        Ok((outcome, Some(item.id), detail))
    }

    async fn upload(
        &mut self,
        drive: &str,
        path: &str,
        size: u64,
        content: &crate::repo::ContentRef,
        conflict: &str,
    ) -> Result<DriveItem> {
        // Check the stored content end to end before anything reaches the
        // tenant: a file is never committed and then found to be wrong.
        let mut sha = crate::crypto::Sha256Stream::default();
        let mut total = 0u64;
        for id in &content.chunks {
            let chunk = self.ctx.repo.get_chunk(id).await?;
            sha.update(&chunk);
            total += chunk.len() as u64;
        }
        if total != size || content.size != size || sha.finish_hex() != content.sha256 {
            bail!("backup content does not match its recorded size or hash");
        }
        if size == 0 {
            // Upload sessions need at least one byte. Create the empty file as
            // a child with an explicit conflict behaviour.
            let parent = self
                .parent_of(path)?
                .flatten()
                .context("parent folder not resolved")?;
            let name = path.rsplit('/').next().expect("name");
            let body =
                json!({ "name": name, "file": {}, "@microsoft.graph.conflictBehavior": conflict });
            return Ok(self
                .g()
                .post_json(&format!("/drives/{drive}/items/{parent}/children"), &body)
                .await?);
        }
        // Every non-empty file goes through an upload session. The simple
        // PUT /content endpoint does not reliably honour conflict behaviour
        // and can overwrite; the session body does.
        let sess: UploadSession = self
            .g()
            .post_json(
                &format!(
                    "/drives/{drive}/root:/{}:/createUploadSession",
                    drive_path(path)
                ),
                &json!({ "item": { "@microsoft.graph.conflictBehavior": conflict } }),
            )
            .await?;
        // Repository chunks are 16 x 320 KiB, a valid fragment size.
        let mut offset = 0u64;
        let mut done = None;
        for id in &content.chunks {
            let chunk = self.ctx.repo.get_chunk(id).await?;
            let len = chunk.len() as u64;
            if len == 0 {
                continue;
            }
            done = self
                .g()
                .upload_fragment(&sess.upload_url, chunk.into(), offset, size)
                .await?;
            offset += len;
        }
        let v = done.context("upload session did not complete")?;
        Ok(serde_json::from_value(v)?)
    }
}
