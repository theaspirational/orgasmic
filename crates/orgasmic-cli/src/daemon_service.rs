//! Persistent OS service adapters for the CLI-owned local daemon lifecycle.
//!
//! `orgasmic serve` stays the foreground debug primitive. This module only
//! renders and installs the user-owned service wrappers that make the same
//! daemon process persistent across login/reboot where the host OS provides a
//! non-admin service owner.
//!
//! orgasmic:dec_2D5BC

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::daemon_runtime;
use crate::home::Home;

const MACOS_LABEL: &str = "orgasmic.daemon";
const MACOS_PLIST_NAME: &str = "orgasmic.daemon.plist";
const SYSTEMD_UNIT_NAME: &str = "orgasmic-daemon.service";
const WINDOWS_TASK_NAME: &str = r"\OrgasmicDaemon";
const WINDOWS_TASK_XML: &str = "orgasmic-daemon-task.xml";
const WINDOWS_WRAPPER: &str = "orgasmic-daemon.cmd";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServiceStart {
    Persistent,
    DetachedFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersistenceStatus {
    pub adapter: &'static str,
    pub installed: bool,
    pub enabled: bool,
    pub detail: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostPlatform {
    Macos,
    Linux,
    Windows,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdapterKind {
    MacosLaunchAgent,
    LinuxSystemdUser,
    LinuxDetachedFallback,
    WindowsScheduledTask,
    GenericDetachedProcess,
}

impl AdapterKind {
    fn name(self) -> &'static str {
        match self {
            Self::MacosLaunchAgent => "macos-launch-agent",
            Self::LinuxSystemdUser => "linux-systemd-user",
            Self::LinuxDetachedFallback => "linux-detached-process",
            Self::WindowsScheduledTask => "windows-scheduled-task",
            Self::GenericDetachedProcess => "generic-detached-process",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServiceSpec {
    exe: PathBuf,
    home: PathBuf,
    cwd: PathBuf,
    stdout: PathBuf,
    stderr: PathBuf,
    path: String,
}

pub(crate) const DAEMON_DRIVER_BINARIES: &[&str] = &["tmux", "rmux"];
pub(crate) const DAEMON_HARNESS_BINARIES: &[&str] = &["claude", "cursor-agent"];

/// PATH baked into generated daemon service definitions so launchd/systemd/task
/// wrappers can resolve harness and driver CLIs.
pub(crate) fn daemon_service_path() -> String {
    if let Some(path) = capture_login_shell_path() {
        return path;
    }
    default_daemon_path()
}

pub(crate) fn binary_resolves_on_path(binary: &str, path: &str) -> bool {
    if Path::new(binary).is_absolute() {
        return Path::new(binary).is_file();
    }
    std::env::split_paths(path).any(|dir| dir.join(binary).is_file())
}

fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn capture_login_shell_path() -> Option<String> {
    #[cfg(not(unix))]
    {
        return None;
    }
    #[cfg(unix)]
    {
        let shell = std::env::var_os("SHELL")?;
        let output = Command::new(&shell)
            .args(["-lc", "printf %s \"$PATH\""])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let path = String::from_utf8(output.stdout).ok()?.trim().to_string();
        if path.is_empty() {
            None
        } else {
            Some(path)
        }
    }
}

fn default_daemon_path() -> String {
    let sep = path_separator();
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.push("/opt/homebrew/bin".to_string());
    }
    if let Some(home) = user_home_dir() {
        dirs.push(
            home.join(".cargo")
                .join("bin")
                .to_string_lossy()
                .into_owned(),
        );
        dirs.push(
            home.join(".local")
                .join("bin")
                .to_string_lossy()
                .into_owned(),
        );
        dirs.push(
            home.join(".npm-global")
                .join("bin")
                .to_string_lossy()
                .into_owned(),
        );
        #[cfg(windows)]
        dirs.push(
            home.join("AppData")
                .join("Roaming")
                .join("npm")
                .to_string_lossy()
                .into_owned(),
        );
    }
    if cfg!(windows) {
        dirs.extend([
            r"C:\Program Files".to_string(),
            r"C:\Windows\System32".to_string(),
        ]);
    } else {
        dirs.extend([
            "/usr/local/bin".to_string(),
            "/usr/bin".to_string(),
            "/bin".to_string(),
            "/usr/sbin".to_string(),
            "/sbin".to_string(),
        ]);
    }
    dirs.join(sep)
}

fn path_separator() -> &'static str {
    if cfg!(windows) {
        ";"
    } else {
        ":"
    }
}

pub(crate) fn persistence_status(home: &Home) -> PersistenceStatus {
    match selected_adapter_kind() {
        AdapterKind::MacosLaunchAgent => macos_status(home),
        AdapterKind::LinuxSystemdUser => linux_systemd_status(home),
        AdapterKind::LinuxDetachedFallback => PersistenceStatus {
            adapter: AdapterKind::LinuxDetachedFallback.name(),
            installed: false,
            enabled: false,
            detail: Some(
                "systemd --user is unavailable; using a detached process for this session"
                    .to_string(),
            ),
        },
        AdapterKind::WindowsScheduledTask => windows_status(home),
        AdapterKind::GenericDetachedProcess => PersistenceStatus {
            adapter: AdapterKind::GenericDetachedProcess.name(),
            installed: false,
            enabled: false,
            detail: Some("no persistent service adapter is available for this OS".to_string()),
        },
    }
}

pub(crate) fn start(home: &Home) -> Result<ServiceStart> {
    match selected_adapter_kind() {
        AdapterKind::MacosLaunchAgent => {
            start_macos_launch_agent(home)?;
            Ok(ServiceStart::Persistent)
        }
        AdapterKind::LinuxSystemdUser => {
            start_linux_systemd(home)?;
            Ok(ServiceStart::Persistent)
        }
        AdapterKind::WindowsScheduledTask => {
            start_windows_scheduled_task(home)?;
            Ok(ServiceStart::Persistent)
        }
        AdapterKind::LinuxDetachedFallback | AdapterKind::GenericDetachedProcess => {
            Ok(ServiceStart::DetachedFallback)
        }
    }
}

pub(crate) fn stop(home: &Home) -> Result<()> {
    match selected_adapter_kind() {
        AdapterKind::MacosLaunchAgent => stop_macos_launch_agent(home),
        AdapterKind::LinuxSystemdUser => stop_linux_systemd(home),
        AdapterKind::WindowsScheduledTask => stop_windows_scheduled_task(home),
        AdapterKind::LinuxDetachedFallback | AdapterKind::GenericDetachedProcess => Ok(()),
    }
}

fn selected_adapter_kind() -> AdapterKind {
    select_adapter_for_host(current_platform(), systemd_user_available())
}

fn select_adapter_for_host(platform: HostPlatform, systemd_available: bool) -> AdapterKind {
    match platform {
        HostPlatform::Macos => AdapterKind::MacosLaunchAgent,
        HostPlatform::Linux if systemd_available => AdapterKind::LinuxSystemdUser,
        HostPlatform::Linux => AdapterKind::LinuxDetachedFallback,
        HostPlatform::Windows => AdapterKind::WindowsScheduledTask,
        HostPlatform::Other => AdapterKind::GenericDetachedProcess,
    }
}

fn current_platform() -> HostPlatform {
    #[cfg(target_os = "macos")]
    {
        HostPlatform::Macos
    }
    #[cfg(target_os = "linux")]
    {
        HostPlatform::Linux
    }
    #[cfg(target_os = "windows")]
    {
        HostPlatform::Windows
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        HostPlatform::Other
    }
}

fn systemd_user_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        command_success("systemctl", &["--user", "show-environment"])
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

fn service_spec(home: &Home) -> Result<ServiceSpec> {
    home.ensure()?;
    std::fs::create_dir_all(home.logs())
        .with_context(|| format!("create {}", home.logs().display()))?;
    let runtime_override = daemon_runtime::active(home)?;
    let exe = match &runtime_override {
        Some(runtime) => runtime.binary.clone(),
        None => std::env::current_exe().context("resolve current executable")?,
    };
    Ok(ServiceSpec {
        exe,
        home: home.root.clone(),
        cwd: runtime_override
            .map(|runtime| runtime.source_checkout)
            .unwrap_or_else(|| {
                if home.source().is_dir() {
                    home.source()
                } else {
                    home.root.clone()
                }
            }),
        stdout: home.logs().join("daemon.out.log"),
        stderr: home.logs().join("daemon.err.log"),
        path: daemon_service_path(),
    })
}

fn start_macos_launch_agent(home: &Home) -> Result<()> {
    let spec = service_spec(home)?;
    let plist = macos_plist_path()?;
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(&plist, render_macos_launch_agent(&spec))
        .with_context(|| format!("write {}", plist.display()))?;
    let uid = current_uid();
    let domain = format!("gui/{uid}");
    let service = format!("{domain}/{MACOS_LABEL}");
    let _ = run_command(
        "launchctl",
        &["bootout", &domain, plist.to_string_lossy().as_ref()],
    );
    run_command("launchctl", &["enable", &service]).context("enable LaunchAgent")?;
    run_command(
        "launchctl",
        &["bootstrap", &domain, plist.to_string_lossy().as_ref()],
    )
    .context("load LaunchAgent")?;
    // The plist is RunAtLoad; bootstrap already starts it. `kickstart -k`
    // immediately kills/restarts that fresh process and races readiness probes.
    Ok(())
}

fn stop_macos_launch_agent(_home: &Home) -> Result<()> {
    let plist = macos_plist_path()?;
    let uid = current_uid();
    let domain = format!("gui/{uid}");
    let service = format!("{domain}/{MACOS_LABEL}");
    let _ = run_command(
        "launchctl",
        &["bootout", &domain, plist.to_string_lossy().as_ref()],
    );
    let _ = run_command("launchctl", &["disable", &service]);
    Ok(())
}

fn macos_status(_home: &Home) -> PersistenceStatus {
    let plist = macos_plist_path().ok();
    let installed = plist.as_ref().map(|path| path.exists()).unwrap_or(false);
    let uid = current_uid();
    let domain = format!("gui/{uid}");
    let disabled = command_output_contains(
        "launchctl",
        &["print-disabled", &domain],
        &format!("\"{MACOS_LABEL}\" => true"),
    );
    let enabled = installed && !disabled;
    PersistenceStatus {
        adapter: AdapterKind::MacosLaunchAgent.name(),
        installed,
        enabled,
        detail: plist.map(|path| format!("LaunchAgent {}", path.display())),
    }
}

fn macos_plist_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is required for LaunchAgent path")?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(MACOS_PLIST_NAME))
}

fn start_linux_systemd(home: &Home) -> Result<()> {
    let spec = service_spec(home)?;
    let unit = linux_systemd_unit_path(home);
    if let Some(parent) = unit.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(&unit, render_linux_systemd_unit(&spec))
        .with_context(|| format!("write {}", unit.display()))?;
    run_command("systemctl", &["--user", "daemon-reload"]).context("reload systemd --user")?;
    run_command(
        "systemctl",
        &["--user", "enable", "--now", SYSTEMD_UNIT_NAME],
    )
    .context("enable/start systemd --user unit")?;
    Ok(())
}

fn stop_linux_systemd(home: &Home) -> Result<()> {
    if linux_systemd_unit_path(home).exists() || systemd_user_available() {
        let _ = run_command(
            "systemctl",
            &["--user", "disable", "--now", SYSTEMD_UNIT_NAME],
        );
        let _ = run_command("systemctl", &["--user", "daemon-reload"]);
    }
    Ok(())
}

fn linux_systemd_status(home: &Home) -> PersistenceStatus {
    let unit = linux_systemd_unit_path(home);
    PersistenceStatus {
        adapter: AdapterKind::LinuxSystemdUser.name(),
        installed: unit.exists(),
        enabled: command_success(
            "systemctl",
            &["--user", "is-enabled", "--quiet", SYSTEMD_UNIT_NAME],
        ),
        detail: Some(format!("systemd user unit {}", unit.display())),
    }
}

fn linux_systemd_unit_path(home: &Home) -> PathBuf {
    linux_systemd_config_dir(home).join(SYSTEMD_UNIT_NAME)
}

fn linux_systemd_config_dir(home: &Home) -> PathBuf {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty())
    {
        return PathBuf::from(config_home).join("systemd").join("user");
    }
    if let Some(user_home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(user_home)
            .join(".config")
            .join("systemd")
            .join("user");
    }
    home.root.join("user").join("systemd")
}

fn start_windows_scheduled_task(home: &Home) -> Result<()> {
    let spec = service_spec(home)?;
    let service_dir = windows_service_dir(home);
    std::fs::create_dir_all(&service_dir)
        .with_context(|| format!("create {}", service_dir.display()))?;
    let wrapper = service_dir.join(WINDOWS_WRAPPER);
    std::fs::write(&wrapper, render_windows_wrapper(&spec))
        .with_context(|| format!("write {}", wrapper.display()))?;
    let xml = service_dir.join(WINDOWS_TASK_XML);
    std::fs::write(&xml, render_windows_scheduled_task(&spec, &wrapper))
        .with_context(|| format!("write {}", xml.display()))?;
    run_command(
        "schtasks",
        &[
            "/Create",
            "/TN",
            WINDOWS_TASK_NAME,
            "/XML",
            xml.to_string_lossy().as_ref(),
            "/F",
        ],
    )
    .context("create non-admin scheduled task")?;
    run_command("schtasks", &["/Run", "/TN", WINDOWS_TASK_NAME]).context("start scheduled task")?;
    Ok(())
}

fn stop_windows_scheduled_task(_home: &Home) -> Result<()> {
    let _ = run_command("schtasks", &["/End", "/TN", WINDOWS_TASK_NAME]);
    let _ = run_command("schtasks", &["/Delete", "/TN", WINDOWS_TASK_NAME, "/F"]);
    Ok(())
}

fn windows_status(home: &Home) -> PersistenceStatus {
    let installed = command_success("schtasks", &["/Query", "/TN", WINDOWS_TASK_NAME]);
    let enabled = if installed {
        command_output_contains(
            "schtasks",
            &["/Query", "/TN", WINDOWS_TASK_NAME, "/FO", "LIST", "/V"],
            "Enabled",
        )
    } else {
        false
    };
    PersistenceStatus {
        adapter: AdapterKind::WindowsScheduledTask.name(),
        installed,
        enabled,
        detail: Some(format!(
            "per-user scheduled task {}; definition cache {}",
            WINDOWS_TASK_NAME,
            windows_service_dir(home).display()
        )),
    }
}

fn windows_service_dir(home: &Home) -> PathBuf {
    home.state().join("service")
}

fn render_macos_launch_agent(spec: &ServiceSpec) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
  <key>Label</key>\n  <string>{label}</string>\n\
  <key>ProgramArguments</key>\n  <array>\n    <string>{exe}</string>\n    <string>serve</string>\n  </array>\n\
  <key>EnvironmentVariables</key>\n  <dict>\n    <key>ORGASMIC_HOME</key>\n    <string>{home}</string>\n    <key>PATH</key>\n    <string>{path}</string>\n  </dict>\n\
  <key>WorkingDirectory</key>\n  <string>{cwd}</string>\n\
  <key>StandardOutPath</key>\n  <string>{stdout}</string>\n\
  <key>StandardErrorPath</key>\n  <string>{stderr}</string>\n\
  <key>RunAtLoad</key>\n  <true/>\n\
  <key>KeepAlive</key>\n  <true/>\n\
</dict>\n\
</plist>\n",
        label = MACOS_LABEL,
        exe = xml_escape_path(&spec.exe),
        home = xml_escape_path(&spec.home),
        path = xml_escape(&spec.path),
        cwd = xml_escape_path(&spec.cwd),
        stdout = xml_escape_path(&spec.stdout),
        stderr = xml_escape_path(&spec.stderr),
    )
}

fn render_linux_systemd_unit(spec: &ServiceSpec) -> String {
    // systemd does not apply uniform quoting across directives. `ExecStart=` is a
    // command line and `Environment=` is a list of assignments — both support and
    // need double-quote escaping for values with spaces. But the single-value path
    // directives (`WorkingDirectory=`, `StandardOutput=append:`, `StandardError=`)
    // take the rest of the line verbatim: a wrapping quote becomes part of the
    // path, so the unit is rejected as "path is not absolute" and never starts.
    // Emit those paths raw (the whole value is the path, so embedded spaces are
    // fine without quoting).
    format!(
        "[Unit]\n\
Description=orgasmic daemon\n\
After=network.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={} serve\n\
WorkingDirectory={}\n\
Environment={}\n\
Environment={}\n\
StandardOutput=append:{}\n\
StandardError=append:{}\n\
Restart=on-failure\n\
RestartSec=2\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        systemd_quote_arg(&path_text(&spec.exe)),
        path_text(&spec.cwd),
        systemd_quote_env("ORGASMIC_HOME", &path_text(&spec.home)),
        systemd_quote_env("PATH", &spec.path),
        path_text(&spec.stdout),
        path_text(&spec.stderr),
    )
}

fn render_windows_scheduled_task(spec: &ServiceSpec, wrapper: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<Task version=\"1.4\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
  <RegistrationInfo>\n    <Description>orgasmic daemon</Description>\n  </RegistrationInfo>\n\
  <Triggers>\n    <LogonTrigger>\n      <Enabled>true</Enabled>\n    </LogonTrigger>\n  </Triggers>\n\
  <Principals>\n    <Principal id=\"Author\">\n      <LogonType>InteractiveToken</LogonType>\n      <RunLevel>LeastPrivilege</RunLevel>\n    </Principal>\n  </Principals>\n\
  <Settings>\n    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n    <AllowHardTerminate>true</AllowHardTerminate>\n    <StartWhenAvailable>true</StartWhenAvailable>\n    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>\n    <Enabled>true</Enabled>\n  </Settings>\n\
  <Actions Context=\"Author\">\n    <Exec>\n      <Command>{wrapper}</Command>\n      <WorkingDirectory>{cwd}</WorkingDirectory>\n    </Exec>\n  </Actions>\n\
</Task>\n",
        wrapper = xml_escape_path(wrapper),
        cwd = xml_escape_path(&spec.cwd),
    )
}

fn render_windows_wrapper(spec: &ServiceSpec) -> String {
    format!(
        "@echo off\r\n\
set \"PATH={}\"\r\n\
set \"ORGASMIC_HOME={}\"\r\n\
cd /d \"{}\"\r\n\
\"{}\" serve >> \"{}\" 2>> \"{}\"\r\n",
        cmd_escape(&spec.path),
        cmd_escape(&path_text(&spec.home)),
        cmd_escape(&path_text(&spec.cwd)),
        cmd_escape(&path_text(&spec.exe)),
        cmd_escape(&path_text(&spec.stdout)),
        cmd_escape(&path_text(&spec.stderr)),
    )
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn xml_escape_path(path: &Path) -> String {
    xml_escape(&path_text(path))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn systemd_quote_env(key: &str, value: &str) -> String {
    systemd_quote_arg(&format!("{key}={value}"))
}

fn systemd_quote_arg(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn cmd_escape(value: &str) -> String {
    value.replace('%', "%%").replace('"', "\"\"")
}

fn run_command(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("run {program}"))?;
    if !status.success() {
        bail!("{program} {:?} failed with {status}", args);
    }
    Ok(())
}

fn command_success(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn command_output_contains(program: &str, args: &[&str], needle: &str) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|stdout| stdout.contains(needle))
        .unwrap_or(false)
}

#[cfg(unix)]
fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec {
            exe: PathBuf::from("/Applications/Orgasmic & Tools/orgasmic"),
            home: PathBuf::from("/Users/tester/Orgasmic Home"),
            cwd: PathBuf::from("/Users/tester/src/orgasmic"),
            stdout: PathBuf::from("/Users/tester/Orgasmic Home/logs/daemon.out.log"),
            stderr: PathBuf::from("/Users/tester/Orgasmic Home/logs/daemon.err.log"),
            path: "/opt/homebrew/bin:/Users/tester/.cargo/bin:/Users/tester/.local/bin:/Users/tester/.npm-global/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
        }
    }

    #[test]
    fn adapter_selection_prefers_native_service_owners() {
        assert_eq!(
            select_adapter_for_host(HostPlatform::Macos, false),
            AdapterKind::MacosLaunchAgent
        );
        assert_eq!(
            select_adapter_for_host(HostPlatform::Linux, true),
            AdapterKind::LinuxSystemdUser
        );
        assert_eq!(
            select_adapter_for_host(HostPlatform::Windows, false),
            AdapterKind::WindowsScheduledTask
        );
    }

    #[test]
    fn linux_selection_reports_detached_fallback_without_systemd_user() {
        assert_eq!(
            select_adapter_for_host(HostPlatform::Linux, false),
            AdapterKind::LinuxDetachedFallback
        );
    }

    #[test]
    fn macos_launch_agent_definition_is_user_owned_and_deterministic() {
        let plist = render_macos_launch_agent(&spec());
        assert!(plist.contains("<string>orgasmic.daemon</string>"));
        assert!(plist.contains("<key>ORGASMIC_HOME</key>"));
        assert!(plist.contains("<key>PATH</key>"));
        assert!(plist.contains("/opt/homebrew/bin"));
        assert!(plist.contains("/Users/tester/.cargo/bin"));
        assert!(plist.contains("/Users/tester/.local/bin"));
        assert!(plist.contains("/Users/tester/.npm-global/bin"));
        assert!(plist.contains("/Users/tester/Orgasmic Home"));
        assert!(plist.contains("daemon.out.log"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("Orgasmic &amp; Tools"));
    }

    #[test]
    fn linux_systemd_unit_definition_is_user_service() {
        let unit = render_linux_systemd_unit(&spec());
        assert!(unit.contains("[Service]"));
        assert!(unit.contains("ExecStart=\"/Applications/Orgasmic & Tools/orgasmic\" serve"));
        assert!(unit.contains("Environment=\"ORGASMIC_HOME=/Users/tester/Orgasmic Home\""));
        assert!(unit.contains("Environment=\"PATH=/opt/homebrew/bin:/Users/tester/.cargo/bin"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains("Restart=on-failure"));
        // Single-value path directives must be raw/unquoted — systemd rejects the
        // unit ("path is not absolute") if the value is wrapped in double quotes.
        assert!(unit.contains("WorkingDirectory=/Users/tester/src/orgasmic\n"));
        assert!(
            unit.contains("StandardOutput=append:/Users/tester/Orgasmic Home/logs/daemon.out.log\n")
        );
        assert!(
            unit.contains("StandardError=append:/Users/tester/Orgasmic Home/logs/daemon.err.log\n")
        );
        assert!(!unit.contains("WorkingDirectory=\""));
        assert!(!unit.contains("append:\""));
    }

    #[test]
    fn windows_scheduled_task_definition_is_non_admin_logon_owned() {
        let wrapper =
            PathBuf::from(r"C:\Users\tester\AppData\Local\orgasmic\service\orgasmic-daemon.cmd");
        let xml = render_windows_scheduled_task(&spec(), &wrapper);
        assert!(xml.contains("encoding=\"UTF-8\""));
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains("orgasmic-daemon.cmd"));
        assert!(xml.contains("<Enabled>true</Enabled>"));
    }

    #[test]
    fn windows_wrapper_sets_home_and_redirects_logs() {
        let wrapper = render_windows_wrapper(&spec());
        assert!(wrapper.contains("set \"PATH=/opt/homebrew/bin:/Users/tester/.cargo/bin"));
        assert!(wrapper.contains("set \"ORGASMIC_HOME=/Users/tester/Orgasmic Home\""));
        assert!(wrapper.contains("orgasmic\" serve >>"));
        assert!(wrapper.contains("daemon.err.log"));
    }

    #[test]
    fn default_daemon_path_includes_user_tool_dirs() {
        let path = default_daemon_path();
        assert!(path.contains("/opt/homebrew/bin") || cfg!(not(target_os = "macos")));
        assert!(path.contains(".cargo/bin"));
        assert!(path.contains(".local/bin"));
        assert!(path.contains(".npm-global/bin"));
        assert!(path.contains("/usr/bin"));
    }

    #[test]
    fn binary_resolves_on_path_checks_each_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("tmux");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!("{}:/usr/bin", tmp.path().display());
        assert!(binary_resolves_on_path("tmux", &path));
        assert!(!binary_resolves_on_path("claude", &path));
    }

    #[test]
    fn service_spec_uses_temporary_local_source_runtime_override() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let checkout = tmp.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("Cargo.toml"), "[workspace]\n").unwrap();
        let binary = checkout.join("target/release/orgasmic");
        if let Some(parent) = binary.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&binary, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        crate::daemon_runtime::set_local_source(&home, &checkout, false).unwrap();

        let spec = service_spec(&home).unwrap();
        assert_eq!(spec.exe, binary.canonicalize().unwrap());
        assert_eq!(spec.cwd, checkout.canonicalize().unwrap());
        assert!(render_macos_launch_agent(&spec).contains("target/release/orgasmic"));
        assert!(render_linux_systemd_unit(&spec).contains("target/release/orgasmic"));
    }
}
