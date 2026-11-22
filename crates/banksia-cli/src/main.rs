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
