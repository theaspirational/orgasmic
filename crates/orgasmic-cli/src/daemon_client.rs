// arch: arch_C87Z9.5, arch_Z3Z3V.2
// orgasmic:arch_C87Z9, dec_XV9AK, dec_N17XX
//! Thin HTTP client the CLI uses to talk to the local daemon.
//!
//! The CLI is the "complete tool surface for humans and manager agents"
//! (arch_006), so every daemon-mediated operation has a CLI alias that
//! posts to the matching REST route. This module owns the token lookup,
//! base URL composition, and request_id propagation.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use orgasmic_core::Home;
use reqwest::header::AUTHORIZATION;
use reqwest::{Client, RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_yaml::Value as YamlValue;

use crate::daemon_lifecycle;
use crate::manager::DispatchPlan;

/// Per-request timeout for the dispatch POST. Well above the worst-case
/// (now non-blocking) acquire so a slow mux/daemon startup never trips the
/// client's default timeout and fabricates a zombie lease.
const DEFAULT_DISPATCH_REQUEST_TIMEOUT_SECS: u64 = 30;
const API_PREFIX: &str = "/api";
const DAEMON_URL_ENV: &str = "ORGASMIC_DAEMON_URL";
const DAEMON_TOKEN_ENV: &str = "ORGASMIC_DAEMON_TOKEN";
const DAEMON_TOKEN_FILE_ENV: &str = "ORGASMIC_DAEMON_TOKEN_FILE";

fn dispatch_request_timeout() -> std::time::Duration {
    std::env::var("ORGASMIC_DISPATCH_HTTP_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or_else(|| std::time::Duration::from_secs(DEFAULT_DISPATCH_REQUEST_TIMEOUT_SECS))
}

#[derive(Debug, Clone)]
pub struct DaemonClient {
    base: String,
    token: String,
    client: Client,
}

impl DaemonClient {
    pub async fn from_home_autostart_async(home: &Home) -> Result<Self> {
        daemon_lifecycle::ensure_running_async(home).await?;
        Self::from_home(home)
    }

    pub fn from_home_autostart(home: &Home) -> Result<Self> {
        daemon_lifecycle::ensure_running(home)?;
        Self::from_home(home)
    }

    pub fn from_home(home: &Home) -> Result<Self> {
        let token = read_bearer_token(home)?;
        let base = read_base_url(home)?;
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .context("build http client")?;
        Ok(Self {
            base,
            token,
            client,
        })
    }

    pub async fn get<R: DeserializeOwned>(&self, path: &str) -> Result<R> {
        let req = self.client.get(self.url(path));
        send_json(self.bearer(req)).await
    }

    pub async fn post_json<B: Serialize + ?Sized, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<R> {
        let req = self.client.post(self.url(path)).json(body);
        send_json(self.bearer(req)).await
    }

    pub(crate) async fn post_dispatch(&self, plan: &DispatchPlan) -> Result<DispatchResponse> {
        let task = path_segment(&plan.dispatch_task());
        let project = path_segment(&plan.project_id);
        let request = build_dispatch_request(plan);
        // Belt-and-suspenders against the dispatch acquire race: the daemon now
        // returns as soon as the mux session is spawned (~100ms), but a slow
        // mux/daemon startup could still exceed the client's default 10s. A
        // per-request override (well above the worst-case acquire) keeps the CLI
        // from timing out and reporting a transport failure while the daemon
        // actually completed the acquire — which is what created zombie leases.
        let req = self
            .client
            .post(self.url(&format!("/projects/{project}/tasks/{task}/dispatch")))
            .timeout(dispatch_request_timeout())
            .json(&request);
        send_json(self.bearer(req)).await
    }

    pub(crate) async fn post_dispatch_cleanup(
        &self,
        plan: &DispatchPlan,
    ) -> Result<DispatchCleanupResponse> {
        let task = path_segment(&plan.dispatch_task());
        let project = path_segment(&plan.project_id);
        let body = serde_json::json!({
            "kind": plan.kind.as_str(),
            "worktree_path": plan.worktree_path,
            "branch": plan.branch,
        });
        self.post_json(
            &format!("/projects/{project}/tasks/{task}/dispatch/cleanup"),
            &body,
        )
        .await
    }

    pub(crate) fn dispatch_failure_needs_daemon_cleanup(err: &anyhow::Error) -> bool {
        let msg = err.to_string();
        if msg.contains("daemon returned") {
            return false;
        }
        msg.contains("timed out")
            || msg.contains("operation timed out")
            || msg.contains("daemon request failed")
    }

    fn url(&self, path: &str) -> String {
        let path = api_path(path);
        if path.starts_with('/') {
            format!("{}{}", self.base, path)
        } else {
            format!("{}/{}", self.base, path)
        }
    }

    pub fn absolute_url(&self, path: &str) -> String {
        self.url(path)
    }

    fn bearer(&self, req: RequestBuilder) -> RequestBuilder {
        req.header(AUTHORIZATION, format!("Bearer {}", self.token))
    }
}

fn api_path(path: &str) -> String {
    let normalized = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    if normalized == API_PREFIX || normalized.starts_with(&format!("{API_PREFIX}/")) {
        normalized
    } else {
        format!("{API_PREFIX}{normalized}")
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct DispatchRequest {
    kind: String,
    brief_path: PathBuf,
    worktree_path: PathBuf,
    last_path: PathBuf,
    stdout_path: PathBuf,
    worker_id: Option<String>,
    model_override: Option<String>,
    effort_override: Option<String>,
    provider_override: Option<String>,
    reason: Option<String>,
    branch: String,
    liveness: String,
    goal_id: Option<String>,
}

pub(crate) fn build_dispatch_request(plan: &DispatchPlan) -> DispatchRequest {
    DispatchRequest {
        kind: plan.kind.as_str().to_string(),
        brief_path: plan.brief_path.clone(),
        worktree_path: plan.worktree_path.clone(),
        last_path: plan.last_path.clone(),
        stdout_path: plan.stdout_path.clone(),
        worker_id: plan.worker_override.clone(),
        model_override: plan.model_override.clone(),
        effort_override: plan.effort_override.clone(),
        provider_override: None,
        reason: plan.reason.clone(),
        branch: plan.branch.clone(),
        liveness: plan.from_sha.clone(),
        goal_id: plan.goal_id.clone(),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct DispatchCleanupResponse {
    pub status: String,
    #[allow(dead_code)]
    pub released_run_id: Option<String>,
    pub worktree_removed: bool,
    pub branch_deleted: bool,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DispatchResponse {
    pub run_id: String,
    #[allow(dead_code)]
    pub session_path: PathBuf,
    pub pid: u32,
    pub worker_id: String,
    pub driver: String,
    pub harness: String,
    #[allow(dead_code)]
    pub dispatch_tx_id: String,
}

fn path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

async fn send_json<R: DeserializeOwned>(req: RequestBuilder) -> Result<R> {
    let response = req
        .send()
        .await
        .map_err(|e| anyhow!("daemon request failed: {e} — is the daemon reachable?"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        if status == StatusCode::UNAUTHORIZED {
            bail!(
                "unauthorized — check {DAEMON_TOKEN_ENV}/{DAEMON_TOKEN_FILE_ENV} \
                 for external daemons or $ORGASMIC_HOME/user/auth/token locally"
            );
        }
        if status == StatusCode::CONFLICT {
            bail!("conflict — node changed on disk; reload base_version and retry: {body}");
        }
        bail!("daemon returned {status}: {body}");
    }
    response
        .json::<R>()
        .await
        .map_err(|e| anyhow!("decode daemon response: {e}"))
}

pub(crate) fn read_bearer_token(home: &Home) -> Result<String> {
    if explicit_daemon_url().is_some() {
        if let Some(token) = read_external_token()? {
            return Ok(token);
        }
    }
    read_home_token(home)
}

fn read_external_token() -> Result<Option<String>> {
    if let Ok(raw) = std::env::var(DAEMON_TOKEN_ENV) {
        let token = raw.trim().to_string();
        if !token.is_empty() {
            return Ok(Some(token));
        }
    }
    if let Ok(path) = std::env::var(DAEMON_TOKEN_FILE_ENV) {
        let path = path.trim();
        if path.is_empty() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(path).with_context(|| format!("read {path}"))?;
        let token = raw.trim().to_string();
        if token.is_empty() {
            bail!("bearer token at {path} is empty");
        }
        return Ok(Some(token));
    }
    Ok(None)
}

fn read_home_token(home: &Home) -> Result<String> {
    let path = home.auth_token();
    if !path.exists() {
        bail!(
            "bearer token not found at {} — run `orgasmic status` once to start the daemon and generate it",
            path.display()
        );
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let token = raw.trim().to_string();
    if token.is_empty() {
        bail!("bearer token at {} is empty", path.display());
    }
    Ok(token)
}

fn read_base_url(home: &Home) -> Result<String> {
    if let Some(url) = explicit_daemon_url() {
        return Ok(url);
    }
    let (bind, port) = read_bind_port(&home.config())?;
    let host = if bind.is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        bind.to_string()
    };
    Ok(format!("http://{host}:{port}"))
}

fn explicit_daemon_url() -> Option<String> {
    std::env::var(DAEMON_URL_ENV)
        .ok()
        .map(|url| url.trim_end_matches('/').to_string())
        .filter(|url| !url.is_empty())
}

fn read_bind_port(config: &Path) -> Result<(std::net::IpAddr, u16)> {
    let mut bind: std::net::IpAddr = "127.0.0.1".parse().unwrap();
    let mut port: u16 = 4848;
    if config.exists() {
        let raw = std::fs::read_to_string(config)
            .with_context(|| format!("read {}", config.display()))?;
        let value: YamlValue =
            serde_yaml::from_str(&raw).with_context(|| format!("parse {}", config.display()))?;
        if let Some(b) = value
            .get("bind_host")
            .or_else(|| value.get("bind"))
            .and_then(YamlValue::as_str)
        {
            if let Ok(addr) = b.parse() {
                bind = addr;
            }
        }
        if let Some(p) = value
            .get("bind_port")
            .or_else(|| value.get("port"))
            .and_then(YamlValue::as_u64)
        {
            if let Ok(p) = u16::try_from(p) {
                port = p;
            }
        }
    }
    Ok((bind, port))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use super::*;
    use crate::manager::{DispatchKind, DispatchPlan};
    use orgasmic_core::Home;

    fn env_guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    struct ScopedEnv {
        keys: Vec<(&'static str, Option<String>)>,
    }

    impl ScopedEnv {
        fn set(pairs: &[(&'static str, &str)]) -> Self {
            let keys = pairs
                .iter()
                .map(|(key, value)| {
                    let prior = std::env::var(key).ok();
                    std::env::set_var(key, value);
                    (*key, prior)
                })
                .collect();
            Self { keys }
        }

        fn clear(keys: &[&'static str]) -> Self {
            let keys = keys
                .iter()
                .map(|key| {
                    let prior = std::env::var(key).ok();
                    std::env::remove_var(key);
                    (*key, prior)
                })
                .collect();
            Self { keys }
        }
    }

    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            for (key, prior) in &self.keys {
                match prior {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn sample_plan(worker_override: Option<String>) -> DispatchPlan {
        DispatchPlan {
            project_root: PathBuf::from("/tmp/project"),
            project_id: "orgasmic".into(),
            tasks: vec!["TASK-1".into()],
            kind: DispatchKind::Implementer,
            brief_path: PathBuf::from("/tmp/brief.md"),
            brief_content: "brief".into(),
            from_sha: "abc123".into(),
            worktree_path: PathBuf::from("/tmp/worktree"),
            branch: "task-1-impl".into(),
            model_override: None,
            effort_override: None,
            last_path: PathBuf::from("/tmp/last.txt"),
            stdout_path: PathBuf::from("/tmp/stdout.log"),
            goal_id: None,
            reason: None,
            dry_run: false,
            worker_override,
        }
    }

    #[test]
    fn dispatch_args_worker_flag_passes_through_to_daemon_request() {
        let request = build_dispatch_request(&sample_plan(Some("foo".into())));
        assert_eq!(request.worker_id.as_deref(), Some("foo"));
        let request = build_dispatch_request(&sample_plan(None));
        assert!(request.worker_id.is_none());
        assert_eq!(request.branch, "task-1-impl");
        assert_eq!(request.liveness, "abc123");
    }

    #[test]
    fn daemon_client_prefixes_api_paths_once() {
        assert_eq!(api_path("/daemon/status"), "/api/daemon/status");
        assert_eq!(api_path("daemon/status"), "/api/daemon/status");
        assert_eq!(api_path("/api/daemon/status"), "/api/daemon/status");
    }

    #[test]
    fn explicit_daemon_url_prefers_token_env_over_home_token() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        std::fs::write(home.auth_token(), "home-token\n").unwrap();
        let _env = ScopedEnv::set(&[
            (DAEMON_URL_ENV, "http://127.0.0.1:9999"),
            (DAEMON_TOKEN_ENV, "env-token"),
        ]);
        let _clear = ScopedEnv::clear(&[DAEMON_TOKEN_FILE_ENV]);

        assert_eq!(read_bearer_token(&home).unwrap(), "env-token");
    }

    #[test]
    fn explicit_daemon_url_uses_token_file_before_home_token() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        std::fs::write(home.auth_token(), "home-token\n").unwrap();
        let token_file = tmp.path().join("remote-token");
        std::fs::write(&token_file, "file-token\n").unwrap();
        let _env = ScopedEnv::set(&[
            (DAEMON_URL_ENV, "http://127.0.0.1:9999"),
            (DAEMON_TOKEN_FILE_ENV, token_file.to_str().unwrap()),
        ]);
        let _clear = ScopedEnv::clear(&[DAEMON_TOKEN_ENV]);

        assert_eq!(read_bearer_token(&home).unwrap(), "file-token");
    }

    #[test]
    fn local_daemon_ignores_external_token_env() {
        let _guard = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        std::fs::write(home.auth_token(), "home-token\n").unwrap();
        let _env = ScopedEnv::set(&[(DAEMON_TOKEN_ENV, "env-token")]);
        let _clear = ScopedEnv::clear(&[DAEMON_URL_ENV, DAEMON_TOKEN_FILE_ENV]);

        assert_eq!(read_bearer_token(&home).unwrap(), "home-token");
    }
}
