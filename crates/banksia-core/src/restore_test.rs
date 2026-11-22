//! Scheduled restore tests.
//!
//! For each backed-up user and workload: take the latest snapshot, check its
//! age, pick a deterministic sample, restore it to a throwaway target with the
//! normal restore engine, read every item back from the tenant, compare it
//! with the backup, and write an evidence pack.

use crate::{
    backup::resolve_user,
    config::{RestoreTestConfig, Workload},
    evidence::{Check, EvidencePack, ItemEvidence, Policy, Verdict},
    repo::Repository,
    restore::{
        self, Action, PlanRequest, Selection,
        apply::{self, ApplyContext, MAX_MIME_IMPORT, Outcome},
    },
    snapshot::Snapshot,
};
use anyhow::{Context, Result};
use banksia_graph::{GraphClient, types::Message};
use std::{path::PathBuf, time::Instant};

pub struct RestoreTestContext<'a> {
    pub repo: &'a Repository,
    /// Restore-test credential, for writing to and reading back from the targets.
    pub test_graph: &'a GraphClient,
    pub policy: &'a RestoreTestConfig,
    pub state_dir: PathBuf,
    pub operator: String,
    pub repository_label: String,
    /// Hex seed; random if not given.
    pub seed: Option<String>,
    /// Limit to these source users (UPN or object id). Empty means all backed-up users.
    pub users: Vec<String>,
    /// Users and workloads that are expected to have backups (from
    /// `backup.users`). Each one without a snapshot is a failed check.
    pub expected: Vec<crate::config::UserSelection>,
}

/// Category for an error, safe to write into evidence. Full detail goes to
/// the operator log only, because errors can carry folder and file names.
fn redact(e: &str) -> String {
    let l = e.to_ascii_lowercase();
    let cat = if l.contains("quickxorhash") {
        "content hash mismatch (quickXorHash)"
    } else if l.contains("sha-256") || l.contains("sha256") {
        "content hash mismatch (SHA-256)"
    } else if l.contains("size") {
        "size mismatch"
    } else if l.contains("attachment") {
        "attachment mismatch"
    } else if l.contains("body") {
        "message body mismatch"
    } else if l.contains("subject") {
        "subject mismatch"
    } else if l.contains("internetmessageid") {
        "Internet message id mismatch"
    } else if l.contains("not parse") || l.contains("unparseable") {
        "restored message could not be parsed"
    } else if l.contains("resolve") {
        "could not resolve the test target"
    } else if l.contains("allowlist") {
        "target not allowed"
    } else if l.contains("throttl")
        || l.contains(" 429")
        || l.contains(" 503")
        || l.contains("unavailable")
    {
        "Graph throttled or unavailable"
    } else if l.contains("authentication") || l.contains("token") {
        "authentication failed"
    } else if l.contains("not resolved") || l.contains("folder") {
        "could not create a restore folder"
    } else if l.contains("missing from the repository") || l.contains("failed authentication") {
        "repository data missing or damaged"
    } else {
        "restore or verification error (see operator log)"
    };
    cat.to_string()
}

/// Small deterministic PRNG (SplitMix64) so a recorded seed reproduces the sample.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

fn seed_for(seed: &str, tag: &str, workload: Workload) -> u64 {
    let h = crate::crypto::sha256_hex(format!("{seed}/{tag}/{workload}").as_bytes());
    u64::from_str_radix(&h[..16], 16).unwrap_or(0)
}

fn sample<T: Clone>(items: &[T], n: usize, seed: u64) -> Vec<T> {
    let mut idx: Vec<usize> = (0..items.len()).collect();
    let mut rng = Rng(seed);
    for i in (1..idx.len()).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        idx.swap(i, j);
    }
    let mut chosen: Vec<usize> = idx.into_iter().take(n).collect();
    chosen.sort_unstable();
    chosen.into_iter().map(|i| items[i].clone()).collect()
}

pub async fn run(ctx: &RestoreTestContext<'_>) -> Result<EvidencePack> {
    let started_at = Instant::now();
    let started = crate::time::now_rfc3339();
    let run_id = crate::time::sortable_id();
    let seed = ctx.seed.clone().unwrap_or_else(|| {
        let mut b = [0u8; 16];
        crate::crypto::fill_random(&mut b);
        hex::encode(b)
    });
    let mut pack = EvidencePack {
        format: crate::evidence::FORMAT.into(),
        run_id: run_id.clone(),
        banksia_version: env!("CARGO_PKG_VERSION").into(),
        started,
        completed: String::new(),
        duration_ms: 0,
        repo_id: ctx.repo.config().repo_id.clone(),
        repository: ctx.repository_label.clone(),
        operator: ctx.operator.clone(),
        host: crate::audit::host(),
        seed: seed.clone(),
        policy: Policy {
            sample_size: ctx.policy.sample_size,
            max_snapshot_age_hours: ctx.policy.max_snapshot_age_hours,
            mail_target: ctx.policy.mail_target.clone(),
            drive_target: ctx.policy.drive_target.clone(),
        },
        result: Verdict::Fail,
        summary: Default::default(),
        checks: Vec::new(),
    };
    ctx.repo
        .audit(
            "restore_test.started",
            &ctx.operator,
            None,
            serde_json::json!({"run_id": run_id, "seed": seed, "policy": pack.policy}),
        )
        .await?;

    // Which principals and workloads to test: everything configured for
    // backup, plus anything else with snapshots. A configured pair with no
    // snapshot fails rather than being silently left out.
    let mut pairs: Vec<(String, Workload)> = ctx
        .repo
        .list_snapshots(None)
        .await?
        .into_iter()
        .map(|r| (r.principal_tag, r.workload))
        .collect();
    let mut missing: Vec<(String, Workload, String)> = Vec::new();
    for u in &ctx.expected {
        let tag = ctx.repo.find_principal(&u.id).await?.map(|(t, _)| t);
        for w in &u.workloads {
            match &tag {
                Some(t) if pairs.iter().any(|(pt, pw)| pt == t && pw == w) => {}
                Some(t) => missing.push((t.clone(), *w, u.id.clone())),
                None => missing.push((ctx.repo.principal_tag(&u.id), *w, u.id.clone())),
            }
        }
    }
    if !ctx.users.is_empty() {
        let mut keep = Vec::new();
        for u in &ctx.users {
            match ctx.repo.find_principal(u).await? {
                Some((tag, _)) => keep.push(tag),
                None if ctx.expected.iter().any(|e| e.id.eq_ignore_ascii_case(u)) => {
                    keep.push(ctx.repo.principal_tag(u))
                }
                None => anyhow::bail!("--user {u} has no backups and is not in backup.users"),
            }
        }
        pairs.retain(|(t, _)| keep.contains(t));
        missing.retain(|(t, _, id)| keep.contains(t) || keep.contains(&ctx.repo.principal_tag(id)));
    }
    pairs.sort();
    pairs.dedup();
    missing.sort();
    missing.dedup();
    for (tag, workload, _) in &missing {
        pack.checks.push(Check {
            source_principal_tag: tag.clone(),
            workload: *workload,
            snapshot_id: None,
            snapshot_completed: None,
            snapshot_age_hours: None,
            snapshot_partial: None,
            snapshot_items: None,
            target: "-".into(),
            plan_id: None,
            plan_hash: None,
            restore_ms: None,
            verify_ms: None,
            result: Verdict::Fail,
            reasons: vec!["configured for backup but no snapshot exists".into()],
            items: vec![],
        });
    }

    for (tag, workload) in pairs {
        let target = match workload {
            Workload::Mail => ctx.policy.mail_target.clone(),
            Workload::Drive => ctx.policy.drive_target.clone(),
        };
        let check = match target {
            None => Check {
                source_principal_tag: tag.clone(),
                workload,
                snapshot_id: None,
                snapshot_completed: None,
                snapshot_age_hours: None,
                snapshot_partial: None,
                snapshot_items: None,
                target: "not configured".into(),
                plan_id: None,
                plan_hash: None,
                restore_ms: None,
                verify_ms: None,
                result: Verdict::Skipped,
                reasons: vec![format!(
                    "restore_test.{}_target is not configured",
                    if workload == Workload::Mail {
                        "mail"
                    } else {
                        "drive"
                    }
                )],
                items: vec![],
            },
            Some(t) => match test_one(ctx, &seed, &tag, workload, &t, &run_id).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), %workload, "restore test check failed");
                    Check {
                        source_principal_tag: tag.clone(),
                        workload,
                        snapshot_id: None,
                        snapshot_completed: None,
                        snapshot_age_hours: None,
                        snapshot_partial: None,
                        snapshot_items: None,
                        target: t,
                        plan_id: None,
                        plan_hash: None,
                        restore_ms: None,
                        verify_ms: None,
                        result: Verdict::Fail,
                        reasons: vec![redact(&format!("{e:#}"))],
                        items: vec![],
                    }
                }
            },
        };
        pack.checks.push(check);
    }

    pack.completed = crate::time::now_rfc3339();
    pack.duration_ms = started_at.elapsed().as_millis() as u64;
    pack.finalise();
    ctx.repo
        .audit(
            "restore_test.finished",
            &ctx.operator,
            None,
            serde_json::json!({"run_id": run_id, "result": pack.result, "summary": pack.summary}),
        )
        .await?;
    Ok(pack)
}

async fn test_one(
    ctx: &RestoreTestContext<'_>,
    seed: &str,
    tag: &str,
    workload: Workload,
    target: &str,
    run_id: &str,
) -> Result<Check> {
    let snap = ctx
        .repo
        .latest_snapshot(tag, workload)
        .await?
        .context("no snapshot")?;
    let mut check = Check {
        source_principal_tag: tag.to_string(),
        workload,
        snapshot_id: Some(snap.id.clone()),
        snapshot_completed: Some(snap.completed.clone()),
        snapshot_age_hours: None,
        snapshot_partial: Some(snap.partial),
        snapshot_items: Some(snap.stats.items),
        target: target.to_string(),
        plan_id: None,
        plan_hash: None,
        restore_ms: None,
        verify_ms: None,
        result: Verdict::Pass,
        reasons: vec![],
        items: vec![],
    };
    match crate::time::parse_rfc3339(&snap.completed) {
        Some(t) => {
            let age = (crate::time::now() - t).as_seconds_f64() / 3600.0;
            check.snapshot_age_hours = Some((age * 10.0).round() / 10.0);
            if age > ctx.policy.max_snapshot_age_hours as f64 {
                check.result = Verdict::Fail;
                check.reasons.push(format!(
                    "latest snapshot is {age:.1} hours old; policy allows {} hours",
                    ctx.policy.max_snapshot_age_hours
                ));
            }
        }
        None => {
            check.result = Verdict::Fail;
            check
                .reasons
                .push("snapshot completion time is unreadable".into());
        }
    }
    if snap.partial {
        // Items that failed to back up cannot be restored; a partial snapshot
        // is not evidence that the backup works.
        check.result = Verdict::Fail;
        check.reasons.push(format!(
            "latest snapshot is partial ({} item errors)",
            snap.errors.len()
        ));
    }

    let selection = match select(&snap, ctx.policy.sample_size, seed_for(seed, tag, workload)) {
        Some(s) => s,
        None => {
            check.result = Verdict::Fail;
            check
                .reasons
                .push("snapshot has no restorable items to test".into());
            return Ok(check);
        }
    };

    let who = resolve_user(ctx.test_graph, target)
        .await
        .with_context(|| format!("cannot resolve test target {target}"))?;
    let plan = restore::build_plan(
        ctx.repo,
        PlanRequest {
            snapshot: &snap,
            selection,
            target_user: target.to_string(),
            target_object_id: who.object_id.clone(),
            in_place: false,
            created_by: ctx.operator.clone(),
            purpose: format!("restore test {run_id}"),
        },
    )?;
    let bytes = restore::encode_plan(&plan)?;
    let hash = restore::plan_hash(&bytes);
    check.plan_id = Some(plan.plan_id.clone());
    check.plan_hash = Some(hash.clone());

    // Restore-test policy: pre-approved, but only into the configured targets.
    let allowed: Vec<String> = [&ctx.policy.mail_target, &ctx.policy.drive_target]
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    let actx = ApplyContext {
        repo: ctx.repo,
        graph: ctx.test_graph,
        state_dir: ctx.state_dir.clone(),
        enabled: true,
        allowed_targets: allowed,
        operator: ctx.operator.clone(),
        audit_kind: "restore_test",
    };
    let t0 = Instant::now();
    let report = apply::apply(&actx, &plan, &hash, false).await?;
    check.restore_ms = Some(t0.elapsed().as_millis() as u64);

    let t1 = Instant::now();
    for (i, a) in plan.actions.iter().enumerate() {
        let r = &report.results[i];
        let (source_id, kind) = match a {
            Action::Message { source_id, .. } => (source_id, "message"),
            Action::File { source_id, .. } => (source_id, "file"),
            _ => {
                if r.outcome == Outcome::Failed {
                    check.result = Verdict::Fail;
                    tracing::warn!(
                        detail = r.detail.as_deref().unwrap_or(""),
                        "restore test folder action failed"
                    );
                    check
                        .reasons
                        .push("could not create a restore folder".into());
                }
                continue;
            }
        };
        let item_ref = ctx
            .repo
            .keys()
            .id_hex("evidence item", source_id.as_bytes())[..16]
            .to_string();
        let mut ev = ItemEvidence {
            item_ref,
            kind: kind.into(),
            size: 0,
            sha256: String::new(),
            restore_outcome: format!("{:?}", r.outcome).to_lowercase(),
            checks_passed: vec![],
            result: Verdict::Pass,
            failure: None,
        };
        let outcome = match (a, &r.outcome, &r.tenant_id) {
            (_, Outcome::Failed, _) => Err(anyhow::anyhow!(
                "restore failed: {}",
                r.detail.clone().unwrap_or_default()
            )),
            (_, _, None) => Err(anyhow::anyhow!("restore did not return a tenant item id")),
            (
                Action::Message {
                    content,
                    subject,
                    internet_message_id,
                    ..
                },
                _,
                Some(id),
            ) => {
                ev.size = content.size;
                ev.sha256 = content.sha256.clone();
                verify_message(
                    ctx,
                    &who.object_id,
                    id,
                    content,
                    subject.as_deref(),
                    internet_message_id.as_deref(),
                )
                .await
            }
            (
                Action::File {
                    content,
                    size,
                    quick_xor_hash,
                    ..
                },
                _,
                Some(id),
            ) => {
                ev.size = *size;
                ev.sha256 = content.sha256.clone();
                verify_file(
                    ctx,
                    &who.object_id,
                    id,
                    *size,
                    quick_xor_hash.as_deref(),
                    &content.sha256,
                )
                .await
            }
            _ => unreachable!("only messages and files reach here"),
        };
        match outcome {
            Ok(passed) => ev.checks_passed = passed,
            Err(e) => {
                ev.result = Verdict::Fail;
                let full = format!("{e:#}");
                tracing::warn!(item = %ev.item_ref, error = %full, "restore test item failed");
                ev.failure = Some(redact(&full));
                check.result = Verdict::Fail;
            }
        }
        check.items.push(ev);
    }
    check.verify_ms = Some(t1.elapsed().as_millis() as u64);
    if check.items.iter().any(|i| i.result == Verdict::Fail) {
        check
            .reasons
            .push("one or more restored items failed verification".into());
    }
    Ok(check)
}

/// Choose a sample of restorable items: messages small enough for MIME import, or files.
fn select(snap: &Snapshot, n: usize, seed: u64) -> Option<Selection> {
    match snap.workload {
        Workload::Mail => {
            let ids: Vec<String> = snap
                .mail
                .as_ref()?
                .messages
                .iter()
                .filter(|m| m.content.size <= MAX_MIME_IMPORT)
                .map(|m| m.id.clone())
                .collect();
            (!ids.is_empty()).then(|| Selection::MailMessages(sample(&ids, n, seed)))
        }
        Workload::Drive => {
            let ids: Vec<String> = snap
                .drive
                .as_ref()?
                .items
                .iter()
                .filter(|i| !i.folder && i.content.is_some())
                .map(|i| i.id.clone())
                .collect();
            (!ids.is_empty()).then(|| Selection::DriveItems(sample(&ids, n, seed)))
        }
    }
}

async fn verify_file(
    ctx: &RestoreTestContext<'_>,
    user: &str,
    item_id: &str,
    size: u64,
    qx: Option<&str>,
    sha256: &str,
) -> Result<Vec<String>> {
    let drive: banksia_graph::types::Drive = ctx
        .test_graph
        .get_json(&format!("/users/{user}/drive"))
        .await?;
    let item: banksia_graph::types::DriveItem = ctx
        .test_graph
        .get_json(&format!("/drives/{}/items/{item_id}", drive.id))
        .await?;
    let mut passed = vec![];
    anyhow::ensure!(
        item.size == Some(size),
        "size in tenant {:?} differs from backup {size}",
        item.size
    );
    passed.push("size".to_string());
    if let Some(want) = qx {
        anyhow::ensure!(
            item.quick_xor_hash() == Some(want),
            "quickXorHash in tenant differs from backup"
        );
        passed.push("quick_xor_hash_metadata".to_string());
    }
    let mut dl = ctx
        .test_graph
        .download(&format!("/drives/{}/items/{item_id}/content", drive.id))
        .await?;
    let mut sha = crate::crypto::Sha256Stream::default();
    let mut q = banksia_quickxor::QuickXor::new();
    let mut n = 0u64;
    while let Some(c) = dl.chunk().await? {
        sha.update(&c);
        q.update(&c);
        n += c.len() as u64;
    }
    anyhow::ensure!(n == size, "downloaded {n} bytes, backup has {size}");
    if let Some(want) = qx {
        anyhow::ensure!(
            q.finish_base64() == want,
            "downloaded content quickXorHash differs from backup"
        );
        passed.push("quick_xor_hash_content".to_string());
    }
    anyhow::ensure!(
        sha.finish_hex() == sha256,
        "downloaded content SHA-256 differs from backup"
    );
    passed.push("sha256_content".to_string());
    Ok(passed)
}

async fn verify_message(
    ctx: &RestoreTestContext<'_>,
    user: &str,
    message_id: &str,
    content: &crate::repo::ContentRef,
    subject: Option<&str>,
    imid: Option<&str>,
) -> Result<Vec<String>> {
    let mut passed = vec![];
    let meta: Message = ctx
        .test_graph
        .get_json(&format!(
            "/users/{user}/messages/{message_id}?$select=subject,internetMessageId"
        ))
        .await?;
    if let Some(s) = subject {
        anyhow::ensure!(
            meta.subject.as_deref() == Some(s),
            "subject in tenant differs from backup"
        );
        passed.push("subject".to_string());
    }
    if let Some(want) = imid {
        anyhow::ensure!(
            meta.internet_message_id.as_deref() == Some(want),
            "internetMessageId in tenant differs from backup"
        );
        passed.push("internet_message_id".to_string());
    }
    // Exchange rewrites some headers on import, so compare attachments, not raw bytes.
    let restored = ctx
        .test_graph
        .download(&format!("/users/{user}/messages/{message_id}/$value"))
        .await?
        .bytes()
        .await?;
    let original = ctx.repo.read_content(content).await?;
    let a = content_hashes(&original).context("backed-up message could not be parsed as MIME")?;
    let b = content_hashes(&restored)?;
    anyhow::ensure!(
        a.bodies == b.bodies,
        "message body differs from backup ({} body parts in backup, {} restored)",
        a.bodies.len(),
        b.bodies.len()
    );
    passed.push(format!("body_sha256 ({})", a.bodies.len()));
    anyhow::ensure!(
        a.attachments == b.attachments,
        "attachments differ ({} in backup, {} restored)",
        a.attachments.len(),
        b.attachments.len()
    );
    passed.push(format!("attachments_sha256 ({})", a.attachments.len()));
    Ok(passed)
}

/// Hashes of a message's content parts: each text body, each HTML body and
/// each attachment. Bodies are compared after normalising line endings and
/// trailing whitespace, which Exchange may change on import.
struct ContentHashes {
    bodies: Vec<String>,
    attachments: Vec<String>,
}

fn content_hashes(mime: &[u8]) -> Result<ContentHashes> {
    let m = mail_parser::MessageParser::default()
        .parse(mime)
        .context("restored message could not be parsed as MIME")?;
    let norm = |t: &str| {
        let t = t.replace("\r\n", "\n");
        let lines: Vec<&str> = t.lines().map(str::trim_end).collect();
        crate::crypto::sha256_hex(lines.join("\n").trim_end().as_bytes())
    };
    let mut bodies: Vec<String> = Vec::new();
    for i in 0.. {
        match m.body_text(i) {
            Some(t) => bodies.push(format!("text:{}", norm(&t))),
            None => break,
        }
    }
    let mut attachments: Vec<String> = m
        .attachments()
        .map(|p| crate::crypto::sha256_hex(&p.decoded()))
        .collect();
    attachments.sort();
    Ok(ContentHashes {
        bodies,
        attachments,
    })
}

#[cfg(test)]
fn attachment_hashes(mime: &[u8]) -> Vec<String> {
    content_hashes(mime)
        .map(|c| c.attachments)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_is_deterministic_and_bounded() {
        let items: Vec<u32> = (0..50).collect();
        let a = sample(&items, 5, 42);
        assert_eq!(a, sample(&items, 5, 42));
        assert_ne!(a, sample(&items, 5, 43));
        assert_eq!(a.len(), 5);
        assert_eq!(sample(&items[..3], 5, 1).len(), 3);
    }

    #[test]
    fn attachment_hashes_ignore_header_rewrites() {
        let m1 = b"Subject: x\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nhi\r\n--b\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=a.bin\r\nContent-Transfer-Encoding: base64\r\n\r\nAAECAw==\r\n--b--\r\n";
        let mut m2 = b"X-MS-Exchange-Organization: rewritten\r\n".to_vec();
        m2.extend_from_slice(m1);
        assert_eq!(attachment_hashes(m1).len(), 1);
        assert_eq!(attachment_hashes(m1), attachment_hashes(&m2));
        let a = content_hashes(m1).unwrap();
        let b = content_hashes(&m2).unwrap();
        assert_eq!(a.bodies, b.bodies);
        let changed = String::from_utf8_lossy(m1).replace("\r\nhi\r\n", "\r\nbye\r\n");
        assert_ne!(content_hashes(changed.as_bytes()).unwrap().bodies, a.bodies);
    }

    #[test]
    fn redaction_drops_names() {
        let r =
            redact("parent folder of Banksia Restore/x/Inbox/HR Layoffs/Alice was not resolved");
        assert!(!r.contains("HR") && !r.contains("Alice"), "{r}");
        assert_eq!(
            redact("uploaded quickXorHash does not match the backup"),
            "content hash mismatch (quickXorHash)"
        );
    }
}
