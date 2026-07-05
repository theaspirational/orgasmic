// arch: arch_Z8CW2
// orgasmic:arch_Z8CW2, dec_N9HW0, dec_KF2MR
//! Host-local member records: name, hashed token, per-project capability
//! grants. Lives at `$ORGASMIC_HOME/user/auth/members.org` — never in the
//! repo-shared `.orgasmic/`. Shared between the CLI (`orgasmic member
//! add|revoke|list`, direct filesystem, no daemon required) and the daemon
//! (session/login resolution).
//!
//! File shape, one heading per member (mirrors `projects.rs`'s `board.org`):
//!
//! ```text
//! * MEMBER alice
//! :PROPERTIES:
//! :NAME:        alice
//! :TOKEN_HASH:  <sha256 hex>
//! :GRANTS:      proj-a=editor proj-b=viewer *=artifacts
//! :END:
//! ```
//!
//! `GRANTS` is a space-separated list of `<project-or-*>=<role>` pairs;
//! resolution (exact project beats `*`) lives in the daemon's authz seam, not
//! here — this module only reads/writes the record shape.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::{Home, OrgFile};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberEntry {
    pub name: String,
    pub token_hash: String,
    /// `(project-or-*, role)` pairs, in file order.
    pub grants: Vec<(String, String)>,
}

pub fn members_path(home: &Home) -> PathBuf {
    home.members_org()
}

pub fn read_members(home: &Home) -> Result<Vec<MemberEntry>> {
    let path = members_path(home);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let source =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    parse_members(&source, &path)
}

fn parse_members(source: &str, path: &Path) -> Result<Vec<MemberEntry>> {
    if source.trim().is_empty() {
        return Ok(Vec::new());
    }
    let file = OrgFile::parse(source.to_string(), path.to_string_lossy())
        .with_context(|| format!("parse {}", path.display()))?;
    let mut out = Vec::new();
    for h in &file.headings {
        let Some(name) = h.property("NAME") else {
            continue;
        };
        let grants = h
            .property("GRANTS")
            .unwrap_or("")
            .split_whitespace()
            .filter_map(|pair| pair.split_once('='))
            .map(|(project, role)| (project.to_string(), role.to_string()))
            .collect();
        out.push(MemberEntry {
            name: name.to_string(),
            token_hash: h.property("TOKEN_HASH").unwrap_or("").to_string(),
            grants,
        });
    }
    Ok(out)
}

fn render_members(entries: &[MemberEntry]) -> String {
    let mut out = String::from("#+title: orgasmic members\n#+orgasmic_version: 1\n\n");
    for e in entries {
        let grants = e
            .grants
            .iter()
            .map(|(project, role)| format!("{project}={role}"))
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!("* MEMBER {}\n", e.name));
        out.push_str(":PROPERTIES:\n");
        out.push_str(&format!(":NAME:        {}\n", e.name));
        out.push_str(&format!(":TOKEN_HASH:  {}\n", e.token_hash));
        out.push_str(&format!(":GRANTS:      {grants}\n"));
        out.push_str(":END:\n\n");
    }
    out
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Mint a fresh member token: 32 random bytes as 64 hex chars, same shape as
/// the daemon's single bearer-token generator (`orgasmic-daemon::auth`) —
/// distinct secret domain (member vs admin), so kept as its own small
/// generator rather than a shared abstraction.
fn mint_token() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::thread_rng().gen();
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn validate_single_line(s: &str, field: &str, max: usize) -> Result<()> {
    if s.is_empty() || s.len() > max {
        bail!("{field} must be 1-{max} chars");
    }
    if s.chars()
        .any(|c| c == '\n' || c == '\r' || c == '\0' || c.is_control())
    {
        bail!("{field} must not contain newlines or control characters");
    }
    Ok(())
}

fn validate_member_name(name: &str) -> Result<()> {
    if name.len() > 64 {
        bail!("member name must be 1-64 chars");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        || name.is_empty()
    {
        bail!("member name must match [A-Za-z0-9_.-] and be non-empty");
    }
    Ok(())
}

fn validate_grants(grants: &[(String, String)]) -> Result<()> {
    if grants.is_empty() {
        bail!("at least one grant is required");
    }
    for (project, role) in grants {
        if project != "*" {
            validate_single_line(project, "grant project", 64)?;
        }
        validate_single_line(role, "grant role", 64)?;
        if role.contains(char::is_whitespace) || project.contains(char::is_whitespace) {
            bail!("grant entries must not contain whitespace");
        }
    }
    Ok(())
}

/// Mint a new member record and return the plaintext token (shown to the
/// caller exactly once — only the hash is persisted). Errors if `name` is
/// already on file; revoke first to re-mint under the same name (comment
/// attribution survives, since it is by name, not by record).
pub fn add_member(home: &Home, name: &str, grants: &[(String, String)]) -> Result<String> {
    validate_member_name(name)?;
    validate_grants(grants)?;

    let path = members_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    fs2::FileExt::lock_exclusive(&file).with_context(|| format!("lock {}", path.display()))?;
    let result = add_member_locked(&mut file, &path, name, grants);
    let unlock = fs2::FileExt::unlock(&file).with_context(|| format!("unlock {}", path.display()));
    result.and_then(|token| unlock.map(|_| token))
}

fn add_member_locked(
    file: &mut std::fs::File,
    path: &Path,
    name: &str,
    grants: &[(String, String)],
) -> Result<String> {
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("seek {}", path.display()))?;
    let mut source = String::new();
    file.read_to_string(&mut source)
        .with_context(|| format!("read {}", path.display()))?;
    let mut existing = parse_members(&source, path)?;
    if existing.iter().any(|e| e.name == name) {
        bail!("member {name} already exists; revoke first to re-mint");
    }

    let token = mint_token();
    let token_hash = sha256_hex(token.as_bytes());
    existing.push(MemberEntry {
        name: name.to_string(),
        token_hash,
        grants: grants.to_vec(),
    });

    let rendered = render_members(&existing);
    file.set_len(0)
        .with_context(|| format!("truncate {}", path.display()))?;
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("seek {}", path.display()))?;
    file.write_all(rendered.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = file.metadata()?.permissions();
        perms.set_mode(0o600);
        file.set_permissions(perms)?;
    }
    OrgFile::parse(rendered, path.to_string_lossy())
        .context("members.org failed to parse after write")?;
    Ok(token)
}

/// Remove `name`'s record entirely. Returns `false` if no such member existed
/// (not an error — revoking an already-gone member is a no-op the CLI can
/// report plainly). Any session resolved against the removed record becomes
/// invalid on its next lookup (the daemon re-reads this file fresh per
/// request — no separate invalidation channel needed).
pub fn revoke_member(home: &Home, name: &str) -> Result<bool> {
    let path = members_path(home);
    if !path.exists() {
        return Ok(false);
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    fs2::FileExt::lock_exclusive(&file).with_context(|| format!("lock {}", path.display()))?;
    let result = revoke_member_locked(&mut file, &path, name);
    let unlock = fs2::FileExt::unlock(&file).with_context(|| format!("unlock {}", path.display()));
    result.and_then(|removed| unlock.map(|_| removed))
}

fn revoke_member_locked(file: &mut std::fs::File, path: &Path, name: &str) -> Result<bool> {
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("seek {}", path.display()))?;
    let mut source = String::new();
    file.read_to_string(&mut source)
        .with_context(|| format!("read {}", path.display()))?;
    let mut existing = parse_members(&source, path)?;
    let before = existing.len();
    existing.retain(|e| e.name != name);
    if existing.len() == before {
        return Ok(false);
    }

    let rendered = render_members(&existing);
    file.set_len(0)
        .with_context(|| format!("truncate {}", path.display()))?;
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("seek {}", path.display()))?;
    file.write_all(rendered.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    OrgFile::parse(rendered, path.to_string_lossy())
        .context("members.org failed to parse after write")?;
    Ok(true)
}

pub fn find_member_by_name(home: &Home, name: &str) -> Result<Option<MemberEntry>> {
    Ok(read_members(home)?.into_iter().find(|e| e.name == name))
}

/// Hash `token` and look up the member whose stored hash matches. Used by
/// `/login`; constant-time-compare is unnecessary here since we are comparing
/// derived hash digests, not raw secrets against each other byte-by-byte in a
/// way that leaks the secret (the presented token is hashed once, then a
/// plain equality check against on-disk hashes — same shape as any
/// password-hash lookup).
pub fn find_member_by_token(home: &Home, token: &str) -> Result<Option<MemberEntry>> {
    let hash = sha256_hex(token.trim().as_bytes());
    Ok(read_members(home)?.into_iter().find(|e| e.token_hash == hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_mints_token_and_hashes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let token = add_member(&home, "alice", &[("*".into(), "editor".into())]).unwrap();
        assert_eq!(token.len(), 64);

        let entries = read_members(&home).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "alice");
        assert_eq!(entries[0].grants, vec![("*".to_string(), "editor".to_string())]);
        assert_eq!(entries[0].token_hash, sha256_hex(token.as_bytes()));
        assert_ne!(entries[0].token_hash, token);

        let raw = std::fs::read_to_string(members_path(&home)).unwrap();
        assert!(!raw.contains(&token), "plaintext token must never be persisted");
    }

    #[test]
    fn rejects_duplicate_name() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        add_member(&home, "alice", &[("*".into(), "viewer".into())]).unwrap();
        let err = add_member(&home, "alice", &[("*".into(), "editor".into())]).unwrap_err();
        assert!(format!("{err}").contains("already exists"));
    }

    #[test]
    fn revoke_removes_record_and_allows_remint_under_same_name() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        add_member(&home, "alice", &[("*".into(), "viewer".into())]).unwrap();
        assert!(revoke_member(&home, "alice").unwrap());
        assert!(read_members(&home).unwrap().is_empty());
        // Revoking again is a no-op, not an error.
        assert!(!revoke_member(&home, "alice").unwrap());

        let token = add_member(&home, "alice", &[("*".into(), "editor".into())]).unwrap();
        let entries = read_members(&home).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].grants, vec![("*".to_string(), "editor".to_string())]);
        assert_eq!(entries[0].token_hash, sha256_hex(token.as_bytes()));
    }

    #[test]
    fn find_by_token_matches_and_rejects_wrong_token() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let token = add_member(&home, "alice", &[("proj-a".into(), "editor".into())]).unwrap();

        let found = find_member_by_token(&home, &token).unwrap().unwrap();
        assert_eq!(found.name, "alice");
        assert!(find_member_by_token(&home, "not-a-real-token")
            .unwrap()
            .is_none());
    }

    #[test]
    fn multi_grant_round_trips_exact_and_wildcard() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        add_member(
            &home,
            "bob",
            &[
                ("proj-a".into(), "editor".into()),
                ("proj-b".into(), "viewer".into()),
            ],
        )
        .unwrap();
        let entries = read_members(&home).unwrap();
        assert_eq!(
            entries[0].grants,
            vec![
                ("proj-a".to_string(), "editor".to_string()),
                ("proj-b".to_string(), "viewer".to_string()),
            ]
        );
    }

    #[test]
    fn rejects_invalid_member_name() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let err = add_member(&home, "not a name!", &[("*".into(), "viewer".into())]).unwrap_err();
        assert!(format!("{err}").contains("member name"));
    }

    #[test]
    fn rejects_empty_grants() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let err = add_member(&home, "alice", &[]).unwrap_err();
        assert!(format!("{err}").contains("grant is required"));
    }

    #[test]
    fn list_is_empty_when_file_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        assert!(read_members(&home).unwrap().is_empty());
    }
}
