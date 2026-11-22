//! `banksia.toml` configuration.
//!
//! Unknown keys are rejected. Secrets are never accepted inline: a secret
//! field must be a table naming an environment variable or a file, for example
//! `client_secret = { env = "BANKSIA_BACKUP_CLIENT_SECRET" }`.

use crate::secret::SecretSource;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub tenant: TenantConfig,
    pub storage: StorageConfig,
    pub key: KeyConfig,
    #[serde(default)]
    pub graph: GraphConfig,
    #[serde(default)]
    pub state: StateConfig,
    pub backup: BackupConfig,
    #[serde(default)]
    pub restore: RestoreConfig,
    pub restore_test: Option<RestoreTestConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantConfig {
    /// Directory (tenant) id or verified domain.
    pub tenant_id: String,
    /// Override for national clouds and tests.
    #[serde(default = "default_authority")]
    pub authority_host: String,
}

fn default_authority() -> String {
    "https://login.microsoftonline.com".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphConfig {
    #[serde(default = "default_graph")]
    pub base_url: String,
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    /// Allow plain HTTP base URLs. Only for local fakes in tests.
    #[serde(default)]
    pub allow_insecure_http_for_tests: bool,
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            base_url: default_graph(),
            max_retries: default_retries(),
            concurrency: default_concurrency(),
            allow_insecure_http_for_tests: false,
        }
    }
}

fn default_graph() -> String {
    "https://graph.microsoft.com/v1.0".into()
}
fn default_retries() -> u32 {
    8
}
fn default_concurrency() -> usize {
    4
}

/// An Entra app registration used with the client credentials flow.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppCredentials {
    pub client_id: String,
    /// PEM file holding the public certificate registered on the app.
    pub certificate_path: Option<PathBuf>,
    /// PKCS#8 PEM private key for the certificate.
    #[serde(default, deserialize_with = "secret_opt")]
    pub private_key: Option<SecretSource>,
    /// Client secret. Supported for evaluation; prefer certificates.
    #[serde(default, deserialize_with = "secret_opt")]
    pub client_secret: Option<SecretSource>,
}

impl AppCredentials {
    fn validate(&self, what: &str) -> Result<()> {
        if self.client_id.trim().is_empty() {
            bail!("{what}: client_id is empty");
        }
        match (
            &self.certificate_path,
            &self.private_key,
            &self.client_secret,
        ) {
            (Some(_), Some(k), None) => k.validate(&format!("{what}.private_key")),
            (None, None, Some(s)) => s.validate(&format!("{what}.client_secret")),
            _ => bail!(
                "{what}: set certificate_path and private_key, or client_secret, but not both"
            ),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum StorageConfig {
    /// Azure Blob Storage.
    Azure {
        account: String,
        container: String,
        #[serde(default)]
        prefix: String,
        /// Storage account key. If unset, Banksia uses workload identity,
        /// managed identity or Azure CLI credentials from the environment.
        #[serde(default, deserialize_with = "secret_opt")]
        access_key: Option<SecretSource>,
    },
    /// S3-compatible storage such as Cloudflare R2.
    S3 {
        endpoint: String,
        bucket: String,
        #[serde(default = "default_region")]
        region: String,
        #[serde(default)]
        prefix: String,
        #[serde(deserialize_with = "secret")]
        access_key_id: SecretSource,
        #[serde(deserialize_with = "secret")]
        secret_access_key: SecretSource,
    },
    /// A local directory.
    Local { path: PathBuf },
    /// In-memory, for tests only.
    Memory,
}

fn default_region() -> String {
    "auto".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyConfig {
    /// Where the master key is read from.
    #[serde(deserialize_with = "secret")]
    pub master_key: SecretSource,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateConfig {
    #[serde(default = "default_state_dir")]
    pub dir: PathBuf,
}

impl Default for StateConfig {
    fn default() -> Self {
        Self {
            dir: default_state_dir(),
        }
    }
}

fn default_state_dir() -> PathBuf {
    PathBuf::from(".banksia-state")
}

#[derive(
    Debug, Clone, Copy, Deserialize, serde::Serialize, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(rename_all = "snake_case")]
pub enum Workload {
    Mail,
    Drive,
}

impl Workload {
    pub fn as_str(self) -> &'static str {
        match self {
            Workload::Mail => "mail",
            Workload::Drive => "drive",
        }
    }
}

impl std::fmt::Display for Workload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupConfig {
    pub auth: AppCredentials,
    #[serde(default)]
    pub users: Vec<UserSelection>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserSelection {
    /// UPN or Entra object id.
    pub id: String,
    #[serde(default = "default_workloads")]
    pub workloads: Vec<Workload>,
}

fn default_workloads() -> Vec<Workload> {
    vec![Workload::Mail, Workload::Drive]
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreConfig {
    /// Restores into the tenant are refused unless this is true.
    #[serde(default)]
    pub enabled: bool,
    /// Users (UPN or object id) that restores may write into.
    #[serde(default)]
    pub allowed_targets: Vec<String>,
    pub auth: Option<AppCredentials>,
}

impl RestoreConfig {
    pub fn is_allowed(&self, upn_or_id: &str, object_id: &str) -> bool {
        self.allowed_targets
            .iter()
            .any(|t| t.eq_ignore_ascii_case(upn_or_id) || t.eq_ignore_ascii_case(object_id))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreTestConfig {
    /// Throwaway mailbox that test restores write into.
    pub mail_target: Option<String>,
    /// Throwaway user whose OneDrive test restores write into.
    pub drive_target: Option<String>,
    #[serde(default = "default_sample")]
    pub sample_size: usize,
    #[serde(default = "default_max_age")]
    pub max_snapshot_age_hours: u64,
    /// Local directory for evidence packs.
    #[serde(default = "default_evidence_dir")]
    pub evidence_dir: PathBuf,
    /// Credentials for restore tests. Falls back to `restore.auth`.
    pub auth: Option<AppCredentials>,
}

fn default_sample() -> usize {
    5
}
fn default_max_age() -> u64 {
    26
}
fn default_evidence_dir() -> PathBuf {
    PathBuf::from("evidence")
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read configuration {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid configuration {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        // toml's Display quotes the offending source line, which could echo a
        // secret pasted into the file. Report the line and message only.
        let cfg: Config = toml::from_str(text).map_err(|e| {
            let line = e
                .span()
                .map(|r| text[..r.start.min(text.len())].matches('\n').count() + 1);
            match line {
                Some(l) => anyhow::anyhow!("line {l}: {}", e.message()),
                None => anyhow::anyhow!("{}", e.message()),
            }
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.tenant.tenant_id.trim().is_empty() {
            bail!("tenant.tenant_id is empty");
        }
        let insecure = self.graph.allow_insecure_http_for_tests;
        check_endpoint(
            "tenant.authority_host",
            &self.tenant.authority_host,
            insecure,
        )?;
        check_endpoint("graph.base_url", &self.graph.base_url, insecure)?;
        self.key.master_key.validate("key.master_key")?;
        self.backup.auth.validate("backup.auth")?;
        if let Some(a) = &self.restore.auth {
            a.validate("restore.auth")?;
        }
        if self.restore.enabled && self.restore.auth.is_none() {
            bail!("restore.enabled is true but restore.auth is not configured");
        }
        if let Some(rt) = &self.restore_test {
            if let Some(a) = &rt.auth {
                a.validate("restore_test.auth")?;
            }
            for t in [&rt.mail_target, &rt.drive_target].into_iter().flatten() {
                if !self
                    .restore
                    .allowed_targets
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(t))
                {
                    bail!("restore_test target {t} must also be listed in restore.allowed_targets");
                }
            }
            if rt.sample_size == 0 {
                bail!("restore_test.sample_size must be at least 1");
            }
        }
        for u in &self.backup.users {
            if u.id.trim().is_empty() {
                bail!("backup.users: empty id");
            }
        }
        if self.graph.concurrency == 0 {
            bail!("graph.concurrency must be at least 1");
        }
        Ok(())
    }
}

/// Endpoints must be https with no credentials in the URL. Plain http is
/// only accepted, with the test flag set, for a loopback host.
fn check_endpoint(what: &str, value: &str, allow_insecure: bool) -> Result<()> {
    let u =
        url::Url::parse(value).map_err(|e| anyhow::anyhow!("{what} is not a valid URL: {e}"))?;
    if !u.username().is_empty() || u.password().is_some() {
        bail!("{what} must not contain credentials");
    }
    let loopback = match u.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    match u.scheme() {
        "https" => Ok(()),
        "http" if allow_insecure && loopback => Ok(()),
        "http" if allow_insecure => {
            bail!("{what}: graph.allow_insecure_http_for_tests only permits a loopback host")
        }
        _ => bail!("{what} must use https"),
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SecretField {
    Source(SecretSource),
    Inline(#[allow(dead_code)] toml::Value),
}

fn secret<'de, D: Deserializer<'de>>(d: D) -> Result<SecretSource, D::Error> {
    match SecretField::deserialize(d)? {
        SecretField::Source(s) => Ok(s),
        SecretField::Inline(_) => Err(serde::de::Error::custom(
            "inline secrets are not allowed; use { env = \"NAME\" } or { file = \"/path\" }",
        )),
    }
}

fn secret_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<SecretSource>, D::Error> {
    secret(d).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
[tenant]
tenant_id = "contoso.onmicrosoft.com"

[storage]
kind = "local"
path = "/var/lib/banksia/repo"

[key]
master_key = { file = "/etc/banksia/master.key" }

[backup.auth]
client_id = "00000000-0000-0000-0000-000000000001"
client_secret = { env = "BANKSIA_BACKUP_CLIENT_SECRET" }

[[backup.users]]
id = "adele@contoso.onmicrosoft.com"
"#;

    #[test]
    fn parses_minimal() {
        let c = Config::parse(BASE).unwrap();
        assert_eq!(
            c.backup.users[0].workloads,
            vec![Workload::Mail, Workload::Drive]
        );
        assert!(!c.restore.enabled);
        assert_eq!(c.graph.base_url, "https://graph.microsoft.com/v1.0");
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../banksia.example.toml");
        Config::parse(text).unwrap();
    }

    #[test]
    fn rejects_inline_secret() {
        let t = BASE.replace(
            r#"client_secret = { env = "BANKSIA_BACKUP_CLIENT_SECRET" }"#,
            r#"client_secret = "abc123""#,
        );
        let err = format!("{:#}", Config::parse(&t).unwrap_err());
        assert!(err.contains("inline secrets are not allowed"), "{err}");
        assert!(
            !err.contains("abc123"),
            "error must not echo the secret: {err}"
        );
    }

    #[test]
    fn endpoint_guard() {
        assert!(check_endpoint("x", "https://graph.microsoft.com/v1.0", false).is_ok());
        assert!(check_endpoint("x", "http://graph.microsoft.com/v1.0", false).is_err());
        assert!(check_endpoint("x", "http://127.0.0.1:9/v1.0", false).is_err());
        assert!(check_endpoint("x", "http://127.0.0.1:9/v1.0", true).is_ok());
        assert!(check_endpoint("x", "http://[::1]:9/v1.0", true).is_ok());
        assert!(check_endpoint("x", "http://localhost/v1.0", true).is_ok());
        // Userinfo tricks and non-loopback hosts are refused.
        assert!(check_endpoint("x", "http://localhost:80@attacker.example/v1.0", true).is_err());
        assert!(check_endpoint("x", "https://user:pw@graph.microsoft.com/v1.0", false).is_err());
        assert!(check_endpoint("x", "http://10.0.0.5/v1.0", true).is_err());
        let t = BASE.replace(
            "[tenant]\n",
            "[tenant]\nauthority_host = \"http://login.example\"\n",
        );
        assert!(Config::parse(&t).is_err(), "plain http authority refused");
    }

    #[test]
    fn rejects_unknown_keys() {
        let t = format!("{BASE}\n[backup.extra]\nx = 1\n");
        assert!(Config::parse(&t).is_err());
        let t = BASE.replace("[tenant]\n", "[tenant]\ncolour = \"red\"\n");
        assert!(Config::parse(&t).is_err());
    }

    #[test]
    fn restore_needs_auth_and_test_targets_must_be_allowed() {
        let t = format!("{BASE}\n[restore]\nenabled = true\n");
        assert!(Config::parse(&t).is_err());
        let t = format!(
            "{BASE}\n[restore]\nallowed_targets = []\n[restore_test]\nmail_target = \"t@x\"\n"
        );
        assert!(Config::parse(&t).is_err());
        let t = format!(
            "{BASE}\n[restore]\nallowed_targets = [\"T@x\"]\n[restore_test]\nmail_target = \"t@x\"\n"
        );
        assert!(Config::parse(&t).is_ok());
    }
}
