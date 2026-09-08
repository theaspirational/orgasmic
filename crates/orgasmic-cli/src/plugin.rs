use crate::{daemon_client::DaemonClient, manager::resolve_project};
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use orgasmic_core::{
    plugin::{validate_id, PluginManifest},
    Home,
};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Subcommand, Debug)]
pub enum PluginCmd {
    /// Install a local plugin folder or shallow-clone a Git URL; does not enable it.
    Add {
        /// Local directory or Git URL containing plugin.org.
        source: String,
    },
    /// Validate an installed manifest and its declared commands.
    Check {
        /// Installed plugin id (or pass --path).
        id: Option<String>,
        /// Validate a development folder without installing it.
        #[arg(long, conflicts_with = "id", required_unless_present = "id")]
        path: Option<PathBuf>,
    },
    /// Enable for one ledger after approving the manifest capabilities (admin only).
    Enable {
        /// Installed plugin id.
        id: String,
        /// Ledger to enable in; defaults to the project resolved from cwd.
        #[arg(long)]
        project: Option<String>,
        /// Approve the displayed capabilities without an interactive prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Disable for one ledger; retained nodes become read-only (admin only).
    Disable {
        /// Installed plugin id.
        id: String,
        /// Ledger to disable in; defaults to the project resolved from cwd.
        #[arg(long)]
        project: Option<String>,
    },
    /// Move the installed folder to recoverable storage; retain ownership and node data.
    Remove {
        /// Installed plugin id to disable everywhere and move to recovery storage.
        id: String,
    },
    /// List installed plugins, activation state and validation errors for a ledger.
    List {
        /// Ledger to inspect; defaults to the project resolved from cwd.
        #[arg(long)]
        project: Option<String>,
    },
    /// Create a minimal disabled plugin under the user plugin directory.
    Scaffold {
        /// New plugin id using lowercase letters, digits and hyphens.
        id: String,
    },
    /// Execute a declared command with a revocable, project-scoped plugin principal.
    Run {
        /// Enabled plugin id.
        id: String,
        /// Executable declared in the manifest COMMANDS field.
        command: String,
        /// Ledger to run against; defaults to the project resolved from cwd.
        #[arg(long)]
        project: Option<String>,
        /// Arguments passed verbatim to the executable after --.
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
}

pub fn cmd_plugin(home: &Home, cmd: PluginCmd) -> Result<()> {
    if let PluginCmd::Check { id, path } = cmd {
        let path = match path {
            Some(path) => path,
            None => {
                let id = id.context("pass id or --path")?;
                validate_id(&id)?;
                home.user().join("plugins").join(id)
            }
        };
        let manifest = PluginManifest::read_dir(&path)?;
        println!("{} {}: ok", manifest.id, manifest.version);
        return Ok(());
    }
    tokio::runtime::Runtime::new()?.block_on(async {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        let plugins = home.user().join("plugins");
        match cmd {
            PluginCmd::List { project } => {
                let project = resolve_project(project)?;
                let result: serde_json::Value = client.get(&crate::path_with_project_query("/plugins", Some(project))).await?;
                crate::print_json(&result)
            }
            PluginCmd::Enable { id, project, yes } => {
                validate_id(&id)?;
                let project = resolve_project(project)?;
                reconcile(&client).await?;
                let manifest = PluginManifest::read_dir(&plugins.join(&id))?;
                anyhow::ensure!(manifest.id == id, "directory name must match plugin id");
                eprintln!("{id} requests: {}", manifest.capabilities.iter().cloned().collect::<Vec<_>>().join(", "));
                eprintln!("Plugin commands run as your OS user with full host filesystem access; daemon calls are capability-scoped.");
                if !yes {
                    anyhow::ensure!(io::stdin().is_terminal(), "approval requires a terminal or --yes");
                    eprint!("Enable for {project}? [y/N] "); io::stderr().flush()?;
                    let mut answer = String::new(); io::stdin().read_line(&mut answer)?;
                    anyhow::ensure!(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"), "not enabled");
                }
                activation(&client, &id, &project, true, manifest.capabilities).await
            }
            PluginCmd::Disable { id, project } => {
                validate_id(&id)?;
                activation(&client, &id, &resolve_project(project)?, false, BTreeSet::new()).await
            }
            PluginCmd::Add { source } => {
                reconcile(&client).await?;
                std::fs::create_dir_all(&plugins)?;
                let temp = tempfile::tempdir_in(&plugins)?;
                let staged = temp.path().join("source");
                if source.contains("://") || source.starts_with("git@") {
                    anyhow::ensure!(Command::new("git").args(["clone", "--depth", "1", "--", &source]).arg(&staged).status()?.success(), "git clone failed");
                } else { copy_tree(Path::new(&source), &staged)?; }
                let manifest = PluginManifest::read_dir(&staged)?;
                let destination = plugins.join(&manifest.id);
                anyhow::ensure!(!destination.exists(), "plugin already installed; remove it before replacing");
                std::fs::rename(staged, &destination)?;
                reconcile(&client).await?;
                println!("added {} {}; enable explicitly to approve capabilities", manifest.id, manifest.version);
                Ok(())
            }
            PluginCmd::Scaffold { id } => {
                validate_id(&id)?;
                reconcile(&client).await?;
                std::fs::create_dir_all(&plugins)?;
                let destination = plugins.join(&id);
                anyhow::ensure!(!destination.exists(), "plugin already exists");
                let temp = tempfile::tempdir_in(&plugins)?;
                let source = format!("* Plugin\n:PROPERTIES:\n:ID: {id}\n:VERSION: 0.1.0\n:PLUGIN_API: 1\n:SCHEMA: 1\n:REQUIRES: core.nodes@1\n:CAPABILITIES: nodes.read nodes.write\n:END:\n** Node type {id}\n:PROPERTIES:\n:COLLECTION: {id}\n:ID_PREFIX: {}-\n:LABEL: {id}\n:LABEL_PLURAL: {id}\n:REQUIRED_PROPERTIES: ID\n:END:\n", id.to_ascii_uppercase());
                PluginManifest::parse(&source, "plugin.org")?;
                std::fs::write(temp.path().join("plugin.org"), source)?;
                std::fs::rename(temp.path(), &destination)?;
                reconcile(&client).await?;
                println!("scaffolded {} (disabled)", destination.display());
                Ok(())
            }
            PluginCmd::Remove { id } => {
                validate_id(&id)?;
                let result: serde_json::Value = client.post_json(&format!("/plugins/{id}/remove"), &serde_json::json!({})).await?;
                crate::print_json(&result)
            }
            PluginCmd::Run { id, command, project, args } => run(home, &client, &id, &command, resolve_project(project)?, args).await,
            PluginCmd::Check { .. } => unreachable!(),
        }
    })
}

async fn reconcile(client: &DaemonClient) -> Result<()> {
    let _: serde_json::Value = client
        .post_json("/plugins/reconcile", &serde_json::json!({}))
        .await?;
    Ok(())
}
async fn activation(
    client: &DaemonClient,
    id: &str,
    project: &str,
    enabled: bool,
    capabilities: BTreeSet<String>,
) -> Result<()> {
    let result: serde_json::Value = client.post_json(&format!("/plugins/{id}/activation"), &serde_json::json!({"project": project, "enabled": enabled, "approved_capabilities": capabilities})).await?;
    crate::print_json(&result)
}
fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    anyhow::ensure!(
        source.is_dir() && !source.symlink_metadata()?.file_type().is_symlink(),
        "source must be a directory, not a symlink"
    );
    std::fs::create_dir(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let target = destination.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), target)?;
        } else {
            bail!("plugin source contains symlink or special file");
        }
    }
    Ok(())
}

async fn run(
    home: &Home,
    client: &DaemonClient,
    id: &str,
    command: &str,
    project: String,
    args: Vec<OsString>,
) -> Result<()> {
    validate_id(id)?;
    let root = home.user().join("plugins").join(id);
    let manifest = PluginManifest::read_dir(&root)?;
    let executable = manifest.command_path(&root, command)?;
    let lease: serde_json::Value = client
        .post_json(
            &format!("/plugins/{id}/run"),
            &serde_json::json!({"project": project, "command": command}),
        )
        .await?;
    let token = lease["token"]
        .as_str()
        .context("missing plugin credential")?;
    let status = Command::new(executable)
        .args(args)
        .env(
            "ORGASMIC_DAEMON_URL",
            client.absolute_url("/").trim_end_matches("/api/"),
        )
        .env("ORGASMIC_DAEMON_TOKEN", token)
        .env("ORGASMIC_PLUGIN_TOKEN", token)
        .env_remove("ORGASMIC_DAEMON_TOKEN_FILE")
        .env_remove("ORGASMIC_TOKEN")
        .env("ORGASMIC_PLUGIN_ID", id)
        .env("ORGASMIC_PROJECT", &project)
        .status();
    let revoked: Result<serde_json::Value> = client
        .post_json(
            "/plugins/run/revoke",
            &serde_json::json!({"lease": lease["lease"]}),
        )
        .await;
    revoked.context("revoke plugin credential")?;
    anyhow::ensure!(
        status.context("start plugin command")?.success(),
        "plugin command failed"
    );
    Ok(())
}
