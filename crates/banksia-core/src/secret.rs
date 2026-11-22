//! Secret values and where they come from.
//!
//! Configuration never holds a secret value. It names an environment variable
//! or a file, and the value is loaded at run time into a zeroising wrapper.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{fmt, path::PathBuf};
use zeroize::Zeroizing;

/// A secret held in memory. Not `Serialize`, not `Display`, redacted in `Debug`.
#[derive(Clone)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

/// Where to read a secret from. Exactly one source must be set.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecretSource {
    pub env: Option<String>,
    pub file: Option<PathBuf>,
}

impl SecretSource {
    pub fn env(name: &str) -> Self {
        Self {
            env: Some(name.to_string()),
            file: None,
        }
    }

    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self {
            env: None,
            file: Some(path.into()),
        }
    }

    pub fn is_set(&self) -> bool {
        self.env.is_some() || self.file.is_some()
    }

    pub fn validate(&self, what: &str) -> Result<()> {
        match (&self.env, &self.file) {
            (Some(_), Some(_)) => {
                bail!("{what}: set either an environment variable or a file, not both")
            }
            (None, None) => bail!("{what}: no secret source configured"),
            _ => Ok(()),
        }
    }

    /// Load the secret. Files must not be readable by group or others on Unix.
    pub fn load(&self, what: &str) -> Result<Secret> {
        self.validate(what)?;
        let raw = if let Some(name) = &self.env {
            std::env::var(name)
                .with_context(|| format!("{what}: environment variable {name} is not set"))?
        } else {
            let path = self.file.as_ref().expect("validated");
            check_private_file(path).with_context(|| format!("{what}: {}", path.display()))?;
            std::fs::read_to_string(path)
                .with_context(|| format!("{what}: cannot read {}", path.display()))?
        };
        let value = raw.trim().to_string();
        if value.is_empty() {
            bail!("{what}: secret is empty");
        }
        Ok(Secret::new(value))
    }
}

/// Refuse secret files that other local users can read.
pub fn check_private_file(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).context("cannot stat file")?;
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            bail!(
                "file permissions {:o} are too open; run chmod 600",
                mode & 0o777
            );
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let s = Secret::new("hunter2".into());
        assert!(!format!("{s:?}").contains("hunter2"));
    }

    #[test]
    fn exactly_one_source() {
        assert!(SecretSource::default().validate("x").is_err());
        let both = SecretSource {
            env: Some("A".into()),
            file: Some("b".into()),
        };
        assert!(both.validate("x").is_err());
        assert!(SecretSource::env("A").validate("x").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_world_readable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s");
        std::fs::write(&p, "value").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(SecretSource::file(&p).load("x").is_err());
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(SecretSource::file(&p).load("x").unwrap().expose(), "value");
    }
}
