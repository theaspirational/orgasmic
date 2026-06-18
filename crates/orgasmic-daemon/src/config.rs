// arch: arch_C87Z9.1
// orgasmic:arch_C87Z9, dec_N17XX
//! Daemon runtime configuration.
//!
//! Loaded from `$ORGASMIC_HOME/config.yaml` with CLI overrides for
//! `--bind` / `--port`. Local-first defaults per dec_021.

use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use orgasmic_core::Home;
use serde::{Deserialize, Serialize};

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
            let parsed: YamlConfig =
                serde_yaml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
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
                auto_commit_signal: parsed
                    .dispatch
                    .as_ref()
                    .and_then(|dispatch| dispatch.auto_commit_signal)
                    .unwrap_or_else(default_auto_commit_signal),
                driver_defaults: driver_defaults(parsed.drivers),
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
    auto_commit_signal: Option<bool>,
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
}
