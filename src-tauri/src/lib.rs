use std::sync::Mutex;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use std::io::{Read, Write};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use std::net::TcpStream;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use std::path::{Path, PathBuf};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use std::process::Command;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use std::time::Duration;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use orgasmic_core::Home;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use tauri_plugin_updater::{Update, UpdaterExt};
use url::Url;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
const LOCAL_DAEMON_URL: &str = "http://127.0.0.1:4848";
#[cfg(target_os = "android")]
const ANDROID_EMULATOR_DAEMON_URL: &str = "http://10.0.2.2:4848";
const UPDATE_REPO: &str = "theaspirational/orgasmic";

struct PendingAppUpdate(Mutex<Option<Update>>);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalBackendProfile {
    id: &'static str,
    name: &'static str,
    base_url: String,
    token: Option<String>,
    home: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeProbe {
    cli_path: Option<String>,
    cli_version: Option<String>,
    daemon_state: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppUpdateMetadata {
    channel: String,
    current_version: String,
    version: String,
}

#[derive(Debug, Deserialize)]
struct LocalRecoveryStatus {
    live_runs: Vec<serde_json::Value>,
}

#[tauri::command]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn runtime_probe() -> RuntimeProbe {
    let Some(cli) = discover_cli() else {
        return RuntimeProbe {
            cli_path: None,
            cli_version: None,
            daemon_state: None,
            error: None,
        };
    };
    let cli_version = command_stdout(&cli, &["--version"]).ok();
    let daemon_state = command_stdout(&cli, &["daemon", "status"]).ok();
    RuntimeProbe {
        cli_path: Some(cli.to_string_lossy().to_string()),
        cli_version,
        daemon_state,
        error: None,
    }
}

#[tauri::command]
#[cfg(any(target_os = "android", target_os = "ios"))]
fn runtime_probe() -> RuntimeProbe {
    RuntimeProbe {
        cli_path: None,
        cli_version: None,
        daemon_state: None,
        error: Some("local CLI runtime is not available on mobile".to_string()),
    }
}

#[tauri::command]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn runtime_launch_url() -> Result<String, String> {
    let cli = discover_cli().ok_or_else(|| "orgasmic CLI is not installed".to_string())?;
    command_stdout(&cli, &["ui", "--print-url"]).map(|url| url.trim().to_string())
}

#[tauri::command]
#[cfg(any(target_os = "android", target_os = "ios"))]
fn runtime_launch_url() -> Result<String, String> {
    Err("local CLI runtime is not available on mobile".to_string())
}

#[tauri::command]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn update_runtime() -> Result<String, String> {
    let cli = discover_cli().ok_or_else(|| "orgasmic CLI is not installed".to_string())?;
    command_stdout(&cli, &["update"])
}

#[tauri::command]
#[cfg(any(target_os = "android", target_os = "ios"))]
fn update_runtime() -> Result<String, String> {
    Err("local CLI runtime is not available on mobile".to_string())
}

#[tauri::command]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn local_backend_profile() -> Result<LocalBackendProfile, String> {
    let home = Home::from_env().map_err(|err| err.to_string())?;
    Ok(LocalBackendProfile {
        id: "local",
        name: "Local daemon",
        base_url: LOCAL_DAEMON_URL.to_string(),
        token: read_token(&home),
        home: home.root.to_string_lossy().to_string(),
    })
}

#[tauri::command]
#[cfg(target_os = "android")]
fn local_backend_profile() -> Result<LocalBackendProfile, String> {
    Ok(LocalBackendProfile {
        id: "local",
        name: "Android emulator host",
        base_url: ANDROID_EMULATOR_DAEMON_URL.to_string(),
        token: None,
        home: String::new(),
    })
}

#[tauri::command]
#[cfg(target_os = "ios")]
fn local_backend_profile() -> Result<LocalBackendProfile, String> {
    Ok(LocalBackendProfile {
        id: "local",
        name: "Remote daemon",
        base_url: String::new(),
        token: None,
        home: String::new(),
    })
}

#[tauri::command]
async fn check_app_update(
    app: AppHandle,
    pending_update: State<'_, PendingAppUpdate>,
    channel: String,
) -> Result<Option<AppUpdateMetadata>, String> {
    let endpoint = update_endpoint(&channel)?;
    let update = app
        .updater_builder()
        .endpoints(vec![endpoint])
        .map_err(|err| err.to_string())?
        // Channel selection is explicit: switching from nightly back to stable
        // should still surface the selected channel's release, even when its
        // semver is lower than the currently installed nightly build.
        .version_comparator(|current, release| release.version != current)
        .build()
        .map_err(|err| err.to_string())?
        .check()
        .await
        .map_err(|err| err.to_string())?;

    let metadata = update.as_ref().map(|update| AppUpdateMetadata {
        channel,
        current_version: update.current_version.clone(),
        version: update.version.clone(),
    });

    let mut guard = pending_update
        .0
        .lock()
        .map_err(|_| "app update state poisoned".to_string())?;
    *guard = update;
    Ok(metadata)
}

#[tauri::command]
async fn install_app_update(pending_update: State<'_, PendingAppUpdate>) -> Result<(), String> {
    {
        let guard = pending_update
            .0
            .lock()
            .map_err(|_| "app update state poisoned".to_string())?;
        if guard.is_none() {
            return Err("there is no pending update".to_string());
        }
    }

    assert_no_live_runs_before_update().await?;

    let update = {
        let mut guard = pending_update
            .0
            .lock()
            .map_err(|_| "app update state poisoned".to_string())?;
        guard
            .take()
            .ok_or_else(|| "there is no pending update".to_string())?
    };

    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|err| err.to_string())
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
async fn assert_no_live_runs_before_update() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(|| {
        let home = Home::from_env().map_err(|err| err.to_string())?;
        let token = read_token(&home)
            .ok_or_else(|| "cannot verify active runs: missing daemon auth token".to_string())?;
        let recovery = fetch_local_recovery_status(&token)?;
        if recovery.live_runs.is_empty() {
            return Ok(());
        }
        Err(format!(
            "Update blocked: {} active run(s)",
            recovery.live_runs.len()
        ))
    })
    .await
    .map_err(|err| err.to_string())?
}

#[cfg(any(target_os = "android", target_os = "ios"))]
async fn assert_no_live_runs_before_update() -> Result<(), String> {
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(PendingAppUpdate(Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![
            runtime_probe,
            runtime_launch_url,
            local_backend_profile,
            check_app_update,
            install_app_update,
            update_runtime
        ])
        .run(tauri::generate_context!())
        .expect("error while running orgasmic app");
}

fn update_endpoint(channel: &str) -> Result<Url, String> {
    match channel {
        "stable" | "nightly" => {
            let url =
                format!("https://github.com/{UPDATE_REPO}/releases/download/{channel}/latest.json");
            Url::parse(&url).map_err(|err| err.to_string())
        }
        _ => Err(format!("unsupported update channel: {channel}")),
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn discover_cli() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ORGASMIC_CLI").map(PathBuf::from) {
        if is_executable_file(&path) {
            return Some(path);
        }
    }
    if let Ok(home) = Home::from_env() {
        let path = home.bin_orgasmic();
        if is_executable_file(&path) {
            return Some(path);
        }
    }
    find_on_path("orgasmic")
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn command_stdout(cli: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new(cli)
        .args(args)
        .output()
        .map_err(|err| format!("run {} {}: {err}", cli.display(), args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "{} {} failed with {}\n{}",
            cli.display(),
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn find_on_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(binary);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let candidate = dir.join(format!("{binary}.exe"));
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn fetch_local_recovery_status(token: &str) -> Result<LocalRecoveryStatus, String> {
    let mut stream = TcpStream::connect("127.0.0.1:4848")
        .map_err(|err| format!("check active runs: connect to local daemon: {err}"))?;
    let timeout = Some(Duration::from_secs(3));
    stream
        .set_read_timeout(timeout)
        .map_err(|err| format!("check active runs: set read timeout: {err}"))?;
    stream
        .set_write_timeout(timeout)
        .map_err(|err| format!("check active runs: set write timeout: {err}"))?;

    let request = format!(
        "GET /recovery/status HTTP/1.1\r\nHost: 127.0.0.1:4848\r\nAuthorization: Bearer {token}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("check active runs: write request: {err}"))?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|err| format!("check active runs: read response: {err}"))?;
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| "check active runs: malformed daemon response".to_string())?;
    let status_line = headers
        .lines()
        .next()
        .ok_or_else(|| "check active runs: missing daemon status".to_string())?;
    if !status_line.contains(" 200 ") {
        return Err(format!("check active runs failed: {status_line}"));
    }

    serde_json::from_str(body)
        .map_err(|err| format!("check active runs: parse recovery status: {err}"))
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn read_token(home: &Home) -> Option<String> {
    std::fs::read_to_string(home.auth_token())
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|token| !token.is_empty())
}
