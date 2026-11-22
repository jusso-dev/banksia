//! Restore-test evidence packs.
//!
//! A pack is a directory with `evidence.json`, `report.md`, `manifest.json`
//! (SHA-256 of each file) and `manifest.hmac` (HMAC-SHA256 of the manifest
//! under the repository's evidence key). A copy is stored, encrypted and
//! write once, in the repository.

use crate::{crypto::Keys, repo::Repository};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt::Write as _, path::Path};

pub const FORMAT: &str = "banksia-evidence-v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvidencePack {
    pub format: String,
    pub run_id: String,
    pub banksia_version: String,
    pub started: String,
    pub completed: String,
    pub duration_ms: u64,
    pub repo_id: String,
    pub repository: String,
    pub operator: String,
    pub host: String,
    /// Hex seed for sample selection. Rerun with `--seed` to select the same items.
    pub seed: String,
    pub policy: Policy,
    pub result: Verdict,
    pub summary: Summary,
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Policy {
    pub sample_size: usize,
    pub max_snapshot_age_hours: u64,
    pub mail_target: Option<String>,
    pub drive_target: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
    Skipped,
}

impl Verdict {
    pub fn word(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Skipped => "SKIPPED",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Summary {
    pub checks: u64,
    pub checks_passed: u64,
    pub checks_failed: u64,
    pub checks_skipped: u64,
    pub items_tested: u64,
    pub items_verified: u64,
    pub items_failed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Check {
    /// Keyed tag of the backed-up user. Not the user's name.
    pub source_principal_tag: String,
    pub workload: crate::config::Workload,
    pub snapshot_id: Option<String>,
    pub snapshot_completed: Option<String>,
    pub snapshot_age_hours: Option<f64>,
    pub snapshot_partial: Option<bool>,
    pub snapshot_items: Option<u64>,
    pub target: String,
    pub plan_id: Option<String>,
    pub plan_hash: Option<String>,
    pub restore_ms: Option<u64>,
    pub verify_ms: Option<u64>,
    pub result: Verdict,
    pub reasons: Vec<String>,
    pub items: Vec<ItemEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ItemEvidence {
    /// Keyed reference to the source item. Not the Graph id.
    pub item_ref: String,
    pub kind: String,
    pub size: u64,
    pub sha256: String,
    pub restore_outcome: String,
    pub checks_passed: Vec<String>,
    pub result: Verdict,
    pub failure: Option<String>,
}

impl EvidencePack {
    pub fn finalise(&mut self) {
        let mut s = Summary::default();
        for c in &self.checks {
            s.checks += 1;
            match c.result {
                Verdict::Pass => s.checks_passed += 1,
                Verdict::Fail => s.checks_failed += 1,
                Verdict::Skipped => s.checks_skipped += 1,
            }
            for i in &c.items {
                s.items_tested += 1;
                match i.result {
                    Verdict::Pass => s.items_verified += 1,
                    _ => s.items_failed += 1,
                }
            }
        }
        self.result = if s.checks_failed > 0 || s.items_tested == 0 {
            Verdict::Fail
        } else {
            Verdict::Pass
        };
        self.summary = s;
    }

    pub fn report_markdown(&self) -> String {
        let mut m = String::new();
        let _ = writeln!(m, "# Banksia restore test evidence\n");
        let _ = writeln!(m, "| Field | Value |\n| --- | --- |");
        let rows = [
            ("Result", format!("**{}**", self.result.word())),
            ("Run id", self.run_id.clone()),
            ("Started (UTC)", self.started.clone()),
            ("Completed (UTC)", self.completed.clone()),
            ("Duration", format!("{} ms", self.duration_ms)),
            (
                "Repository",
                format!("{} ({})", self.repo_id, self.repository),
            ),
            ("Operator", format!("{} on {}", self.operator, self.host)),
            ("Banksia version", self.banksia_version.clone()),
            ("Sample seed", format!("`{}`", self.seed)),
            ("Sample size per check", self.policy.sample_size.to_string()),
            (
                "Maximum snapshot age",
                format!("{} hours", self.policy.max_snapshot_age_hours),
            ),
            (
                "Mail test target",
                self.policy
                    .mail_target
                    .clone()
                    .unwrap_or_else(|| "not configured".into()),
            ),
            (
                "OneDrive test target",
                self.policy
                    .drive_target
                    .clone()
                    .unwrap_or_else(|| "not configured".into()),
            ),
        ];
        for (k, v) in rows {
            let _ = writeln!(m, "| {k} | {v} |");
        }
        let s = &self.summary;
        let _ = writeln!(
            m,
            "\n## Summary\n\n{} checks: {} passed, {} failed, {} skipped. {} items restored and read back: {} verified, {} failed.\n",
            s.checks,
            s.checks_passed,
            s.checks_failed,
            s.checks_skipped,
            s.items_tested,
            s.items_verified,
            s.items_failed
        );
        let _ = writeln!(m, "## Checks\n");
        let _ = writeln!(
            m,
            "| Result | Source (tag) | Workload | Snapshot | Snapshot age (h) | Items | Restore (ms) | Verify (ms) | Reasons |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- |"
        );
        for c in &self.checks {
            let _ = writeln!(
                m,
                "| {} | `{}` | {} | {} | {} | {}/{} | {} | {} | {} |",
                c.result.word(),
                &c.source_principal_tag[..12.min(c.source_principal_tag.len())],
                c.workload,
                c.snapshot_id.as_deref().unwrap_or("-"),
                c.snapshot_age_hours
                    .map(|a| format!("{a:.1}"))
                    .unwrap_or_else(|| "-".into()),
                c.items.iter().filter(|i| i.result == Verdict::Pass).count(),
                c.items.len(),
                c.restore_ms
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".into()),
                c.verify_ms
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".into()),
                if c.reasons.is_empty() {
                    "-".into()
                } else {
                    c.reasons.join("; ").replace('|', "/")
                },
            );
        }
        let failures: Vec<(&Check, &ItemEvidence)> = self
            .checks
            .iter()
            .flat_map(|c| {
                c.items
                    .iter()
                    .filter(|i| i.result != Verdict::Pass)
                    .map(move |i| (c, i))
            })
            .collect();
        if !failures.is_empty() {
            let _ = writeln!(m, "\n## Item failures\n");
            for (c, i) in failures {
                let _ = writeln!(
                    m,
                    "- {} `{}` ({}): {}",
                    c.workload,
                    i.item_ref,
                    i.kind,
                    i.failure.as_deref().unwrap_or("unknown").replace('\n', " ")
                );
            }
        }
        let _ = writeln!(
            m,
            "\n## Method\n\n\
For each backed-up user and workload, Banksia loaded the latest snapshot, checked its age against the policy, \
selected a deterministic sample using the seed above, and restored the sample into the test target through the \
same plan and apply engine used for production restores. Every restored item was then read back from Microsoft 365 \
and compared with the backup: files by size, quickXorHash and SHA-256 of the downloaded content; messages by \
subject, Internet message id, SHA-256 of each text body and SHA-256 of each attachment. A partial or missing \
snapshot fails its check. Message bodies, file contents and item names are not included in this pack.\n\n\
## Control guidance\n\n\
This pack is evidence that restoration from backup was tested. It supports the Essential Eight *Regular backups* \
strategy, which expects restoration of data from backups to be tested, and the corresponding backup and restoration \
controls in the Information Security Manual. It does not by itself demonstrate a maturity level. Assessors should \
also review backup scope, retention, immutability and access controls (see `docs/security.md`).\n\n\
## Integrity\n\n\
`manifest.json` lists the SHA-256 of each file in this pack. `manifest.hmac` is an HMAC-SHA256 of `manifest.json` \
under a key derived from the repository master key. Check it with `banksia evidence verify <directory>`. An \
encrypted, write-once copy of this pack is stored in the repository under `evidence/{}/`.",
            self.run_id
        );
        m
    }
}

/// Files of a pack, ready to write.
pub fn render(pack: &EvidencePack, keys: &Keys) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    let mut json = serde_json::to_vec_pretty(pack)?;
    json.push(b'\n');
    files.insert("evidence.json".to_string(), json);
    files.insert("report.md".to_string(), pack.report_markdown().into_bytes());
    let manifest = serde_json::json!({
        "format": FORMAT,
        "run_id": pack.run_id,
        "files": files.iter().map(|(k, v)| (k.clone(), crate::crypto::sha256_hex(v))).collect::<BTreeMap<_, _>>(),
    });
    let mut mbytes = serde_json::to_vec_pretty(&manifest)?;
    mbytes.push(b'\n');
    let mac = keys.evidence_mac(&mbytes);
    files.insert("manifest.json".to_string(), mbytes);
    files.insert("manifest.hmac".to_string(), format!("{mac}\n").into_bytes());
    Ok(files)
}

pub async fn write(
    pack: &EvidencePack,
    repo: &Repository,
    dir: &Path,
) -> Result<std::path::PathBuf> {
    let files = render(pack, repo.keys())?;
    let out = dir.join(&pack.run_id);
    std::fs::create_dir_all(&out).with_context(|| format!("cannot create {}", out.display()))?;
    for (name, data) in &files {
        std::fs::write(out.join(name), data)?;
        let path = format!("evidence/{}/{name}", pack.run_id);
        let sealed = repo.keys().seal(&path, data)?;
        repo.store().put_create(&path, sealed).await?;
    }
    Ok(out)
}

/// Check a pack directory: every file hash and the manifest HMAC.
pub fn verify(dir: &Path, keys: &Keys) -> Result<EvidencePack> {
    let mbytes = std::fs::read(dir.join("manifest.json")).context("manifest.json missing")?;
    let mac =
        std::fs::read_to_string(dir.join("manifest.hmac")).context("manifest.hmac missing")?;
    keys.verify_evidence_mac(&mbytes, &mac)?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&mbytes).context("manifest.json invalid")?;
    if manifest["format"] != FORMAT {
        bail!("unknown evidence format");
    }
    let files = manifest["files"]
        .as_object()
        .context("manifest has no file list")?;
    for (name, want) in files {
        if name.contains('/') || name.contains("..") {
            bail!("manifest lists an invalid file name");
        }
        let data = std::fs::read(dir.join(name)).with_context(|| format!("{name} missing"))?;
        if Some(crate::crypto::sha256_hex(&data).as_str()) != want.as_str() {
            bail!("{name} does not match the manifest");
        }
    }
    let pack: EvidencePack = serde_json::from_slice(&std::fs::read(dir.join("evidence.json"))?)?;
    if Some(pack.run_id.as_str()) != manifest["run_id"].as_str() {
        bail!("evidence.json run id does not match the manifest");
    }
    Ok(pack)
}
