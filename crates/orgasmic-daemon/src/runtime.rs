// arch: arch_Z3Z3V.1
// orgasmic:arch_Z3Z3V
//! Daemon runtime identity per arch_010 / dec_024.
//!
//! `boot_id` is a fresh UUID per daemon process; `runtime_id` will be
//! produced by drivers when they spawn workers (see arch_004). We expose
//! `boot_id` through the status endpoint so the CLI, UI, and manager can
//! detect daemon replacement after a restart.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootIdentity {
    pub boot_id: String,
    pub started_at: DateTime<Utc>,
    pub pid: u32,
    pub version: String,
}

impl BootIdentity {
    pub fn new() -> Self {
        Self {
            boot_id: Uuid::new_v4().to_string(),
            started_at: Utc::now(),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

impl Default for BootIdentity {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_boot_id_is_unique() {
        let a = BootIdentity::new();
        let b = BootIdentity::new();
        assert_ne!(a.boot_id, b.boot_id);
    }
}
