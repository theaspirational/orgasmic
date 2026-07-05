// arch: arch_C87Z9.1
// orgasmic:arch_C87Z9, dec_XV9AK, dec_N17XX
//! Bearer token storage and middleware.
//!
//! Tokens live at `$ORGASMIC_HOME/user/auth/token` (gitignored). First daemon
//! boot generates one if missing. Verification uses constant-time compare.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::http::{header, HeaderMap};
use chrono::{DateTime, Duration, Utc};
use orgasmic_core::Home;
use rand::Rng;
use subtle::ConstantTimeEq;

use crate::authz::Identity;

const UI_SESSION_COOKIE: &str = "orgasmic_ui_session";
const UI_TICKET_TTL_SECONDS: i64 = 60;
const UI_SESSION_TTL_SECONDS: i64 = 12 * 60 * 60;
/// Member session cookie — distinct from the admin `UI_SESSION_COOKIE`.
/// "Long-lived" per dec_N9HW0: unlike the admin flow, a member has no CLI to
/// re-mint a ticket if their session expires, only their durable token
/// (which they may not have saved) — so this outlives the 12h admin session.
const MEMBER_SESSION_COOKIE: &str = "orgasmic_member_session";
const MEMBER_SESSION_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;

/// Read the token from disk, generating one if missing.
pub fn load_or_generate(home: &Home) -> Result<String> {
    let path = home.auth_token();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if path.exists() {
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let token = raw.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    let token = generate_token();
    write_token(&path, &token)?;
    Ok(token)
}

fn write_token(path: &Path, token: &str) -> Result<()> {
    std::fs::write(path, token).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn generate_token() -> String {
    // 32 bytes encoded as 64 hex chars — enough entropy for a local secret
    // and trivial to copy/paste from a terminal.
    let bytes: [u8; 32] = rand::thread_rng().gen();
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[derive(Debug, Clone)]
pub struct AuthState {
    pub token: String,
    ui_sessions: Arc<Mutex<UiSessionStore>>,
    member_sessions: Arc<Mutex<MemberSessionStore>>,
}

#[derive(Debug)]
struct UiSessionStore {
    tickets: BTreeMap<String, DateTime<Utc>>,
    sessions: BTreeMap<String, DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct UiSessionTicket {
    pub ticket: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug)]
struct MemberSessionStore {
    sessions: BTreeMap<String, MemberSession>,
}

#[derive(Debug, Clone)]
struct MemberSession {
    member_name: String,
    expires_at: DateTime<Utc>,
}

impl MemberSessionStore {
    fn prune(&mut self, now: DateTime<Utc>) {
        self.sessions.retain(|_, s| s.expires_at > now);
    }
}

impl AuthState {
    pub fn new(token: String) -> Self {
        Self {
            token,
            ui_sessions: Arc::new(Mutex::new(UiSessionStore {
                tickets: BTreeMap::new(),
                sessions: BTreeMap::new(),
            })),
            member_sessions: Arc::new(Mutex::new(MemberSessionStore {
                sessions: BTreeMap::new(),
            })),
        }
    }

    fn check_token(&self, presented: &str) -> bool {
        let a = self.token.as_bytes();
        let b = presented.trim().as_bytes();
        if a.len() != b.len() {
            return false;
        }
        a.ct_eq(b).into()
    }

    pub fn check(&self, headers: &HeaderMap) -> bool {
        let Some(raw) = headers.get(header::AUTHORIZATION) else {
            return false;
        };
        let Ok(value) = raw.to_str() else {
            return false;
        };
        let Some(presented) = value.strip_prefix("Bearer ") else {
            return false;
        };
        self.check_token(presented)
    }

    pub fn check_headers(&self, headers: &HeaderMap) -> bool {
        self.check(headers) || self.check_ui_session(headers)
    }

    pub fn check_query_token(&self, query: Option<&str>) -> bool {
        let Some(query) = query else {
            return false;
        };
        query.split('&').any(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            key == "token" && self.check_token(value)
        })
    }

    pub fn create_ui_ticket(&self) -> UiSessionTicket {
        let ticket = generate_token();
        let expires_at = Utc::now() + Duration::seconds(UI_TICKET_TTL_SECONDS);
        if let Ok(mut store) = self.ui_sessions.lock() {
            store.prune(Utc::now());
            store.tickets.insert(ticket.clone(), expires_at);
        }
        UiSessionTicket { ticket, expires_at }
    }

    pub fn redeem_ui_ticket(&self, ticket: &str) -> Option<String> {
        let now = Utc::now();
        let mut store = self.ui_sessions.lock().ok()?;
        store.prune(now);
        let expires_at = store.tickets.remove(ticket)?;
        if expires_at <= now {
            return None;
        }
        let session = generate_token();
        store.sessions.insert(
            session.clone(),
            now + Duration::seconds(UI_SESSION_TTL_SECONDS),
        );
        Some(session)
    }

    pub fn ui_session_cookie_name() -> &'static str {
        UI_SESSION_COOKIE
    }

    pub fn ui_session_max_age_seconds() -> i64 {
        UI_SESSION_TTL_SECONDS
    }

    fn check_ui_session(&self, headers: &HeaderMap) -> bool {
        let Some(session) = cookie_value(headers, UI_SESSION_COOKIE) else {
            return false;
        };
        let now = Utc::now();
        let Ok(mut store) = self.ui_sessions.lock() else {
            return false;
        };
        store.prune(now);
        store
            .sessions
            .get(session)
            .map(|expires_at| *expires_at > now)
            .unwrap_or(false)
    }

    pub fn member_session_cookie_name() -> &'static str {
        MEMBER_SESSION_COOKIE
    }

    pub fn member_session_max_age_seconds() -> i64 {
        MEMBER_SESSION_TTL_SECONDS
    }

    /// Mint a named member session, set at `/login` after a token hash match.
    pub fn create_member_session(&self, member_name: &str) -> (String, DateTime<Utc>) {
        let session = generate_token();
        let expires_at = Utc::now() + Duration::seconds(MEMBER_SESSION_TTL_SECONDS);
        if let Ok(mut store) = self.member_sessions.lock() {
            store.prune(Utc::now());
            store.sessions.insert(
                session.clone(),
                MemberSession {
                    member_name: member_name.to_string(),
                    expires_at,
                },
            );
        }
        (session, expires_at)
    }

    fn check_member_session(&self, headers: &HeaderMap) -> Option<String> {
        let session = cookie_value(headers, MEMBER_SESSION_COOKIE)?;
        let now = Utc::now();
        let mut store = self.member_sessions.lock().ok()?;
        store.prune(now);
        store.sessions.get(session).map(|s| s.member_name.clone())
    }

    /// Resolve the request's identity: the pre-existing admin bearer
    /// token/UI session takes priority (unchanged, full access); otherwise a
    /// member session cookie is looked up and cross-checked against
    /// `members.org` *fresh* on every call, so a revoke or grant edit on disk
    /// takes effect on the very next request with no separate invalidation
    /// channel. Returns `None` when neither resolves (401 at the call site).
    pub fn resolve_identity(&self, headers: &HeaderMap, home: &Home) -> Option<Identity> {
        if self.check_headers(headers) {
            return Some(Identity::Admin);
        }
        let member_name = self.check_member_session(headers)?;
        let entry = orgasmic_core::find_member_by_name(home, &member_name)
            .ok()
            .flatten()?;
        Some(Identity::Member {
            name: entry.name,
            grants: entry.grants,
        })
    }
}

impl UiSessionStore {
    fn prune(&mut self, now: DateTime<Utc>) {
        self.tickets.retain(|_, expires_at| *expires_at > now);
        self.sessions.retain(|_, expires_at| *expires_at > now);
    }
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        if key == name {
            Some(value)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn generate_or_load_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let a = load_or_generate(&home).unwrap();
        let b = load_or_generate(&home).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn check_accepts_correct_bearer() {
        let auth = AuthState::new("secret".into());
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        assert!(auth.check(&headers));
    }

    #[test]
    fn check_rejects_missing_or_wrong_token() {
        let auth = AuthState::new("secret".into());
        let headers = HeaderMap::new();
        assert!(!auth.check(&headers));
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong"),
        );
        assert!(!auth.check(&headers));
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("secret"));
        assert!(!auth.check(&headers));
    }

    #[test]
    fn check_accepts_ws_query_token() {
        let auth = AuthState::new("secret".into());
        assert!(auth.check_query_token(Some("topic=graph&token=secret")));
        assert!(!auth.check_query_token(Some("topic=graph&token=wrong")));
        assert!(!auth.check_query_token(Some("topic=graph")));
    }

    fn member_cookie_headers(session: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!(
                "{}={session}",
                AuthState::member_session_cookie_name()
            ))
            .unwrap(),
        );
        headers
    }

    #[test]
    fn resolve_identity_admin_bearer_takes_priority() {
        let auth = AuthState::new("secret".into());
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        assert!(matches!(
            auth.resolve_identity(&headers, &home),
            Some(Identity::Admin)
        ));
    }

    #[test]
    fn resolve_identity_member_session_resolves_current_grants() {
        let auth = AuthState::new("secret".into());
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        orgasmic_core::add_member(&home, "alice", &[("proj-a".into(), "editor".into())]).unwrap();

        let (session, _expires) = auth.create_member_session("alice");
        let headers = member_cookie_headers(&session);
        match auth.resolve_identity(&headers, &home) {
            Some(Identity::Member { name, grants }) => {
                assert_eq!(name, "alice");
                assert_eq!(grants, vec![("proj-a".to_string(), "editor".to_string())]);
            }
            other => panic!("expected member identity, got {other:?}"),
        }
    }

    #[test]
    fn resolve_identity_returns_none_after_revoke() {
        let auth = AuthState::new("secret".into());
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        orgasmic_core::add_member(&home, "alice", &[("*".into(), "viewer".into())]).unwrap();
        let (session, _) = auth.create_member_session("alice");
        let headers = member_cookie_headers(&session);
        assert!(auth.resolve_identity(&headers, &home).is_some());

        orgasmic_core::revoke_member(&home, "alice").unwrap();
        assert!(
            auth.resolve_identity(&headers, &home).is_none(),
            "revoking a member must invalidate their live session on the next lookup"
        );
    }

    #[test]
    fn resolve_identity_rejects_unknown_session() {
        let auth = AuthState::new("secret".into());
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let headers = member_cookie_headers("bogus-session-id");
        assert!(auth.resolve_identity(&headers, &home).is_none());
    }
}
