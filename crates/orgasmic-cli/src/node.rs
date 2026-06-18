use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Deserialize;

use crate::daemon_client::DaemonClient;
use crate::home::Home;
use crate::manager::resolve_project;

#[derive(Subcommand, Debug)]
pub enum NodeCmd {
    /// Read/write node bodies through the daemon org-node editor.
    Body {
        #[command(subcommand)]
        cmd: NodeBodyCmd,
    },
    /// Read/write node drawer properties through the daemon org-node editor.
    Prop {
        #[command(subcommand)]
        cmd: NodePropCmd,
    },
}

#[derive(Subcommand, Debug)]
pub enum NodeBodyCmd {
    /// Replace a node's free prose body (between drawer and first nested heading).
    Set {
        id: String,
        #[arg(long)]
        project: Option<String>,
        /// Explicit layer (`decision`, `architecture`, `glossary`, `project`, `task`).
        #[arg(long)]
        kind: Option<String>,
        /// Target a named `**` section instead of the free prose body.
        #[arg(long)]
        section: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        body: String,
        /// Pass body through the raw escape wrapper (TASK-RCP69).
        #[arg(long)]
        raw: bool,
        /// Optimistic-concurrency token from `org node get` / prior edit; fetched when omitted.
        #[arg(long = "base-version")]
        base_version: Option<String>,
        #[arg(long = "request-id")]
        request_id: Option<String>,
    },
    /// Append to a node's free prose body.
    Append {
        id: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        /// Target a named `**` section instead of the free prose body.
        #[arg(long)]
        section: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        body: String,
        /// Not supported on append (the existing prose would be re-wrapped); use `set --raw`.
        #[arg(long)]
        raw: bool,
        #[arg(long = "base-version")]
        base_version: Option<String>,
        #[arg(long = "request-id")]
        request_id: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum NodePropCmd {
    /// Set (insert or update) one drawer property.
    Set {
        id: String,
        key: String,
        value: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long = "base-version")]
        base_version: Option<String>,
        #[arg(long = "request-id")]
        request_id: Option<String>,
    },
    /// Remove one drawer property.
    Unset {
        id: String,
        key: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long = "base-version")]
        base_version: Option<String>,
        #[arg(long = "request-id")]
        request_id: Option<String>,
    },
}

#[derive(Deserialize)]
struct NodeDoc {
    body: String,
    #[serde(default)]
    sections: Vec<NodeSection>,
    source: NodeSource,
}

#[derive(Deserialize)]
struct NodeSection {
    title: String,
    body: String,
}

#[derive(Deserialize)]
struct NodeSource {
    base_version: String,
}

pub fn cmd_node(home: &Home, cmd: NodeCmd) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        match cmd {
            NodeCmd::Body { cmd } => match cmd {
                NodeBodyCmd::Set {
                    id,
                    project,
                    kind,
                    section,
                    body,
                    raw,
                    base_version,
                    request_id,
                } => {
                    let (base_version, project) =
                        resolve_base_version(&client, project, &id, kind.as_deref(), base_version)
                            .await?;
                    let body_format = if raw { "raw" } else { "default" };
                    let op = body_op(section.as_deref(), &body, body_format);
                    let response: serde_json::Value = client
                        .post_json(
                            &format!("/org/node/{id}/edit"),
                            &edit_request(&project, kind.as_deref(), &base_version, &request_id, op),
                        )
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
                NodeBodyCmd::Append {
                    id,
                    project,
                    kind,
                    section,
                    body,
                    raw,
                    base_version,
                    request_id,
                } => {
                    if raw {
                        anyhow::bail!(
                            "--raw is not supported with `append`: the edit replaces the whole body, so the existing prose would be re-wrapped into a literal block too; compose the full body and use `set --raw` instead"
                        );
                    }
                    let project = Some(resolve_project(project)?);
                    let doc: NodeDoc = client
                        .get(&node_get_path(&id, project.as_deref(), kind.as_deref()))
                        .await?;
                    let base_version = base_version
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or(doc.source.base_version);
                    let existing = match section.as_deref() {
                        None => doc.body,
                        Some(title) => doc
                            .sections
                            .iter()
                            .find(|candidate| candidate.title == title)
                            .map(|candidate| candidate.body.clone())
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "node {id} has no section {title:?}; sections: {:?} (use `set --section` to create one via add)",
                                    doc.sections
                                        .iter()
                                        .map(|candidate| candidate.title.as_str())
                                        .collect::<Vec<_>>()
                                )
                            })?,
                    };
                    let mut merged = existing;
                    if !merged.is_empty() && !merged.ends_with('\n') {
                        merged.push('\n');
                    }
                    merged.push_str(&body);
                    let op = body_op(section.as_deref(), &merged, "default");
                    let response: serde_json::Value = client
                        .post_json(
                            &format!("/org/node/{id}/edit"),
                            &edit_request(&project, kind.as_deref(), &base_version, &request_id, op),
                        )
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
            },
            NodeCmd::Prop { cmd } => match cmd {
                NodePropCmd::Set {
                    id,
                    key,
                    value,
                    project,
                    kind,
                    base_version,
                    request_id,
                } => {
                    let (base_version, project) =
                        resolve_base_version(&client, project, &id, kind.as_deref(), base_version)
                            .await?;
                    let op = serde_json::json!({ "op": "set_property", "key": key, "value": value });
                    let response: serde_json::Value = client
                        .post_json(
                            &format!("/org/node/{id}/edit"),
                            &edit_request(&project, kind.as_deref(), &base_version, &request_id, op),
                        )
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
                NodePropCmd::Unset {
                    id,
                    key,
                    project,
                    kind,
                    base_version,
                    request_id,
                } => {
                    let (base_version, project) =
                        resolve_base_version(&client, project, &id, kind.as_deref(), base_version)
                            .await?;
                    let op = serde_json::json!({ "op": "remove_property", "key": key });
                    let response: serde_json::Value = client
                        .post_json(
                            &format!("/org/node/{id}/edit"),
                            &edit_request(&project, kind.as_deref(), &base_version, &request_id, op),
                        )
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
            },
        }
        Ok::<(), anyhow::Error>(())
    })
}

fn body_op(section: Option<&str>, body: &str, body_format: &str) -> serde_json::Value {
    match section {
        None => serde_json::json!({
            "op": "set_body",
            "body": body,
            "body_format": body_format,
        }),
        Some(title) => serde_json::json!({
            "op": "set_section_body",
            "title": title,
            "body": body,
            "body_format": body_format,
        }),
    }
}

fn edit_request(
    project: &Option<String>,
    kind: Option<&str>,
    base_version: &str,
    request_id: &Option<String>,
    op: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "project": project,
        "kind": kind,
        "base_version": base_version,
        "request_id": request_id,
        "ops": [op],
    })
}

async fn resolve_base_version(
    client: &DaemonClient,
    project: Option<String>,
    id: &str,
    kind: Option<&str>,
    base_version: Option<String>,
) -> Result<(String, Option<String>)> {
    let project = resolve_project(project)?;
    if let Some(base_version) = base_version.filter(|value| !value.trim().is_empty()) {
        return Ok((base_version, Some(project)));
    }
    let doc: NodeDoc = client
        .get(&node_get_path(id, Some(project.as_str()), kind))
        .await?;
    Ok((doc.source.base_version, Some(project)))
}

fn node_get_path(id: &str, project: Option<&str>, kind: Option<&str>) -> String {
    let mut path = format!("/org/node?id={id}");
    if let Some(project) = project.filter(|value| !value.is_empty()) {
        path.push_str("&project=");
        path.push_str(project);
    }
    if let Some(kind) = kind.filter(|value| !value.is_empty()) {
        path.push_str("&kind=");
        path.push_str(kind);
    }
    path
}
