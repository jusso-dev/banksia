//! `banksia`: self-hosted Microsoft 365 backup with in-tenant restore and
//! restore-test evidence.

use anyhow::{Context, Result, bail};
use banksia_core::{config::Config, crypto::MasterKey, repo::Repository, store::Store};
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
