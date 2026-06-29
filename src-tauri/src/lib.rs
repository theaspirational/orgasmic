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

/// Hand the JVM + Android `Context` to `rustls-platform-verifier` before any TLS
/// handshake runs. reqwest's rustls stack — shared by `tauri` and
/// `tauri-plugin-updater` — validates certificates through this verifier, which
/// aborts the whole process ("Expect rustls-platform-verifier to be initialized")
/// if it was never given the Android runtime handles. Called from the Tauri setup
/// hook, before the webview can invoke any networking command. Idempotent: the
/// verifier stores the handles in a process-global `OnceCell`.
#[cfg(target_os = "android")]
fn init_android_cert_verifier() {
    // tao owns the live Activity context (a JNI global ref) and the process
    // JavaVM for the activity's lifetime — the same handles it uses for its own
    // JNI calls. Reached directly because neither tauri nor wry re-export it.
    let Some(ctx) = tao::platform::android::prelude::main_android_context() else {
        eprintln!(
            "rustls-platform-verifier: Android context unavailable at setup; \
             HTTPS requests will abort the process"
        );
        return;
    };

    // SAFETY: `java_vm` is the process JavaVM pointer and `context_jobject` is a
    // JNI global ref to the Activity, both kept alive by tao. We only read them,
    // on the thread we attach below.
    let vm = unsafe { jni::JavaVM::from_raw(ctx.java_vm.cast()) };
    let init = vm.attach_current_thread(|env| {
        let context = unsafe {
            jni::objects::JObject::from_raw(env, ctx.context_jobject as jni::sys::jobject)
        };
        rustls_platform_verifier::android::init_with_env(env, context)
    });
    if let Err(err) = init {
        eprintln!("rustls-platform-verifier: init failed: {err:?}");
    }
}

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
fn update_runtime(channel: String) -> Result<String, String> {
    let cli = discover_cli().ok_or_else(|| "orgasmic CLI is not installed".to_string())?;
    // Keep the runtime on the same channel the app user selected, so switching
    // the app's channel toggle moves both the app and its runtime together.
    command_stdout(&cli, &["update", "--channel", &channel])
}

#[tauri::command]
#[cfg(any(target_os = "android", target_os = "ios"))]
fn update_runtime(_channel: String) -> Result<String, String> {
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AndroidUpdateManifest {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    version_code: Option<u64>,
    #[serde(default)]
    apk_url: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    pub_date: Option<String>,
}

/// Fetch the Android sideload manifest for `channel` from its GitHub release in
/// the app process (no webview CORS). Returns `None` when the release carries no
/// manifest yet (404). The JS side compares versions and drives the prompt.
#[tauri::command]
async fn check_android_update(channel: String) -> Result<Option<AndroidUpdateManifest>, String> {
    let tag = app_release_tag(&channel)?;
    let url = format!("https://github.com/{UPDATE_REPO}/releases/download/{tag}/android-latest.json");
    let response = reqwest::Client::new()
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|err| format!("android update request failed: {err}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(format!("android update request failed: {}", response.status()));
    }
    response
        .json::<AndroidUpdateManifest>()
        .await
        .map(Some)
        .map_err(|err| format!("parse android-latest.json: {err}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_shell::init())
        // Opens external URLs via the platform's default app. On Android this
        // fires an ACTION_VIEW intent (the shell plugin's `open` instead tries
        // to spawn a desktop opener binary and fails with ENOENT), so the
        // sideload APK download must go through this plugin.
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(PendingAppUpdate(Mutex::new(None)))
        .setup(|_app| {
            // Must run before the webview triggers its first HTTPS request.
            #[cfg(target_os = "android")]
            init_android_cert_verifier();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            runtime_probe,
            runtime_launch_url,
            local_backend_profile,
            check_app_update,
            install_app_update,
            update_runtime,
            check_android_update
        ])
        .run(tauri::generate_context!())
        .expect("error while running orgasmic app");
}

fn update_endpoint(channel: &str) -> Result<Url, String> {
    let tag = app_release_tag(channel)?;
    let url = format!("https://github.com/{UPDATE_REPO}/releases/download/{tag}/latest.json");
    Url::parse(&url).map_err(|err| err.to_string())
}

/// Map the user-facing update channel to the APP line's release tag. The app
/// line is namespaced and symmetric — stable -> `apps-stable`, nightly ->
/// `apps-nightly` — so each product line owns its own release tags (the runtime
/// line keeps the bare `stable`/`nightly` tags). App assets no longer share the
/// runtime tags; this mapping is what fixes the old stable-channel 404
/// (app-stable lives in `apps-stable`, never `stable`). dec_B4147.
fn app_release_tag(channel: &str) -> Result<&'static str, String> {
    match channel {
        "stable" => Ok("apps-stable"),
        "nightly" => Ok("apps-nightly"),
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
