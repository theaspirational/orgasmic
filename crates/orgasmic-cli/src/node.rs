use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Deserialize;

use crate::daemon_client::DaemonClient;
use crate::home::Home;
use crate::manager::resolve_project;

/// `--kind` selector for `node body`/`node prop`. Mirrors
/// [`orgasmic_core::NodeKind`] one variant at a time (parity-tested in
/// `node_kind_parity` below) so `--help` lists exactly what the daemon
/// accepts, including `handoff` and `goal` (TASK-JJ9RD).
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum NodeKindArg {
    Decision,
    Glossary,
    Project,
    Task,
    Goal,
    Handoff,
}

impl From<NodeKindArg> for orgasmic_core::NodeKind {
    fn from(value: NodeKindArg) -> Self {
        match value {
            NodeKindArg::Decision => Self::Decision,
            NodeKindArg::Glossary => Self::Glossary,
            NodeKindArg::Project => Self::Project,
            NodeKindArg::Task => Self::Task,
            NodeKindArg::Goal => Self::Goal,
            NodeKindArg::Handoff => Self::Handoff,
        }
    }
}

fn kind_str(kind: Option<NodeKindArg>) -> Option<&'static str> {
    kind.map(|kind| orgasmic_core::NodeKind::from(kind).as_str())
}

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
    // orgasmic:TASK-P0Q5C
    /// Rewrite a node's heading title through the daemon org-node editor.
    Title {
        #[command(subcommand)]
        cmd: NodeTitleCmd,
    },
    /// Submit a complete regenerated node.org replacement.
    Submit {
        /// Existing node id.
        id: String,
        /// Complete replacement node.org file.
        #[arg(long)]
        file: std::path::PathBuf,
        /// Project id; omitted → resolved from the current project.
        #[arg(long)]
        project: Option<String>,
    },
    /// Regenerate a node through its type descriptor's prompt spec.
    Regenerate {
        /// Existing node id.
        id: String,
        /// Project id; omitted → resolved from the current project.
        #[arg(long)]
        project: Option<String>,
        /// Optional guidance appended to the current node and open comments.
        #[arg(long = "extra-prompt")]
        extra_prompt: Option<String>,
        /// Transport mode; required when no live regenerate run exists.
        #[arg(long)]
        mode: Option<String>,
        /// Harness; required when no live regenerate run exists.
        #[arg(long)]
        harness: Option<String>,
        /// Additional harness argument; repeatable.
        #[arg(long = "harness-arg")]
        harness_args: Vec<String>,
        /// Model id passed to the harness; accepted values are the harness's own.
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort passed to the harness, e.g. `low` / `medium` / `high`.
        #[arg(long)]
        effort: Option<String>,
    },
    /// Delete one node through the daemon org-node surface (OCC + tx).
    Delete {
        /// Node id, e.g. `TASK-XXXXX` / `dec_XXXXX` / `term_XXXXX`.
        id: String,
        /// Project id; omitted → resolved from the `.orgasmic/project.org`
        /// above the current directory.
        #[arg(long)]
        project: Option<String>,
        /// Node layer to address (task, decision, glossary, artifact); omitted
        /// → inferred from the id prefix.
        #[arg(long, value_enum)]
        kind: Option<NodeKindArg>,
        /// Optimistic-concurrency token from `org node get` / prior edit; fetched when omitted.
        #[arg(long = "base-version")]
        base_version: Option<String>,
        /// Stable idempotency key. Replaying the same value returns the
        /// original result instead of writing twice.
        #[arg(long = "request-id")]
        request_id: Option<String>,
        /// Print `{id, deleted: true}` instead of the default compact
        /// `{id, changed, tx_id}` mutation response.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum NodeBodyCmd {
    /// Replace a node's free prose body (between drawer and first nested heading).
    Set {
        /// Node id, e.g. `TASK-XXXXX` / `dec_XXXXX` / `term_XXXXX`.
        id: String,
        /// Project id; omitted → resolved from the `.orgasmic/project.org`
        /// above the current directory.
        #[arg(long)]
        project: Option<String>,
        /// Explicit layer selector; see daemon registry for the accepted set.
        #[arg(long, value_enum)]
        kind: Option<NodeKindArg>,
        /// Target a named `**` section instead of the free prose body.
        #[arg(long)]
        section: Option<String>,
        /// Create `--section` when it does not exist yet. Without it a write to
        /// an unknown section is refused by name rather than quietly minting a
        /// heading, which is how a mistyped title used to become permanent.
        #[arg(long, requires = "section")]
        create: bool,
        /// Body text in Org markup, written verbatim.
        #[arg(long, allow_hyphen_values = true)]
        body: String,
        /// Pass body through the raw escape wrapper (TASK-RCP69).
        #[arg(long)]
        raw: bool,
        /// Optimistic-concurrency token from `org node get` / prior edit; fetched when omitted.
        #[arg(long = "base-version")]
        base_version: Option<String>,
        /// Stable idempotency key. Replaying the same value returns the
        /// original result instead of writing twice.
        #[arg(long = "request-id")]
        request_id: Option<String>,
        /// Print the full node document instead of the default compact
        /// `{id, changed, tx_id}` mutation response.
        #[arg(long)]
        json: bool,
    },
    /// Append to a node's free prose body.
    Append {
        /// Node id, e.g. `TASK-XXXXX` / `dec_XXXXX` / `term_XXXXX`.
        id: String,
        /// Project id; omitted → resolved from the `.orgasmic/project.org`
        /// above the current directory.
        #[arg(long)]
        project: Option<String>,
        /// Node layer to address (task, decision, glossary, artifact); omitted
        /// → inferred from the id prefix.
        #[arg(long, value_enum)]
        kind: Option<NodeKindArg>,
        /// Target a named `**` section instead of the free prose body.
        #[arg(long)]
        section: Option<String>,
        /// Body text in Org markup, written verbatim.
        #[arg(long, allow_hyphen_values = true)]
        body: String,
        /// Not supported on append (the existing prose would be re-wrapped); use `set --raw`.
        #[arg(long)]
        raw: bool,
        /// Optimistic-concurrency token from a prior read/edit; fetched when
        /// omitted. The write is refused if the node moved underneath it.
        #[arg(long = "base-version")]
        base_version: Option<String>,
        /// Stable idempotency key. Replaying the same value returns the
        /// original result instead of writing twice.
        #[arg(long = "request-id")]
        request_id: Option<String>,
        /// Print the full node document instead of the default compact
        /// `{id, changed, tx_id}` mutation response.
        #[arg(long)]
        json: bool,
    },
    // orgasmic:TASK-CB6GQ
    /// Remove a named `**` section from a node.
    ///
    /// The inverse of `set --section --create`, and the counterpart to
    /// `prop unset`. `node delete` cannot do this: it addresses whole nodes by
    /// `:ID:`, and a section has none.
    Unset {
        /// Node id, e.g. `TASK-XXXXX` / `dec_XXXXX` / `term_XXXXX`.
        id: String,
        /// Project id; omitted → resolved from the `.orgasmic/project.org`
        /// above the current directory.
        #[arg(long)]
        project: Option<String>,
        /// Explicit layer selector; see daemon registry for the accepted set.
        #[arg(long, value_enum)]
        kind: Option<NodeKindArg>,
        /// Section title to remove. Required: this verb never clears a node's
        /// free prose body by omission.
        #[arg(long)]
        section: String,
        /// Optimistic-concurrency token from a prior read/edit; fetched when omitted.
        #[arg(long = "base-version")]
        base_version: Option<String>,
        /// Stable idempotency key. Replaying the same value returns the
        /// original result instead of writing twice.
        #[arg(long = "request-id")]
        request_id: Option<String>,
        /// Print the full node document instead of the default compact
        /// `{id, changed, tx_id}` mutation response.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum NodePropCmd {
    /// Set (insert or update) one drawer property. Reference-valued keys
    /// (RELATES_TO, GLOSSARY_REFS, MOTIVATED_BY, DEPENDS_ON, IMPLEMENTS,
    /// PARENT) take space-separated node ids, not prose — an unresolvable
    /// token is rejected at write time (use --force to skip the check).
    Set {
        /// Node id, e.g. `TASK-XXXXX` / `dec_XXXXX` / `term_XXXXX`.
        id: String,
        /// Drawer property key. UPPERCASE org drawer spelling — the reader
        /// compares keys byte for byte.
        key: String,
        /// New value for the property, written verbatim.
        value: String,
        /// Project id; omitted → resolved from the `.orgasmic/project.org`
        /// above the current directory.
        #[arg(long)]
        project: Option<String>,
        /// Node layer to address (task, decision, glossary, artifact); omitted
        /// → inferred from the id prefix.
        #[arg(long, value_enum)]
        kind: Option<NodeKindArg>,
        /// Optimistic-concurrency token from a prior read/edit; fetched when
        /// omitted. The write is refused if the node moved underneath it.
        #[arg(long = "base-version")]
        base_version: Option<String>,
        /// Stable idempotency key. Replaying the same value returns the
        /// original result instead of writing twice.
        #[arg(long = "request-id")]
        request_id: Option<String>,
        /// Skip the write-time check that a reference-valued property
        /// resolves to a known node id (for intentional forward references).
        #[arg(long)]
        force: bool,
        /// Print the full node document instead of the default compact
        /// `{id, changed, tx_id}` mutation response.
        #[arg(long)]
        json: bool,
    },
    /// Remove one drawer property.
    Unset {
        /// Node id, e.g. `TASK-XXXXX` / `dec_XXXXX` / `term_XXXXX`.
        id: String,
        /// Drawer property key, in the spelling the drawer actually carries —
        /// removal matches byte for byte, so a legacy `:priority:` line is
        /// removed by `priority`, not by `PRIORITY`.
        key: String,
        /// Project id; omitted → resolved from the `.orgasmic/project.org`
        /// above the current directory.
        #[arg(long)]
        project: Option<String>,
        /// Node layer to address (task, decision, glossary, artifact); omitted
        /// → inferred from the id prefix.
        #[arg(long, value_enum)]
        kind: Option<NodeKindArg>,
        /// Optimistic-concurrency token from a prior read/edit; fetched when
        /// omitted. The write is refused if the node moved underneath it.
        #[arg(long = "base-version")]
        base_version: Option<String>,
        /// Stable idempotency key. Replaying the same value returns the
        /// original result instead of writing twice.
        #[arg(long = "request-id")]
        request_id: Option<String>,
        /// Print the full node document instead of the default compact
        /// `{id, changed, tx_id}` mutation response.
        #[arg(long)]
        json: bool,
    },
}

// orgasmic:TASK-P0Q5C
#[derive(Subcommand, Debug)]
pub enum NodeTitleCmd {
    /// Rewrite the heading's title prose for a decision, glossary, handoff
    /// or task node. The lifecycle keyword and the org tags on the same line
    /// are preserved; a title Org cannot store verbatim is refused with the
    /// reason (same guard as `task update --title`). Goal titles are refused:
    /// they have no daemon write path yet (TASK-V460X). Project titles are
    /// refused: the daemon rewrite would drop the `PROJECT` heading word that
    /// project.org is located by.
    Set {
        /// Node id, e.g. `TASK-XXXXX` / `dec_XXXXX` / `term_XXXXX` /
        /// `handoff-current`.
        id: String,
        /// New title prose, written verbatim after the id token.
        #[arg(long)]
        title: String,
        /// Project id; omitted → resolved from the `.orgasmic/project.org`
        /// above the current directory.
        #[arg(long)]
        project: Option<String>,
        /// Node layer to address; omitted → inferred from the id prefix.
        #[arg(long, value_enum)]
        kind: Option<NodeKindArg>,
        /// Optimistic-concurrency token from a prior read/edit; fetched when
        /// omitted. The write is refused if the node moved underneath it.
        #[arg(long = "base-version")]
        base_version: Option<String>,
        /// Stable idempotency key. Replaying the same value returns the
        /// original result instead of writing twice.
        #[arg(long = "request-id")]
        request_id: Option<String>,
        /// Print the full node document instead of the default compact
        /// `{id, changed, tx_id}` mutation response.
        #[arg(long)]
        json: bool,
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
    // orgasmic:task_ZYWZD
    /// Nested sub-sections the daemon now reports (TASK-ZYWZD). `body` is only
    /// the prose above them, so an append that ignored these would land in the
    /// middle of the section it claimed to append to.
    #[serde(default)]
    sections: Vec<NodeSection>,
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
                    create,
                    body,
                    raw,
                    base_version,
                    request_id,
                    json,
                } => {
                    let (base_version, project) =
                        resolve_base_version(&client, project, &id, kind_str(kind), base_version)
                            .await?;
                    let body_format = if raw { "raw" } else { "default" };
                    let op = body_op(section.as_deref(), &body, body_format, create);
                    let response: serde_json::Value = client
                        .post_json(
                            &edit_path(&id, json),
                            &edit_request(
                                &project,
                                kind_str(kind),
                                &base_version,
                                &request_id,
                                op,
                                false,
                            ),
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
                    json,
                } => {
                    if raw {
                        anyhow::bail!(
                            "--raw is not supported with `append`: the edit replaces the whole body, so the existing prose would be re-wrapped into a literal block too; compose the full body and use `set --raw` instead"
                        );
                    }
                    let project = Some(resolve_project(project)?);
                    let doc: NodeDoc = client
                        .get(&node_get_path(&id, project.as_deref(), kind_str(kind)))
                        .await?;
                    let base_version = base_version
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or_else(|| doc.source.base_version.clone());
                    let mut merged = append_base_body(&doc, section.as_deref(), &id)?;
                    if !merged.is_empty() && !merged.ends_with('\n') {
                        merged.push('\n');
                    }
                    merged.push_str(&body);
                    let op = body_op(section.as_deref(), &merged, "default", false);
                    let response: serde_json::Value = client
                        .post_json(
                            &edit_path(&id, json),
                            &edit_request(
                                &project,
                                kind_str(kind),
                                &base_version,
                                &request_id,
                                op,
                                false,
                            ),
                        )
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
                NodeBodyCmd::Unset {
                    id,
                    project,
                    kind,
                    section,
                    base_version,
                    request_id,
                    json,
                } => {
                    let (base_version, project) =
                        resolve_base_version(&client, project, &id, kind_str(kind), base_version)
                            .await?;
                    let response: serde_json::Value = client
                        .post_json(
                            &edit_path(&id, json),
                            &edit_request(
                                &project,
                                kind_str(kind),
                                &base_version,
                                &request_id,
                                remove_section_op(&section),
                                false,
                            ),
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
                    force,
                    json,
                } => {
                    let (base_version, project) =
                        resolve_base_version(&client, project, &id, kind_str(kind), base_version)
                            .await?;
                    let op = serde_json::json!({ "op": "set_property", "key": key, "value": value });
                    let response: serde_json::Value = client
                        .post_json(
                            &edit_path(&id, json),
                            &edit_request(
                                &project,
                                kind_str(kind),
                                &base_version,
                                &request_id,
                                op,
                                force,
                            ),
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
                    json,
                } => {
                    let (base_version, project) =
                        resolve_base_version(&client, project, &id, kind_str(kind), base_version)
                            .await?;
                    let op = serde_json::json!({ "op": "remove_property", "key": key });
                    let response: serde_json::Value = client
                        .post_json(
                            &edit_path(&id, json),
                            &edit_request(
                                &project,
                                kind_str(kind),
                                &base_version,
                                &request_id,
                                op,
                                false,
                            ),
                        )
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
            },
            NodeCmd::Title {
                cmd:
                    NodeTitleCmd::Set {
                        id,
                        title,
                        project,
                        kind,
                        base_version,
                        request_id,
                        json,
                    },
            } => {
                // orgasmic:TASK-P0Q5C
                // The daemon's /org/node editor refuses goal nodes wholesale;
                // say up front that no supported tooling can retitle one yet.
                if matches!(kind, Some(NodeKindArg::Goal))
                    || (kind.is_none() && id.starts_with("goal-"))
                {
                    anyhow::bail!(
                        "goal titles cannot be set through `node title set`: goal nodes have no \
                         daemon title write path yet (TASK-V460X); use `goal set` to replace the \
                         active goal"
                    );
                }
                // The daemon's set_title composes `<id> <title>` from the
                // drawer id, so `* PROJECT <id>` becomes `* <id> <title>` —
                // and `ProjectFile::from_org` locates project.org by the
                // `PROJECT ` heading prefix, so every cwd project resolution
                // would fail afterwards. Refuse until the daemon keeps the word.
                if matches!(kind, Some(NodeKindArg::Project)) {
                    anyhow::bail!(
                        "project titles cannot be set through `node title set`: the daemon \
                         rewrite would drop the `PROJECT` heading word that project.org is \
                         located by (ProjectFile::from_org), breaking project resolution; \
                         no daemon write path keeps it yet"
                    );
                }
                let response = set_node_title(
                    &client,
                    NodeTitleWrite {
                        id: &id,
                        project,
                        kind: kind_str(kind),
                        title: &title,
                        base_version,
                        request_id,
                        json,
                    },
                )
                .await?;
                println!("{}", serde_json::to_string_pretty(&response)?);
            }
            NodeCmd::Submit { id, file, project } => {
                let project = resolve_project(project)?;
                let content = std::fs::read_to_string(&file)
                    .with_context(|| format!("read regenerated node {}", file.display()))?;
                let response: serde_json::Value = client
                    .post_json(
                        &format!("/org/node/{id}/submit?project={project}"),
                        &serde_json::json!({ "content": content }),
                    )
                    .await?;
                println!("{}", serde_json::to_string_pretty(&response)?);
            }
            NodeCmd::Regenerate {
                id,
                project,
                extra_prompt,
                mode,
                harness,
                harness_args,
                model,
                effort,
            } => {
                let project = resolve_project(project)?;
                let response: serde_json::Value = client
                    .post_json(
                        &format!("/org/node/{id}/regenerate?project={project}"),
                        &serde_json::json!({
                            "extraPrompt": extra_prompt,
                            "mode": mode,
                            "harness": harness,
                            "harness_args": harness_args,
                            "model": model,
                            "effort": effort,
                        }),
                    )
                    .await?;
                println!("{}", serde_json::to_string_pretty(&response)?);
            }
            NodeCmd::Delete {
                id,
                project,
                kind,
                base_version,
                request_id,
                json,
            } => {
                // orgasmic:TASK-N4TGD
                let (base_version, project) =
                    resolve_base_version(&client, project, &id, kind_str(kind), base_version)
                        .await?;
                let path = if json {
                    format!("/org/node/{id}/delete?json=true")
                } else {
                    format!("/org/node/{id}/delete")
                };
                let response: serde_json::Value = client
                    .post_json(
                        &path,
                        &serde_json::json!({
                            "project": project,
                            "kind": kind_str(kind),
                            "base_version": base_version,
                            "request_id": request_id,
                        }),
                    )
                    .await?;
                println!("{}", serde_json::to_string_pretty(&response)?);
            }
        }
        Ok::<(), anyhow::Error>(())
    })
}

// orgasmic:task_ZYWZD
/// The prose `append` reads, extends, and writes back.
///
/// `append` re-submits the whole prose span, so it can only be honest when that
/// span is the whole of what it claims to append to. A section carrying nested
/// sub-headings is refused by name: appending would silently insert the new
/// text *above* those sub-sections rather than at the end of the section.
fn append_base_body(doc: &NodeDoc, section: Option<&str>, id: &str) -> Result<String> {
    let Some(title) = section else {
        return Ok(doc.body.clone());
    };
    let target = doc
        .sections
        .iter()
        .find(|candidate| candidate.title == title)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "node {id} has no section {title:?}; sections: {:?} (use `set --section` to create one via add)",
                doc.sections
                    .iter()
                    .map(|candidate| candidate.title.as_str())
                    .collect::<Vec<_>>()
            )
        })?;
    if !target.sections.is_empty() {
        let nested: Vec<&str> = target
            .sections
            .iter()
            .map(|nested| nested.title.as_str())
            .collect();
        anyhow::bail!(
            "section {title:?} of node {id} has {} nested sub-section(s) ({}); \
             `append` re-writes the prose above them, so the appended text would land \
             before them rather than at the end of the section — refusing rather than \
             writing it somewhere you did not ask for. Read the node, compose the full \
             section, and use `node body set --section {title:?}` (with `===` sub-headings)",
            nested.len(),
            nested.join(", "),
        );
    }
    Ok(target.body.clone())
}

// orgasmic:task_XPYRR
/// Rewrite one node's heading title through the org-node editor — the same
/// OCC token, structural guard and tx every other node write goes through.
///
/// Lives here rather than in the task command because the edit is a node edit;
/// `task update --title` is only the task-shaped name for it. The `set_title`
/// op writes the title prose alone: the lifecycle keyword and the org tags on
/// the same heading line are preserved by the rewriter, not restated here.
pub struct NodeTitleWrite<'a> {
    pub id: &'a str,
    pub project: Option<String>,
    pub kind: Option<&'a str>,
    pub title: &'a str,
    /// OCC token; fetched from the node document when omitted.
    pub base_version: Option<String>,
    pub request_id: Option<String>,
    /// Print the full node document instead of the compact mutation response.
    pub json: bool,
}

pub async fn set_node_title(
    client: &DaemonClient,
    req: NodeTitleWrite<'_>,
) -> Result<serde_json::Value> {
    let (base_version, project) =
        resolve_base_version(client, req.project, req.id, req.kind, req.base_version).await?;
    let op = serde_json::json!({ "op": "set_title", "title": req.title });
    client
        .post_json(
            &edit_path(req.id, req.json),
            &edit_request(
                &project,
                req.kind,
                &base_version,
                &req.request_id,
                op,
                false,
            ),
        )
        .await
}

fn edit_path(id: &str, want_full: bool) -> String {
    if want_full {
        format!("/org/node/{id}/edit?json=true")
    } else {
        format!("/org/node/{id}/edit")
    }
}

// orgasmic:TASK-CB6GQ — `create` picks `add_section` over `set_section_body`.
// The daemon has always had both ops; it used to run them through one code path
// that appended whenever the section was missing, so every edit could create.
fn body_op(
    section: Option<&str>,
    body: &str,
    body_format: &str,
    create: bool,
) -> serde_json::Value {
    match section {
        None => serde_json::json!({
            "op": "set_body",
            "body": body,
            "body_format": body_format,
        }),
        Some(title) => serde_json::json!({
            "op": if create { "add_section" } else { "set_section_body" },
            "title": title,
            "body": body,
            "body_format": body_format,
        }),
    }
}

// orgasmic:TASK-CB6GQ
fn remove_section_op(title: &str) -> serde_json::Value {
    serde_json::json!({ "op": "remove_section", "title": title })
}

fn edit_request(
    project: &Option<String>,
    kind: Option<&str>,
    base_version: &str,
    request_id: &Option<String>,
    op: serde_json::Value,
    force: bool,
) -> serde_json::Value {
    serde_json::json!({
        "project": project,
        "kind": kind,
        "base_version": base_version,
        "request_id": request_id,
        "ops": [op],
        "force": force,
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

// orgasmic:task_ZYWZD
#[cfg(test)]
mod append_round_trip {
    use super::*;

    fn doc(sections: Vec<NodeSection>) -> NodeDoc {
        NodeDoc {
            body: "Node prose.".to_string(),
            sections,
            source: NodeSource {
                base_version: "0000000000000000".to_string(),
            },
        }
    }

    fn section(title: &str, body: &str, nested: Vec<NodeSection>) -> NodeSection {
        NodeSection {
            title: title.to_string(),
            body: body.to_string(),
            sections: nested,
        }
    }

    #[test]
    fn append_reads_the_prose_it_will_write_back() {
        let doc = doc(vec![section("Description", "Existing prose.", Vec::new())]);
        assert_eq!(
            append_base_body(&doc, None, "TASK-001").unwrap(),
            "Node prose."
        );
        assert_eq!(
            append_base_body(&doc, Some("Description"), "TASK-001").unwrap(),
            "Existing prose."
        );
    }

    /// `append` re-submits the section's prose span. When the section also
    /// carries sub-sections, that span is not the end of the section, so the
    /// append is refused by name instead of landing above them.
    #[test]
    fn append_refuses_a_section_with_nested_sub_sections_naming_them() {
        let doc = doc(vec![section(
            "Description",
            "Lead prose.",
            vec![
                section("The gap", "Detail.", Vec::new()),
                section("Ask", "More.", Vec::new()),
            ],
        )]);
        let err = append_base_body(&doc, Some("Description"), "TASK-001")
            .unwrap_err()
            .to_string();
        assert!(err.contains("The gap") && err.contains("Ask"), "{err}");
        assert!(err.contains('2'), "must name how many: {err}");
        assert!(err.contains("node body set --section"), "{err}");
    }

    #[test]
    fn append_to_a_missing_section_still_lists_what_exists() {
        let doc = doc(vec![section("Description", "Prose.", Vec::new())]);
        let err = append_base_body(&doc, Some("Evidence"), "TASK-001")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("has no section") && err.contains("Description"),
            "{err}"
        );
    }
}

#[cfg(test)]
mod node_kind_parity {
    use super::NodeKindArg;
    use clap::ValueEnum;
    use std::collections::BTreeSet;

    /// Anti-drift guarantee (TASK-JJ9RD): the CLI `--kind` enum must offer
    /// exactly the kinds the daemon accepts (`orgasmic_daemon::api::
    /// accepted_node_kinds`). Core keeps historical kinds parseable even when
    /// they are no longer accepted by the daemon.
    #[test]
    fn cli_kind_arg_matches_daemon_registry() {
        let cli_kinds: BTreeSet<&str> = NodeKindArg::value_variants()
            .iter()
            .map(|arg| orgasmic_core::NodeKind::from(*arg).as_str())
            .collect();
        let daemon_kinds: BTreeSet<&str> = orgasmic_daemon::api::accepted_node_kinds()
            .iter()
            .map(|kind| kind.as_str())
            .collect();
        assert_eq!(
            cli_kinds, daemon_kinds,
            "CLI --kind enum and daemon-accepted kinds drifted apart"
        );
    }
}

// orgasmic:TASK-CB6GQ
#[cfg(test)]
mod node_edit_op_parity {
    use std::collections::BTreeSet;

    /// Ops the daemon accepts that no `orgasmic node` verb can ask for, each
    /// with the reason it is unreachable on purpose.
    ///
    /// Empty, and meant to stay that way. `remove_section` sat outside this
    /// list's absence for as long as the enum has had it: implemented in
    /// `orgasmic-core`, dispatched by the daemon, routed over HTTP, and
    /// unreachable from the CLI — so a section created by a mistyped
    /// `--section` could never be removed by supported tooling. Nothing
    /// compared the two surfaces, so nothing noticed until a dead `Description`
    /// stub on `handoff-current` had to be left in place.
    const DELIBERATELY_NOT_EXPOSED: &[&str] = &[];

    /// Every `op` tag some `orgasmic node` verb emits.
    fn cli_emitted_ops() -> BTreeSet<&'static str> {
        BTreeSet::from([
            // node body set
            "set_body",
            "set_section_body",
            // node body set --section --create
            "add_section",
            // node body unset --section
            "remove_section",
            // node prop set / unset
            "set_property",
            "remove_property",
            // node title set / task update --title / --tag
            "set_title",
            "set_tags",
        ])
    }

    #[test]
    fn every_daemon_edit_op_is_reachable_from_the_cli() {
        let daemon: BTreeSet<&str> = orgasmic_daemon::api::NODE_EDIT_OPS
            .iter()
            .copied()
            .collect();
        let cli = cli_emitted_ops();
        let excused: BTreeSet<&str> = DELIBERATELY_NOT_EXPOSED.iter().copied().collect();

        let unreachable: Vec<&str> = daemon.difference(&cli).copied().collect();
        let unreachable: Vec<&str> = unreachable
            .into_iter()
            .filter(|op| !excused.contains(op))
            .collect();
        assert!(
            unreachable.is_empty(),
            "the daemon accepts these edit ops but no CLI verb can ask for them, \
             so anything they do is unreachable by supported tooling: {unreachable:?}. \
             Add a verb, or list the op in DELIBERATELY_NOT_EXPOSED with its reason."
        );

        let invented: Vec<&str> = cli.difference(&daemon).copied().collect();
        assert!(
            invented.is_empty(),
            "the CLI claims to emit ops the daemon does not accept: {invented:?}"
        );
    }
}
