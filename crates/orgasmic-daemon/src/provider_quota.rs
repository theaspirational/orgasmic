//! Durable, machine-local provider quota lockouts (TASK-40ZMJ).

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use orgasmic_core::Home;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderLockout {
    pub provider: String,
    pub locked_until: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    pub run_id: String,
    pub signal: String,
}

fn store_path(home: &Home) -> PathBuf {
    home.state().join("provider-lockouts.json")
}

fn lock_file(home: &Home) -> Result<File> {
    std::fs::create_dir_all(home.state())?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(home.state().join("provider-lockouts.lock"))?;
    FileExt::lock_exclusive(&file)?;
    Ok(file)
}

fn read_store(home: &Home) -> Result<BTreeMap<String, ProviderLockout>> {
    let path = store_path(home);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("parse provider lockout memory {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn write_store(home: &Home, store: &BTreeMap<String, ProviderLockout>) -> Result<()> {
    let path = store_path(home);
    let temporary = home
        .state()
        .join(format!("provider-lockouts.{}.tmp", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(store)?)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    if cfg!(windows) && path.exists() {
        std::fs::remove_file(&path)?;
    }
    std::fs::rename(&temporary, &path)
        .with_context(|| format!("replace provider lockout memory {}", path.display()))?;
    Ok(())
}

pub fn remember(home: &Home, lockout: ProviderLockout) -> Result<()> {
    if lockout.provider.is_empty()
        || !lockout
            .provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("invalid provider id {:?}", lockout.provider);
    }
    let lock = lock_file(home)?;
    let mut store = read_store(home)?;
    let replace = store
        .get(&lockout.provider)
        .is_none_or(|current| current.observed_at <= lockout.observed_at);
    if replace {
        store.insert(lockout.provider.clone(), lockout);
        write_store(home, &store)?;
    }
    FileExt::unlock(&lock)?;
    Ok(())
}

pub fn active(home: &Home, provider: &str, now: DateTime<Utc>) -> Result<Option<ProviderLockout>> {
    if !store_path(home).exists() {
        return Ok(None);
    }
    let lock = lock_file(home)?;
    let found = read_store(home)?
        .remove(provider)
        .filter(|lockout| lockout.locked_until > now);
    FileExt::unlock(&lock)?;
    Ok(found)
}

/// Parse a provider's `Retry-After` value without inventing a duration.
pub fn retry_deadline(value: &str, observed_at: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(absolute) = DateTime::parse_from_rfc3339(value) {
        return Some(absolute.with_timezone(&Utc));
    }
    let mut parts = value.split_whitespace();
    let token = parts.next()?.to_ascii_lowercase();
    let digits = token.trim_end_matches(|ch: char| ch.is_ascii_alphabetic());
    let amount = digits.parse::<i64>().ok()?;
    let suffix = token
        .strip_prefix(digits)
        .filter(|suffix| !suffix.is_empty())
        .or_else(|| parts.next())
        .unwrap_or("s");
    let seconds = match suffix {
        "s" | "sec" | "secs" | "second" | "seconds" => amount,
        "m" | "min" | "mins" | "minute" | "minutes" => amount.checked_mul(60)?,
        "h" | "hr" | "hrs" | "hour" | "hours" => amount.checked_mul(3_600)?,
        "d" | "day" | "days" => amount.checked_mul(86_400)?,
        _ => return None,
    };
    observed_at.checked_add_signed(Duration::seconds(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_deadline_accepts_provider_durations_and_absolute_time() {
        let now = DateTime::parse_from_rfc3339("2026-09-02T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            retry_deadline("42s", now).unwrap(),
            now + Duration::seconds(42)
        );
        assert_eq!(
            retry_deadline("5 days", now).unwrap(),
            now + Duration::days(5)
        );
        assert_eq!(
            retry_deadline("2026-09-03T12:00:00Z", now).unwrap(),
            now + Duration::days(1)
        );
        assert_eq!(retry_deadline("when the provider says so", now), None);
    }

    #[test]
    fn remembered_lockout_is_active_only_until_its_provider_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path());
        home.ensure().unwrap();
        let now = Utc::now();
        remember(
            &home,
            ProviderLockout {
                provider: "codex".into(),
                locked_until: now + Duration::minutes(5),
                observed_at: now,
                run_id: "run-quota".into(),
                signal: "exit_reason.retry_after".into(),
            },
        )
        .unwrap();

        assert_eq!(
            active(&home, "codex", now).unwrap().unwrap().run_id,
            "run-quota"
        );
        assert!(active(&home, "claude", now).unwrap().is_none());
        assert!(active(&home, "codex", now + Duration::minutes(6))
            .unwrap()
            .is_none());
    }
}
