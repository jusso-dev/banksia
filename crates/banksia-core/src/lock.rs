//! Advisory, cross-process locks in the state directory.

use anyhow::{Context, Result, bail};
use std::{fs::File, path::Path};

/// Held for as long as the value lives. The OS releases it if the process dies.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

impl Lock {
    pub fn acquire(state_dir: &Path, name: &str) -> Result<Self> {
        let dir = state_dir.join("locks");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let path = dir.join(format!("{name}.lock"));
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                bail!("lock {name} is held by another process")
            }
            Err(std::fs::TryLockError::Error(e)) => Err(e).context("cannot lock"),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn second_holder_is_refused_until_release() {
        let d = tempfile::tempdir().unwrap();
        let a = super::Lock::acquire(d.path(), "x").unwrap();
        assert!(super::Lock::acquire(d.path(), "x").is_err());
        assert!(super::Lock::acquire(d.path(), "y").is_ok());
        drop(a);
        super::Lock::acquire(d.path(), "x").unwrap();
    }
}
