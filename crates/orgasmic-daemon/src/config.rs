// arch: arch_C87Z9.1
// orgasmic:arch_C87Z9, dec_N17XX, TASK-AYXPB, dec_WDR5K
//! Daemon runtime configuration.
//!
//! Loaded from `$ORGASMIC_HOME/config.yaml` with CLI overrides for
//! `--bind` / `--port`. Local-first defaults per dec_021.
//!
//! `dispatch:` accepts a sparse governance overlay keyed by kind or
//! `kind,harness` (dec_WDR5K item 3). Absent keys keep code defaults; the file
//! never requires the full schema (`serde(default)` everywhere).

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use orgasmic_core::Home;
use serde::{Deserialize, Serialize};

use crate::governance::{
    known_governance_patch_keys, known_sandbox_permission_keys, normalize_governance_key,
    DispatchGovernanceOverlay, GovernancePatch,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    pub bind: IpAddr,
    pub port: u16,
    #[serde(default)]
    pub lan: bool,
    #[serde(default)]
    pub mdns: bool,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_debounce_ms")]
    pub watcher_debounce_ms: u64,
    #[serde(default = "default_commit_to_project")]
    pub tx_commit_to_project: bool,
    #[serde(default)]
    pub manager_actor: Option<String>,
    #[serde(default = "default_auto_commit_signal")]
    pub auto_commit_signal: bool,
    #[serde(default)]
    pub driver_defaults: DriverDefaults,
    /// Sparse per-kind / per-(kind,harness) governance overlays from `dispatch:`.
    #[serde(skip)]
    pub dispatch_governance: DispatchGovernanceOverlay,
    /// Config keys present in YAML but absent from the known schema.
    #[serde(skip)]
    pub unrecognized_keys: Vec<String>,
    pub home_root: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriverDefaults {
    #[serde(default)]
    pub hermes: HermesDriverDefaults,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HermesDriverDefaults {
    #[serde(default)]
    pub acp_ws: AcpWsDriverDefaults,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcpWsDriverDefaults {
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub session_token_env: Option<String>,
}

fn default_log_level() -> String {
    "info".into()
}

fn default_debounce_ms() -> u64 {
    200
}

fn default_commit_to_project() -> bool {
    true
}

fn default_auto_commit_signal() -> bool {
    true
}

impl DaemonConfig {
    pub fn load(home: &Home) -> Result<Self> {
        let path = home.config();
        let mut cfg = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let value: serde_yaml::Value =
                serde_yaml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
            let unrecognized_keys = collect_unrecognized_keys(&value);
            let parsed: YamlConfig = serde_yaml::from_value(value)
                .with_context(|| format!("parse {}", path.display()))?;
            let (auto_commit_signal, dispatch_governance) = dispatch_from_yaml(parsed.dispatch);
            DaemonConfig {
                bind: parsed
                    .bind_host
                    .or(parsed.bind)
                    .unwrap_or_else(|| "127.0.0.1".parse().unwrap()),
                port: parsed.bind_port.or(parsed.port).unwrap_or(4848),
                lan: parsed.lan_enabled.or(parsed.lan).unwrap_or(false),
                mdns: parsed.mdns.unwrap_or(false),
                log_level: parsed.log_level.unwrap_or_else(default_log_level),
                watcher_debounce_ms: parsed
                    .watcher
                    .as_ref()
                    .and_then(|w| w.debounce_ms)
                    .or(parsed.watcher_debounce_ms)
                    .unwrap_or_else(default_debounce_ms),
                tx_commit_to_project: parsed
                    .tx
                    .as_ref()
                    .and_then(|tx| tx.commit_to_project)
                    .unwrap_or_else(default_commit_to_project),
                manager_actor: parsed
                    .manager
                    .and_then(|manager| manager.actor)
                    .and_then(non_empty),
                auto_commit_signal,
                driver_defaults: driver_defaults(parsed.drivers),
                dispatch_governance,
                unrecognized_keys,
                home_root: home.root.clone(),
            }
        } else {
            DaemonConfig {
                bind: "127.0.0.1".parse().unwrap(),
                port: 4848,
                lan: false,
                mdns: false,
                log_level: default_log_level(),
                watcher_debounce_ms: default_debounce_ms(),
                tx_commit_to_project: default_commit_to_project(),
                manager_actor: None,
                auto_commit_signal: default_auto_commit_signal(),
                driver_defaults: driver_defaults(None),
                dispatch_governance: DispatchGovernanceOverlay::default(),
                unrecognized_keys: Vec::new(),
                home_root: home.root.clone(),
            }
        };
        // LAN bind requires explicit opt-in (dec_021); otherwise pin to localhost.
        if !cfg.lan && !cfg.bind.is_loopback() {
            cfg.bind = "127.0.0.1".parse().unwrap();
        }
        Ok(cfg)
    }

    pub fn with_bind(mut self, bind: IpAddr) -> Self {
        self.bind = bind;
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
}

fn dispatch_from_yaml(parsed: Option<DispatchYaml>) -> (bool, DispatchGovernanceOverlay) {
    let Some(parsed) = parsed else {
        return (
            default_auto_commit_signal(),
            DispatchGovernanceOverlay::default(),
        );
    };
    let auto_commit_signal = parsed
        .auto_commit_signal
        .unwrap_or_else(default_auto_commit_signal);
    let mut map = BTreeMap::new();
    for (key, value) in parsed.governance {
        let canonical = match normalize_governance_key(&key) {
            Ok(canonical) => canonical,
            Err(err) => {
                tracing::warn!(
                    key = %key,
                    error = %err,
                    "ignoring malformed dispatch governance overlay key"
                );
                continue;
            }
        };
        match serde_yaml::from_value::<GovernancePatch>(value) {
            Ok(patch) => {
                map.insert(canonical, patch);
            }
            Err(err) => {
                tracing::warn!(
                    key = %key,
                    error = %err,
                    "ignoring invalid dispatch governance overlay entry"
                );
            }
        }
    }
    (auto_commit_signal, DispatchGovernanceOverlay::from_map(map))
}

fn driver_defaults(parsed: Option<DriversYaml>) -> DriverDefaults {
    let mut defaults = parsed_driver_defaults(parsed);

    if let Some(endpoint) = std::env::var("HERMES_ACP_WS_ENDPOINT")
        .ok()
        .and_then(non_empty)
    {
        defaults.hermes.acp_ws.endpoint = Some(endpoint);
    }
    if let Some(token_env) = std::env::var("HERMES_ACP_WS_SESSION_TOKEN_ENV")
        .ok()
        .and_then(non_empty)
    {
        defaults.hermes.acp_ws.session_token_env = Some(token_env);
    }
    finalize_driver_defaults(defaults)
}

fn parsed_driver_defaults(parsed: Option<DriversYaml>) -> DriverDefaults {
    DriverDefaults {
        hermes: HermesDriverDefaults {
            acp_ws: parsed
                .and_then(|drivers| drivers.hermes)
                .and_then(|hermes| hermes.acp_ws)
                .map(|acp_ws| AcpWsDriverDefaults {
                    endpoint: acp_ws.endpoint.and_then(non_empty),
                    session_token_env: acp_ws.session_token_env.and_then(non_empty),
                })
                .unwrap_or_default(),
        },
    }
}

fn finalize_driver_defaults(mut defaults: DriverDefaults) -> DriverDefaults {
    if defaults.hermes.acp_ws.endpoint.is_some()
        && defaults.hermes.acp_ws.session_token_env.is_none()
    {
        defaults.hermes.acp_ws.session_token_env = Some("HERMES_ACP_WS_SESSION_TOKEN".to_string());
    }
    defaults
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Walk a parsed YAML document and return dotted paths for keys outside the
/// known config schema. Unknown keys are warnings, not errors.
pub fn collect_unrecognized_keys(value: &serde_yaml::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_mapping() else {
        return out;
    };
    for (key, child) in map {
        let Some(name) = key.as_str() else {
            continue;
        };
        match name {
            "bind_host"
            | "bind_port"
            | "lan_enabled"
            | "bind"
            | "port"
            | "lan"
            | "mdns"
            | "log_level"
            | "watcher_debounce_ms" => {}
            "watcher" => collect_object_keys(child, "watcher", &["debounce_ms"], &mut out),
            "tx" => collect_object_keys(child, "tx", &["commit_to_project"], &mut out),
            "manager" => collect_object_keys(child, "manager", &["actor"], &mut out),
            "dispatch" => collect_dispatch_keys(child, &mut out),
            "drivers" => collect_drivers_keys(child, &mut out),
            other => out.push(other.to_string()),
        }
    }
    out
}

fn collect_object_keys(
    value: &serde_yaml::Value,
    prefix: &str,
    known: &[&str],
    out: &mut Vec<String>,
) {
    let Some(map) = value.as_mapping() else {
        return;
    };
    for (key, child) in map {
        let Some(name) = key.as_str() else {
            continue;
        };
        if known.contains(&name) {
            continue;
        }
        let path = format!("{prefix}.{name}");
        out.push(path);
        // Still surface nested unknowns under an already-unknown parent? No —
        // the parent key itself is enough signal.
        let _ = child;
    }
}

fn collect_dispatch_keys(value: &serde_yaml::Value, out: &mut Vec<String>) {
    let Some(map) = value.as_mapping() else {
        return;
    };
    for (key, child) in map {
        let Some(name) = key.as_str() else {
            continue;
        };
        if name == "auto_commit_signal" {
            continue;
        }
        if normalize_governance_key(name).is_ok() {
            collect_governance_patch_keys(child, &format!("dispatch.{name}"), out);
            continue;
        }
        out.push(format!("dispatch.{name}"));
    }
}

fn collect_governance_patch_keys(value: &serde_yaml::Value, prefix: &str, out: &mut Vec<String>) {
    let Some(map) = value.as_mapping() else {
        return;
    };
    let known = known_governance_patch_keys();
    for (key, child) in map {
        let Some(name) = key.as_str() else {
            continue;
        };
        if !known.contains(&name) {
            out.push(format!("{prefix}.{name}"));
            continue;
        }
        if name == "sandbox_permissions" {
            collect_object_keys(
                child,
                &format!("{prefix}.sandbox_permissions"),
                known_sandbox_permission_keys(),
                out,
            );
        }
    }
}

fn collect_drivers_keys(value: &serde_yaml::Value, out: &mut Vec<String>) {
    let Some(map) = value.as_mapping() else {
        return;
    };
    for (key, child) in map {
        let Some(name) = key.as_str() else {
            continue;
        };
        if name != "hermes" {
            out.push(format!("drivers.{name}"));
            continue;
        }
        collect_hermes_keys(child, out);
    }
}

fn collect_hermes_keys(value: &serde_yaml::Value, out: &mut Vec<String>) {
    let Some(map) = value.as_mapping() else {
        return;
    };
    for (key, child) in map {
        let Some(name) = key.as_str() else {
            continue;
        };
        if name != "acp_ws" {
            out.push(format!("drivers.hermes.{name}"));
            continue;
        }
        collect_object_keys(
            child,
            "drivers.hermes.acp_ws",
            &["endpoint", "session_token_env"],
            out,
        );
    }
}

#[derive(Debug, Default, Deserialize)]
struct YamlConfig {
    bind_host: Option<IpAddr>,
    bind_port: Option<u16>,
    lan_enabled: Option<bool>,
    bind: Option<IpAddr>,
    port: Option<u16>,
    lan: Option<bool>,
    mdns: Option<bool>,
    log_level: Option<String>,
    watcher_debounce_ms: Option<u64>,
    watcher: Option<WatcherYaml>,
    tx: Option<TxYaml>,
    manager: Option<ManagerYaml>,
    dispatch: Option<DispatchYaml>,
    drivers: Option<DriversYaml>,
}

#[derive(Debug, Default, Deserialize)]
struct WatcherYaml {
    debounce_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct TxYaml {
    commit_to_project: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct ManagerYaml {
    actor: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DispatchYaml {
    #[serde(default)]
    auto_commit_signal: Option<bool>,
    /// Kind or `kind,harness` sparse patches stored as raw YAML so unknown or
    /// non-object sibling keys (warned by [`collect_unrecognized_keys`]) cannot
    /// fail the whole config parse.
    #[serde(default, flatten)]
    governance: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct DriversYaml {
    hermes: Option<HermesDriverYaml>,
}

#[derive(Debug, Default, Deserialize)]
struct HermesDriverYaml {
    acp_ws: Option<AcpWsDriverYaml>,
}

#[derive(Debug, Default, Deserialize)]
struct AcpWsDriverYaml {
    endpoint: Option<String>,
    session_token_env: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::governance::resolve_governance;
    use orgasmic_core::WorkerKind;

    #[test]
    fn loads_defaults_when_no_config() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        let cfg = DaemonConfig::load(&home).unwrap();
        assert_eq!(cfg.port, 4848);
        assert!(cfg.bind.is_loopback());
        assert!(!cfg.lan);
        assert!(cfg.tx_commit_to_project);
        assert!(cfg.manager_actor.is_none());
        assert!(cfg.auto_commit_signal);
        assert_eq!(cfg.driver_defaults, DriverDefaults::default());
        assert!(cfg.dispatch_governance.is_empty());
        assert!(cfg.unrecognized_keys.is_empty());
    }

    #[test]
    fn rewrites_non_loopback_when_lan_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        std::fs::write(
            home.config(),
            "bind_host: 0.0.0.0\nbind_port: 5000\nlan_enabled: false\n",
        )
        .unwrap();
        let cfg = DaemonConfig::load(&home).unwrap();
        assert!(cfg.bind.is_loopback());
        assert_eq!(cfg.port, 5000);
    }

    #[test]
    fn keeps_lan_bind_when_opted_in() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        std::fs::write(home.config(), "bind_host: 0.0.0.0\nlan_enabled: true\n").unwrap();
        let cfg = DaemonConfig::load(&home).unwrap();
        assert!(!cfg.bind.is_loopback());
        assert!(cfg.lan);
    }

    #[test]
    fn loads_nested_tx_manager_and_watcher_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        std::fs::write(
            home.config(),
            "bind_host: 127.0.0.1\nbind_port: 8739\nwatcher:\n  debounce_ms: 350\ntx:\n  commit_to_project: false\nmanager:\n  actor: dev@example.com\n",
        )
        .unwrap();
        let cfg = DaemonConfig::load(&home).unwrap();
        assert_eq!(cfg.port, 8739);
        assert_eq!(cfg.watcher_debounce_ms, 350);
        assert!(!cfg.tx_commit_to_project);
        assert_eq!(cfg.manager_actor.as_deref(), Some("dev@example.com"));
    }

    #[test]
    fn loads_hermes_acp_ws_driver_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        std::fs::write(
            home.config(),
            "drivers:\n  hermes:\n    acp_ws:\n      endpoint: ws://127.0.0.1:9090/acp\n      session_token_env: HERMES_TEST_TOKEN\n",
        )
        .unwrap();
        let cfg = DaemonConfig::load(&home).unwrap();
        assert_eq!(
            cfg.driver_defaults.hermes.acp_ws.endpoint.as_deref(),
            Some("ws://127.0.0.1:9090/acp")
        );
        assert_eq!(
            cfg.driver_defaults
                .hermes
                .acp_ws
                .session_token_env
                .as_deref(),
            Some("HERMES_TEST_TOKEN")
        );
    }

    #[test]
    fn hermes_acp_ws_endpoint_defaults_token_env_name() {
        let defaults = finalize_driver_defaults(parsed_driver_defaults(Some(DriversYaml {
            hermes: Some(HermesDriverYaml {
                acp_ws: Some(AcpWsDriverYaml {
                    endpoint: Some("ws://127.0.0.1:9090/acp".to_string()),
                    session_token_env: None,
                }),
            }),
        })));

        assert_eq!(
            defaults.hermes.acp_ws.session_token_env.as_deref(),
            Some("HERMES_ACP_WS_SESSION_TOKEN")
        );
    }

    #[test]
    fn loads_sparse_dispatch_governance_overlay() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        std::fs::write(
            home.config(),
            r#"
dispatch:
  auto_commit_signal: false
  implementer:
    max_iterations: 30
    context_budget_chars: 200000
  "implementer,codex":
    stall_timeout_secs: 120
    sandbox_permissions:
      allow_exec: false
      allow_network: true
"#,
        )
        .unwrap();
        let cfg = DaemonConfig::load(&home).unwrap();
        assert!(!cfg.auto_commit_signal);
        assert!(cfg.unrecognized_keys.is_empty());

        let kind_only = resolve_governance(
            WorkerKind::Implementer,
            Some("claude"),
            &cfg.dispatch_governance,
            None,
        );
        assert_eq!(kind_only.max_iterations, Some(30));
        assert_eq!(kind_only.context_budget_chars, Some(200_000));
        // kind overlay does not set stall; code default remains
        assert_eq!(kind_only.stall_timeout_secs, Some(600));

        let kind_harness = resolve_governance(
            WorkerKind::Implementer,
            Some("codex"),
            &cfg.dispatch_governance,
            None,
        );
        assert_eq!(kind_harness.max_iterations, Some(30));
        assert_eq!(kind_harness.stall_timeout_secs, Some(120));
        let sandbox = kind_harness.sandbox_permissions.expect("sandbox overlay");
        assert!(!sandbox.allow_exec);
        assert!(sandbox.allow_network);
    }

    #[test]
    fn dispatch_overlay_legacy_context_budget_migrates_to_chars() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        std::fs::write(
            home.config(),
            r#"
dispatch:
  implementer:
    context_budget: 50000
"#,
        )
        .unwrap();
        let cfg = DaemonConfig::load(&home).unwrap();
        let resolved = resolve_governance(
            WorkerKind::Implementer,
            None,
            &cfg.dispatch_governance,
            None,
        );
        assert_eq!(resolved.context_budget_chars, Some(200_000));
    }

    #[test]
    fn dispatch_overlay_keys_normalize_to_canonical_spelling() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        std::fs::write(
            home.config(),
            r#"
dispatch:
  " implementer ":
    max_iterations: 31
  implementer, codex:
    max_iterations: 41
    stall_timeout_secs: 90
"#,
        )
        .unwrap();
        let cfg = DaemonConfig::load(&home).unwrap();
        assert!(cfg.unrecognized_keys.is_empty());

        let kind_only = resolve_governance(
            WorkerKind::Implementer,
            Some("claude"),
            &cfg.dispatch_governance,
            None,
        );
        assert_eq!(kind_only.max_iterations, Some(31));

        let kind_harness = resolve_governance(
            WorkerKind::Implementer,
            Some("codex"),
            &cfg.dispatch_governance,
            None,
        );
        assert_eq!(kind_harness.max_iterations, Some(41));
        assert_eq!(kind_harness.stall_timeout_secs, Some(90));
    }

    #[test]
    fn dispatch_overlay_unquoted_comma_key_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        std::fs::write(
            home.config(),
            r#"
dispatch:
  implementer,codex:
    max_iterations: 42
"#,
        )
        .unwrap();
        let cfg = DaemonConfig::load(&home).unwrap();
        assert!(cfg.unrecognized_keys.is_empty());
        let resolved = resolve_governance(
            WorkerKind::Implementer,
            Some("codex"),
            &cfg.dispatch_governance,
            None,
        );
        assert_eq!(resolved.max_iterations, Some(42));
    }

    #[test]
    fn dispatch_overlay_malformed_keys_surface_as_unrecognized() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
dispatch:
  implementer,codex,typo:
    max_iterations: 1
"#,
        )
        .unwrap();
        let keys = collect_unrecognized_keys(&yaml);
        assert!(
            keys.iter().any(|k| k == "dispatch.implementer,codex,typo"),
            "{keys:?}"
        );
    }

    #[test]
    fn warns_on_unrecognized_config_keys() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
bind_host: 127.0.0.1
totally_unknown: 1
watcher:
  debounce_ms: 200
  mystery: true
dispatch:
  auto_commit_signal: true
  not_a_kind: 1
  implementer:
    max_iterations: 20
    invent_field: 9
    sandbox_permissions:
      allow_exec: true
      allow_telepathy: true
drivers:
  hermes:
    acp_ws:
      endpoint: ws://x
      extra: 1
  other_driver: {}
"#,
        )
        .unwrap();
        let keys = collect_unrecognized_keys(&yaml);
        assert!(keys.iter().any(|k| k == "totally_unknown"), "{keys:?}");
        assert!(keys.iter().any(|k| k == "watcher.mystery"), "{keys:?}");
        assert!(keys.iter().any(|k| k == "dispatch.not_a_kind"), "{keys:?}");
        assert!(
            keys.iter()
                .any(|k| k == "dispatch.implementer.invent_field"),
            "{keys:?}"
        );
        assert!(
            keys.iter()
                .any(|k| k == "dispatch.implementer.sandbox_permissions.allow_telepathy"),
            "{keys:?}"
        );
        assert!(
            keys.iter().any(|k| k == "drivers.hermes.acp_ws.extra"),
            "{keys:?}"
        );
        assert!(keys.iter().any(|k| k == "drivers.other_driver"), "{keys:?}");
        assert!(!keys.iter().any(|k| k.contains("auto_commit_signal")));
        assert!(!keys.iter().any(|k| k.contains("max_iterations")));
    }

    #[test]
    fn load_surfaces_unrecognized_keys_for_startup_warnings() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        std::fs::write(home.config(), "bind_port: 4848\nnope: true\n").unwrap();
        let cfg = DaemonConfig::load(&home).unwrap();
        assert_eq!(cfg.unrecognized_keys, vec!["nope".to_string()]);
    }
}
