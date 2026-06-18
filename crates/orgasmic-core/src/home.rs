// arch: arch_QXS5W.2, arch_WZFAX.1
// orgasmic:arch_WZFAX, arch_C87Z9, arch_QXS5W, dec_XSV21, dec_N17XX
//! Two-layer `$ORGASMIC_HOME` layout per arch_001 / dec_002.
//!
//! Shared between the CLI and daemon so both resolve user overrides before
//! shipped content the same way (arch_011 loader rule).

use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum HomeError {
    #[error("$HOME is not set; cannot locate orgasmic home")]
    HomeUnset,
    #[error("io {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone)]
pub struct Home {
    pub root: PathBuf,
}

impl Home {
    pub fn from_env() -> Result<Self, HomeError> {
        if let Ok(custom) = std::env::var("ORGASMIC_HOME") {
            if !custom.is_empty() {
                return Ok(Self {
                    root: PathBuf::from(custom),
                });
            }
        }
        let home = std::env::var("HOME").map_err(|_| HomeError::HomeUnset)?;
        if home.is_empty() {
            return Err(HomeError::HomeUnset);
        }
        Ok(Self {
            root: PathBuf::from(home).join(".orgasmic"),
        })
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn source(&self) -> PathBuf {
        self.root.join("orgasmic")
    }
    pub fn runtimes(&self) -> PathBuf {
        self.root.join("runtimes")
    }
    pub fn current_runtime(&self) -> PathBuf {
        self.root.join("current")
    }
    pub fn install_json(&self) -> PathBuf {
        self.root.join("install.json")
    }
    pub fn user(&self) -> PathBuf {
        self.root.join("user")
    }
    pub fn state(&self) -> PathBuf {
        self.root.join("state")
    }
    pub fn tx(&self) -> PathBuf {
        self.state().join("tx")
    }
    pub fn sessions(&self) -> PathBuf {
        self.root.join("sessions")
    }
    pub fn secrets(&self) -> PathBuf {
        self.root.join("secrets")
    }
    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }
    pub fn config(&self) -> PathBuf {
        self.root.join("config.yaml")
    }
    pub fn bin_orgasmic(&self) -> PathBuf {
        self.bin().join("orgasmic")
    }
    /// Managed shell env file (sourced by the user's startup files) that puts
    /// `bin/` on PATH. See `orgasmic-cli`'s `path_env` module.
    pub fn env_file(&self) -> PathBuf {
        self.root.join("env")
    }
    pub fn auth_token(&self) -> PathBuf {
        self.user().join("auth").join("token")
    }
    pub fn board(&self) -> PathBuf {
        self.user().join("board.org")
    }

    pub fn required_dirs(&self) -> Vec<PathBuf> {
        vec![
            self.user(),
            self.user().join("auth"),
            self.state(),
            self.tx(),
            self.sessions(),
            self.secrets(),
            self.logs(),
            self.bin(),
        ]
    }

    pub fn ensure(&self) -> Result<(), HomeError> {
        create_dir_all(&self.root)?;
        for d in self.required_dirs() {
            create_dir_all(&d)?;
        }
        if !self.config().exists() {
            write_file(self.config(), DEFAULT_CONFIG.as_bytes())?;
        }
        let gitignore = self.secrets().join(".gitignore");
        if !gitignore.exists() {
            write_file(gitignore, b"*\n!.gitignore\n")?;
        }
        let auth_gitignore = self.user().join("auth").join(".gitignore");
        if !auth_gitignore.exists() {
            write_file(auth_gitignore, b"token\n")?;
        }
        Ok(())
    }
}

const DEFAULT_CONFIG: &str = "\
# orgasmic config — local-first defaults (dec_021).
bind_host: 127.0.0.1
bind_port: 4848
lan_enabled: false
mdns: false
log_level: info
watcher:
  debounce_ms: 200
tx:
  commit_to_project: true
manager:
  actor: \"\"
";

/// User override beats shipped content (arch_001 / arch_011).
pub fn resolve_loader(home: &Home, relative: &Path) -> Option<PathBuf> {
    let user = home.user().join(relative);
    if user.exists() {
        return Some(user);
    }
    let shipped = home.source().join("shipped").join(relative);
    if shipped.exists() {
        return Some(shipped);
    }
    None
}

fn create_dir_all(path: &Path) -> Result<(), HomeError> {
    std::fs::create_dir_all(path).map_err(|source| HomeError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn write_file(path: PathBuf, bytes: &[u8]) -> Result<(), HomeError> {
    std::fs::write(&path, bytes).map_err(|source| HomeError::Io { path, source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_creates_layout_and_seeds_config() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        for d in home.required_dirs() {
            assert!(d.is_dir(), "{} should be a directory", d.display());
        }
        assert!(home.config().is_file());
        let cfg = std::fs::read_to_string(home.config()).unwrap();
        assert!(cfg.contains("bind_host: 127.0.0.1"));
        // Idempotent — does not overwrite seeded config.
        std::fs::write(home.config(), "bind: 0.0.0.0\n").unwrap();
        home.ensure().unwrap();
        let cfg = std::fs::read_to_string(home.config()).unwrap();
        assert_eq!(cfg, "bind: 0.0.0.0\n");
    }

    #[test]
    fn loader_prefers_user_over_shipped() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        std::fs::create_dir_all(home.source().join("shipped/workers")).unwrap();
        std::fs::create_dir_all(home.user().join("workers")).unwrap();
        let rel = Path::new("workers/implementer-claude.org");
        std::fs::write(home.source().join("shipped").join(rel), "shipped").unwrap();
        let p = resolve_loader(&home, rel).unwrap();
        assert!(p.starts_with(home.source()));
        std::fs::write(home.user().join(rel), "user").unwrap();
        let p = resolve_loader(&home, rel).unwrap();
        assert!(p.starts_with(home.user()));
    }

    #[test]
    fn loader_returns_none_when_neither_present() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        assert!(resolve_loader(&home, Path::new("missing.org")).is_none());
    }
}
