// orgasmic:arch_ARSPJ
//! CLI verbs for the artifact store (TASK-ZEFEY).
//!
//! - `artifact blocks`           — list the block vocabulary
//! - `artifact submit <id>`      — submit MDX; validates block registry
//! - `artifact feedback <id>`    — add a comment, or --consume a CID

use anyhow::Result;
use clap::Subcommand;
use orgasmic_daemon::BLOCK_TYPES;

use crate::daemon_client::DaemonClient;
use crate::home::Home;

/// Where the full per-block shapes and the raw-text conventions (opposite
/// rules for `Code`'s `code={`...`}` attribute vs. `Wireframe`/`Mermaid`'s
/// children) are authored — the source `artifact blocks --full` points at
/// rather than duplicating (TASK-SPBTA), so the two can't drift apart.
pub(crate) const BLOCK_CONTRACT_SPEC_PATH: &str =
    "shipped/prompt-studio/prompt-specs/artifact-generator.org";
/// Fixture exercising all 22 registered block types with real shapes.
pub(crate) const BLOCK_CONTRACT_FIXTURE_PATH: &str =
    "ui/src/lib/artifacts/__fixtures__/all-blocks.ts";

#[derive(Subcommand, Debug)]
pub enum ArtifactCmd {
    /// List the Agent-Native block vocabulary accepted in artifact.mdx.
    Blocks {
        /// Also print per-block shapes and the raw-text conventions (or, if
        /// not inlined, where they're authoritatively documented).
        #[arg(long)]
        full: bool,
    },
    /// Submit (create or update) an artifact from an MDX file.
    ///
    /// Block contract: `orgasmic artifact blocks --full` (or read
    /// shipped/prompt-studio/prompt-specs/artifact-generator.org directly).
    Submit {
        /// Artifact id: ART-<5-char-Crockford-stem> (e.g. ART-XYZAB). Mint a
        /// fresh one with `orgasmic id mint --class artifact`.
        id: String,
        /// Path to the MDX file to submit.
        #[arg(long)]
        file: std::path::PathBuf,
        /// Project id.
        #[arg(long)]
        project: Option<String>,
        /// Artifact title (required for first submit).
        #[arg(long)]
        title: Option<String>,
        /// Space-separated subject node ids (e.g. arch_ARSPJ arch_C87Z9).
        #[arg(long)]
        subject_nodes: Option<String>,
        /// Prompt text for the artifact.
        #[arg(long)]
        prompt: Option<String>,
    },
    /// Add feedback to an artifact, or consume (resolve) an existing comment.
    Feedback {
        /// Artifact id (e.g. ART-XYZAB).
        id: String,
        /// Project id.
        #[arg(long)]
        project: Option<String>,
        /// Consume (resolve + mark consumed) the comment with this CID.
        #[arg(long)]
        consume: Option<String>,
        /// Feedback message (required when not using --consume).
        #[arg(long)]
        message: Option<String>,
        /// JSON anchor object (default: {}).
        #[arg(long, default_value = "{}")]
        anchor: String,
        /// CID this comment resolves (optional).
        #[arg(long)]
        resolution_target: Option<String>,
    },
}

pub fn cmd_artifact(home: &Home, cmd: ArtifactCmd) -> Result<()> {
    match cmd {
        ArtifactCmd::Blocks { full } => cmd_blocks(full),
        ArtifactCmd::Submit {
            id,
            file,
            project,
            title,
            subject_nodes,
            prompt,
        } => cmd_submit(home, id, file, project, title, subject_nodes, prompt),
        ArtifactCmd::Feedback {
            id,
            project,
            consume,
            message,
            anchor,
            resolution_target,
        } => cmd_feedback(
            home,
            id,
            project,
            consume,
            message,
            anchor,
            resolution_target,
        ),
    }
}

fn cmd_blocks(full: bool) -> Result<()> {
    println!("Agent-Native block types ({} total):", BLOCK_TYPES.len());
    for ty in BLOCK_TYPES {
        println!("  <{ty}>");
    }
    if full {
        println!();
        println!("Per-block shapes, attributes, and the raw-text conventions (the");
        println!("opposite rules for Code's `code={{`...`}}` attribute vs.");
        println!("Wireframe/Mermaid/SequenceDiagram/FlowChart's children) are the");
        println!("same contract the artifact generator prompt reads from — not");
        println!("duplicated here so the two can't drift apart:");
        println!("  {BLOCK_CONTRACT_SPEC_PATH}");
        println!("  {BLOCK_CONTRACT_FIXTURE_PATH}");
    }
    Ok(())
}

fn cmd_submit(
    home: &Home,
    id: String,
    file: std::path::PathBuf,
    project: Option<String>,
    title: Option<String>,
    subject_nodes: Option<String>,
    prompt: Option<String>,
) -> Result<()> {
    let content = std::fs::read_to_string(&file)
        .map_err(|e| anyhow::anyhow!("read MDX file {}: {e}", file.display()))?;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        let project_id =
            resolve_project(home, project.as_deref()).await?;

        let subject_nodes_vec: Vec<String> = subject_nodes
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .collect();

        let body = serde_json::json!({
            "content": content,
            "title": title,
            "subject_nodes": if subject_nodes_vec.is_empty() { None } else { Some(subject_nodes_vec) },
            "prompt": prompt,
        });

        let resp: serde_json::Value = client
            .post_json(
                &format!("/artifacts/{id}/submit?project={project_id}"),
                &body,
            )
            .await?;

        if let Some(err) = resp.get("error").and_then(|v| v.as_str()) {
            if let Some(errs) = resp.get("block_errors").and_then(|v| v.as_array()) {
                eprintln!("MDX validation failed:");
                for e in errs {
                    eprintln!("  {}", e.as_str().unwrap_or("unknown"));
                }
                anyhow::bail!("{err}");
            }
            anyhow::bail!("{err}");
        }

        println!(
            "submitted {} version {}",
            resp["artifact_id"].as_str().unwrap_or(&id),
            resp["version"].as_u64().unwrap_or(0)
        );
        Ok(())
    })
}

fn cmd_feedback(
    home: &Home,
    id: String,
    project: Option<String>,
    consume: Option<String>,
    message: Option<String>,
    anchor: String,
    resolution_target: Option<String>,
) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        let project_id = resolve_project(home, project.as_deref()).await?;

        if let Some(cid) = consume {
            let resp: serde_json::Value = client
                .post_json(
                    &format!("/artifacts/{id}/feedback/{cid}/consume?project={project_id}"),
                    &serde_json::Value::Null,
                )
                .await?;
            if let Some(err) = resp.get("error").and_then(|v| v.as_str()) {
                anyhow::bail!("{err}");
            }
            println!("consumed {}", resp["cid"].as_str().unwrap_or(&cid));
        } else {
            let msg = message
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("--message is required when not using --consume"))?;
            let body = serde_json::json!({
                "message": msg,
                "anchor": anchor,
                "resolution_target": resolution_target,
            });
            let resp: serde_json::Value = client
                .post_json(
                    &format!("/artifacts/{id}/feedback?project={project_id}"),
                    &body,
                )
                .await?;
            if let Some(err) = resp.get("error").and_then(|v| v.as_str()) {
                anyhow::bail!("{err}");
            }
            println!("comment added: {}", resp["cid"].as_str().unwrap_or("?"));
        }
        Ok(())
    })
}

/// Resolve the project id: use the explicit arg, or read from the current
/// directory's `.orgasmic/project.org`, or fall back to the first board entry.
async fn resolve_project(_home: &Home, project: Option<&str>) -> anyhow::Result<String> {
    if let Some(p) = project {
        if !p.is_empty() {
            return Ok(p.to_string());
        }
    }
    // Try to read from cwd
    if let Ok(cwd) = std::env::current_dir() {
        let project_org = cwd.join(".orgasmic/project.org");
        if project_org.exists() {
            if let Ok(content) = std::fs::read_to_string(&project_org) {
                for line in content.lines() {
                    let t = line.trim();
                    if t.starts_with(":ID:") {
                        let id = t.trim_start_matches(":ID:").trim().to_string();
                        if !id.is_empty() {
                            return Ok(id);
                        }
                    }
                }
            }
        }
    }
    anyhow::bail!("could not determine project; use --project")
}
