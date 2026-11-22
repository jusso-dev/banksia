//! `banksia`: self-hosted Microsoft 365 backup with in-tenant restore and
//! restore-test evidence.

use anyhow::{Context, Result, bail};
use banksia_core::{
    backup,
    config::{Config, Workload},
    crypto::MasterKey,
    repo::Repository,
    store::Store,
};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "banksia",
    version,
    about = "Microsoft 365 backup with in-tenant restore and restore tests"
)]
struct Cli {
    /// Path to banksia.toml.
    #[arg(
        long,
        short,
        global = true,
        env = "BANKSIA_CONFIG",
        default_value = "banksia.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Master key management.
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Repository management.
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Back up mailboxes and OneDrive.
    Backup {
        #[command(subcommand)]
        command: BackupCommand,
    },
    /// List and inspect snapshots.
    Snapshots {
        #[command(subcommand)]
        command: SnapshotCommand,
    },
    /// Restore items into the tenant: plan, then apply with approval.
    Restore {
        #[command(subcommand)]
        command: RestoreCommand,
    },
    /// Show audit events from the repository.
    Audit {
        #[command(subcommand)]
        command: AuditCommand,
    },
}

#[derive(Subcommand)]
enum AuditCommand {
    /// List audit events, oldest first.
    List {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum RestoreCommand {
    /// Build a restore plan from a snapshot. Reads the repository only and
    /// writes a plan file. This is the dry run of what would be restored.
    Plan {
        /// Source user (UPN or object id) whose backup to restore from.
        #[arg(long)]
        user: String,
        #[arg(long, value_parser = parse_workload)]
        workload: Workload,
        /// Snapshot id, or `latest`.
        #[arg(long, default_value = "latest")]
        snapshot: String,
        /// Mail: message id to restore. Repeatable.
        #[arg(long)]
        message: Vec<String>,
        /// Mail: folder path to restore, for example `Inbox/Projects`.
        #[arg(long)]
        folder: Option<String>,
        /// OneDrive: item id to restore. Repeatable.
        #[arg(long)]
        item: Vec<String>,
        /// OneDrive: file or folder path to restore.
        #[arg(long)]
        path: Option<String>,
        /// Restore everything in the snapshot.
        #[arg(long)]
        all: bool,
        /// Target user. Defaults to the source user.
        #[arg(long)]
        target: Option<String>,
        /// Recreate the original folder structure instead of restoring under
        /// `Banksia Restore/<plan id>`.
        #[arg(long)]
        in_place: bool,
        /// Why this restore is needed, for the audit trail.
        #[arg(long, default_value = "")]
        reason: String,
        /// Where to write the plan.
        #[arg(long)]
        out: PathBuf,
    },
    /// Apply an approved plan. Requires the plan hash printed by `plan`.
    Apply {
        #[arg(long)]
        plan: PathBuf,
        /// The plan hash, or at least its first 12 hex characters.
        #[arg(long)]
        approve: String,
        /// Run every check and lookup, then report what would happen. No writes.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum BackupCommand {
    /// Back up every configured user, or only those given.
    Run {
        /// Limit to these users (UPN or object id). Repeatable.
        #[arg(long)]
        user: Vec<String>,
        /// Limit to one workload.
        #[arg(long, value_parser = parse_workload)]
        workload: Option<Workload>,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SnapshotCommand {
    /// List snapshots, optionally for one user.
    List {
        #[arg(long)]
        user: Option<String>,
    },
    /// Show a snapshot's summary, or its items with --items.
    Show {
        #[arg(long)]
        user: String,
        #[arg(long, value_parser = parse_workload)]
        workload: Workload,
        /// Snapshot id, or `latest`.
        #[arg(long, default_value = "latest")]
        snapshot: String,
        #[arg(long)]
        items: bool,
    },
}

fn parse_workload(s: &str) -> Result<Workload, String> {
    match s {
        "mail" => Ok(Workload::Mail),
        "drive" => Ok(Workload::Drive),
        _ => Err("expected mail or drive".into()),
    }
}

#[derive(Subcommand)]
enum KeyCommand {
    /// Generate a new master key into a file with mode 600. The key is never printed.
    Generate {
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Subcommand)]
enum RepoCommand {
    /// Create a new, empty repository in the configured storage.
    Init,
    /// Show repository details and check the key.
    Info,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("BANKSIA_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::ExitCode::from(1)
        }
    }
}

async fn run(cli: Cli) -> Result<std::process::ExitCode> {
    match cli.command {
        Command::Key {
            command: KeyCommand::Generate { out },
        } => {
            key_generate(&out)?;
            println!("wrote new master key to {}", out.display());
            println!("keep an offline copy: losing this key makes the repository unreadable");
        }
        Command::Backup {
            command:
                BackupCommand::Run {
                    user,
                    workload,
                    json,
                },
        } => {
            let cfg = Config::load(&cli.config)?;
            let repo = open_repo(&cfg).await?;
            let graph = banksia_core::graph::client(&cfg, &cfg.backup.auth, "backup.auth")?;
            let mut users = cfg.backup.users.clone();
            if !user.is_empty() {
                users.retain(|u| user.iter().any(|x| x.eq_ignore_ascii_case(&u.id)));
                if users.is_empty() {
                    bail!("none of the given users are in backup.users");
                }
            }
            if let Some(w) = workload {
                for u in users.iter_mut() {
                    u.workloads.retain(|x| *x == w);
                }
            }
            let ctx = backup::BackupContext {
                repo,
                graph,
                state_dir: cfg.state.dir.clone(),
                parallel_jobs: cfg.graph.concurrency,
            };
            let results = backup::run(&ctx, &users).await;
            let failed = results.iter().filter(|r| !r.ok()).count();
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                for r in &results {
                    match (&r.error, &r.stats) {
                        (Some(e), _) => println!("FAIL  {} {}: {e}", r.user, r.workload),
                        (None, Some(s)) => println!(
                            "{}  {} {}: snapshot {} items {} downloaded {} ({} bytes) carried {} errors {} in {} ms",
                            if r.partial { "PART" } else { "OK  " },
                            r.user,
                            r.workload,
                            r.snapshot_id.as_deref().unwrap_or("-"),
                            s.items,
                            s.items_downloaded,
                            s.bytes_downloaded,
                            s.items_carried_forward,
                            s.errors,
                            s.duration_ms
                        ),
                        _ => {}
                    }
                }
            }
            if failed > 0 {
                return Ok(std::process::ExitCode::from(2));
            }
        }
        Command::Snapshots { command } => {
            let cfg = Config::load(&cli.config)?;
            let repo = open_repo(&cfg).await?;
            match command {
                SnapshotCommand::List { user } => {
                    let tag = match &user {
                        Some(u) => Some(
                            repo.find_principal(u)
                                .await?
                                .with_context(|| format!("no backups for {u}"))?
                                .0,
                        ),
                        None => None,
                    };
                    for r in repo.list_snapshots(tag.as_deref()).await? {
                        let who = repo
                            .principal(&r.principal_tag)
                            .await?
                            .and_then(|p| p.upn)
                            .unwrap_or_else(|| r.principal_tag.clone());
                        println!("{}  {:<5}  {}", r.id, r.workload, who);
                    }
                }
                SnapshotCommand::Show {
                    user,
                    workload,
                    snapshot,
                    items,
                } => {
                    let snap = load_snapshot(&repo, &user, workload, &snapshot).await?;
                    if items {
                        if let Some(m) = &snap.mail {
                            for msg in &m.messages {
                                let folder = m
                                    .folder(&msg.folder_id)
                                    .map(|f| f.path.as_str())
                                    .unwrap_or("?");
                                println!(
                                    "message  {}  {}  {}  {}",
                                    msg.id,
                                    folder,
                                    msg.received.as_deref().unwrap_or("-"),
                                    msg.subject.as_deref().unwrap_or("")
                                );
                            }
                        }
                        if let Some(d) = &snap.drive {
                            for i in d.items.iter().filter(|i| !i.path.is_empty()) {
                                println!(
                                    "{}  {}  {}  {}",
                                    if i.folder { "folder" } else { "file  " },
                                    i.id,
                                    i.size,
                                    i.path
                                );
                            }
                        }
                    } else {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "id": snap.id,
                                "workload": snap.workload,
                                "user": snap.principal.upn,
                                "started": snap.started,
                                "completed": snap.completed,
                                "parent": snap.parent,
                                "partial": snap.partial,
                                "stats": snap.stats,
                                "errors": snap.errors,
                            }))?
                        );
                    }
                }
            }
        }
        Command::Restore { command } => return restore_cmd(&cli.config, command).await,
        Command::Audit {
            command: AuditCommand::List { json },
        } => {
            let cfg = Config::load(&cli.config)?;
            let repo = open_repo(&cfg).await?;
            let events = repo.audit_events().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&events)?);
            } else {
                for e in events {
                    println!(
                        "{}  {:<28} {:<16} plan {}  {}",
                        e.time,
                        e.kind,
                        e.operator,
                        e.plan_id.as_deref().unwrap_or("-"),
                        e.details
                    );
                }
            }
        }
        Command::Repo { command } => {
            let cfg = Config::load(&cli.config)?;
            let store = Store::from_config(&cfg.storage)?;
            let key = load_master_key(&cfg)?;
            match command {
                RepoCommand::Init => {
                    let repo = Repository::init(store, &key).await?;
                    println!(
                        "initialised repository {} at {}",
                        repo.config().repo_id,
                        repo.store().label()
                    );
                }
                RepoCommand::Info => {
                    let repo = Repository::open(store, &key).await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "location": repo.store().label(),
                            "repo_id": repo.config().repo_id,
                            "format_version": repo.config().version,
                            "created": repo.config().created,
                            "chunk_size": repo.config().chunk_size,
                            "key": "ok",
                        }))?
                    );
                }
            }
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

async fn restore_cmd(
    config: &std::path::Path,
    command: RestoreCommand,
) -> Result<std::process::ExitCode> {
    use banksia_core::restore::{self, PlanRequest, Selection};
    let cfg = Config::load(config)?;
    let repo = open_repo(&cfg).await?;
    match command {
        RestoreCommand::Plan {
            user,
            workload,
            snapshot,
            message,
            folder,
            item,
            path,
            all,
            target,
            in_place,
            reason,
            out,
        } => {
            let chosen = [
                !message.is_empty(),
                folder.is_some(),
                !item.is_empty(),
                path.is_some(),
                all,
            ]
            .iter()
            .filter(|x| **x)
            .count();
            if chosen != 1 {
                bail!("choose exactly one of --message, --folder, --item, --path or --all");
            }
            let selection = if all {
                Selection::All
            } else if !message.is_empty() {
                Selection::MailMessages(message)
            } else if let Some(f) = folder {
                Selection::MailFolder(f)
            } else if !item.is_empty() {
                Selection::DriveItems(item)
            } else {
                Selection::DrivePath(path.expect("checked"))
            };
            let snap = load_snapshot(&repo, &user, workload, &snapshot).await?;
            let target_user = target.unwrap_or_else(|| {
                snap.principal
                    .upn
                    .clone()
                    .unwrap_or_else(|| snap.principal.object_id.clone())
            });
            // Resolve the target with read-only backup credentials; planning never writes.
            let graph = banksia_core::graph::client(&cfg, &cfg.backup.auth, "backup.auth")?;
            let who = banksia_core::backup::resolve_user(&graph, &target_user)
                .await
                .with_context(|| format!("cannot resolve target {target_user}"))?;
            if !cfg.restore.is_allowed(&target_user, &who.object_id) {
                eprintln!(
                    "warning: {target_user} is not on restore.allowed_targets; apply will refuse this plan"
                );
            }
            let plan = restore::build_plan(
                &repo,
                PlanRequest {
                    snapshot: &snap,
                    selection,
                    target_user,
                    target_object_id: who.object_id,
                    in_place,
                    created_by: banksia_core::audit::operator(),
                    purpose: reason,
                },
            )?;
            let bytes = restore::encode_plan(&plan)?;
            let hash = restore::plan_hash(&bytes);
            if out.exists() {
                bail!("{} already exists", out.display());
            }
            std::fs::write(&out, &bytes)
                .with_context(|| format!("cannot write {}", out.display()))?;
            repo.audit(
                "restore.plan_created",
                &plan.created_by,
                Some((&plan.plan_id, &hash)),
                serde_json::json!({
                    "target": plan.target.user,
                    "snapshot": plan.source.snapshot_id,
                    "workload": plan.source.workload,
                    "summary": plan.summary,
                    "purpose": plan.purpose,
                }),
            )
            .await?;
            println!("restore plan {}", plan.plan_id);
            println!(
                "  source   {} {} snapshot {} ({})",
                plan.source.user.as_deref().unwrap_or("?"),
                plan.source.workload,
                plan.source.snapshot_id,
                plan.source.snapshot_completed
            );
            println!(
                "  target   {} ({})",
                plan.target.user, plan.target.object_id
            );
            match &plan.mode {
                restore::RestoreMode::InPlace => {
                    println!("  mode     in place (additive; existing items are never overwritten)")
                }
                restore::RestoreMode::Alternate { root } => {
                    println!("  mode     under {}", root.join("/"))
                }
            }
            let s = &plan.summary;
            println!(
                "  actions  {} mail folders, {} messages, {} drive folders, {} files, {} bytes",
                s.mail_folders, s.messages, s.drive_folders, s.files, s.bytes
            );
            for a in plan.actions.iter().take(20) {
                println!("    {}", a.describe());
            }
            if plan.actions.len() > 20 {
                println!("    ... {} more", plan.actions.len() - 20);
            }
            println!("  written  {}", out.display());
            println!("  hash     {hash}");
            println!();
            println!("To apply after review:");
            println!(
                "  banksia restore apply --plan {} --approve {}",
                out.display(),
                &hash[..16]
            );
        }
        RestoreCommand::Apply {
            plan,
            approve,
            dry_run,
            json,
        } => {
            let bytes =
                std::fs::read(&plan).with_context(|| format!("cannot read {}", plan.display()))?;
            let (plan, hash) = restore::verify_approval(&bytes, &approve)?;
            let auth = cfg
                .restore
                .auth
                .as_ref()
                .context("restore.auth is not configured")?;
            let graph = banksia_core::graph::client(&cfg, auth, "restore.auth")?;
            let ctx = banksia_core::restore::apply::ApplyContext {
                repo: &repo,
                graph: &graph,
                state_dir: cfg.state.dir.clone(),
                enabled: cfg.restore.enabled,
                allowed_targets: cfg.restore.allowed_targets.clone(),
                operator: banksia_core::audit::operator(),
                audit_kind: "restore",
            };
            let report = banksia_core::restore::apply::apply(&ctx, &plan, &hash, dry_run).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                for r in &report.results {
                    println!(
                        "{:<22} {}{}",
                        format!("{:?}", r.outcome),
                        r.action,
                        r.detail
                            .as_ref()
                            .map(|d| format!("  ({d})"))
                            .unwrap_or_default()
                    );
                }
                println!(
                    "{}: {} {}, {} already present, {} failed in {} ms",
                    if dry_run { "dry run" } else { "applied" },
                    report.created,
                    if dry_run {
                        "would be created"
                    } else {
                        "created"
                    },
                    report.already_present,
                    report.failed,
                    report.duration_ms
                );
            }
            if !report.ok() {
                return Ok(std::process::ExitCode::from(2));
            }
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

async fn open_repo(cfg: &Config) -> Result<Repository> {
    let store = Store::from_config(&cfg.storage)?;
    let key = load_master_key(cfg)?;
    Repository::open(store, &key).await
}

async fn load_snapshot(
    repo: &Repository,
    user: &str,
    workload: Workload,
    id: &str,
) -> Result<banksia_core::snapshot::Snapshot> {
    let (tag, _) = repo
        .find_principal(user)
        .await?
        .with_context(|| format!("no backups for {user}"))?;
    if id == "latest" {
        repo.latest_snapshot(&tag, workload)
            .await?
            .with_context(|| format!("no {workload} snapshots for {user}"))
    } else {
        repo.load_snapshot(&tag, workload, id).await
    }
}

fn load_master_key(cfg: &Config) -> Result<MasterKey> {
    let s = cfg.key.master_key.load("key.master_key")?;
    MasterKey::parse(s.expose())
}

fn key_generate(out: &std::path::Path) -> Result<()> {
    use std::io::Write;
    if out.exists() {
        bail!(
            "{} already exists; refusing to overwrite a key",
            out.display()
        );
    }
    let key = MasterKey::generate();
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(out)
        .with_context(|| format!("cannot create {}", out.display()))?;
    f.write_all(key.to_key_file().as_bytes())?;
    f.sync_all()?;
    Ok(())
}
