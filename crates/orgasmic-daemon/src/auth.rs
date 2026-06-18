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
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, Utc};
use orgasmic_core::Home;
use rand::Rng;
use subtle::ConstantTimeEq;

const UI_SESSION_COOKIE: &str = "orgasmic_ui_session";
const UI_TICKET_TTL_SECONDS: i64 = 60;
const UI_SESSION_TTL_SECONDS: i64 = 12 * 60 * 60;

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

impl AuthState {
    pub fn new(token: String) -> Self {
        Self {
            token,
            ui_sessions: Arc::new(Mutex::new(UiSessionStore {
                tickets: BTreeMap::new(),
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

pub async fn bearer_middleware(
    State(auth): State<AuthState>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let query_token_allowed = path == "/api/ws"
        || path.starts_with("/api/ws/")
        || path == "/ws"
        || path.starts_with("/ws/");
    let authorized = auth.check_headers(request.headers())
        || (query_token_allowed && auth.check_query_token(request.uri().query()));
    if authorized {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "missing or invalid bearer token",
        )
            .into_response()
    }
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
}
