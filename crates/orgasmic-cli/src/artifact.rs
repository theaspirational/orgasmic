// orgasmic:arch_ARSPJ
//! CLI verbs for the artifact store (TASK-ZEFEY).
//!
//! - `artifact blocks`           — list the block vocabulary
//! - `artifact submit <id>`      — submit MDX; validates block registry
//! - `artifact feedback <id>`    — add a comment, or --consume a CID
//! - `artifact comments <id>`    — list an artifact's comments as JSON

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
        /// CID this comment replies to (optional; threaded reply).
        #[arg(long)]
        reply_to: Option<String>,
    },
    /// List an artifact's comments as JSON (e.g. clicked QuestionForm answers).
    Comments {
        /// Artifact id (e.g. ART-XYZAB).
        id: String,
        /// Project id.
        #[arg(long)]
        project: Option<String>,
        /// Include consumed comments (default hides them).
        #[arg(long)]
        include_consumed: bool,
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
            reply_to,
        } => cmd_feedback(
            home,
            id,
            project,
            consume,
            message,
            anchor,
            resolution_target,
            reply_to,
        ),
        ArtifactCmd::Comments {
            id,
            project,
            include_consumed,
        } => cmd_comments(home, id, project, include_consumed),
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

#[allow(clippy::too_many_arguments)]
fn cmd_feedback(
    home: &Home,
    id: String,
    project: Option<String>,
    consume: Option<String>,
    message: Option<String>,
    anchor: String,
    resolution_target: Option<String>,
    reply_to: Option<String>,
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
                "reply_to": reply_to,
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

fn cmd_comments(
    home: &Home,
    id: String,
    project: Option<String>,
    include_consumed: bool,
) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        let project_id = resolve_project(home, project.as_deref()).await?;
        let comments = fetch_comments(&client, &id, &project_id, include_consumed).await?;
        println!("{}", serde_json::to_string_pretty(&comments)?);
        Ok(())
    })
}

/// GET the artifact detail and reduce it to a JSON array of comment entries.
/// Consumed comments are filtered by the daemon (`include_consumed=false`
/// default), not here.
async fn fetch_comments(
    client: &DaemonClient,
    id: &str,
    project_id: &str,
    include_consumed: bool,
) -> Result<serde_json::Value> {
    let detail: serde_json::Value = client
        .get(&format!(
            "/artifacts/{id}?project={project_id}&include_consumed={include_consumed}"
        ))
        .await
        .map_err(|e| {
            if e.to_string().contains("artifact not found") {
                anyhow::anyhow!("artifact {id} not found")
            } else {
                e
            }
        })?;
    Ok(render_comments(&detail))
}

/// One output entry per comment: cid, author, time, message, anchor, consumed.
/// The stored anchor is a JSON string (question key/answer for QuestionForm
/// clicks); parse it so consumers get an object, falling back to the raw
/// string when it isn't valid JSON.
fn render_comments(detail: &serde_json::Value) -> serde_json::Value {
    let empty = Vec::new();
    let comments = detail
        .get("comments")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    serde_json::Value::Array(
        comments
            .iter()
            .map(|c| {
                let anchor = c
                    .get("anchor")
                    .and_then(|a| a.as_str())
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                    .unwrap_or_else(|| c.get("anchor").cloned().unwrap_or(serde_json::Value::Null));
                serde_json::json!({
                    "cid": c.get("cid"),
                    "author": c.get("author"),
                    // ponytail: always null — the daemon's CommentRecord drops the
                    // journal :TIME:; populate once the endpoint serves it.
                    "time": serde_json::Value::Null,
                    "message": c.get("message"),
                    "anchor": anchor,
                    "consumed": c.get("consumed"),
                })
            })
            .collect(),
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{env_guard, RecordingDaemon, ScopedEnv};

    /// Mock `GET /api/artifacts/:id`: ART-TESTA exists (one open comment with
    /// a QuestionForm answer anchor, one consumed comment), anything else 404s
    /// with the daemon's real not-found body.
    fn respond(path: &str) -> Option<(u16, String)> {
        if path.starts_with("/api/artifacts/ART-TESTA") {
            Some((
                200,
                r#"{
                    "art_id": "ART-TESTA",
                    "prompt": "",
                    "content": "",
                    "comments": [
                        {
                            "cid": "CID-open0001",
                            "author": "aspirational",
                            "version": 1,
                            "anchor": "{\"questionKey\":\"q1\",\"answer\":\"yes\"}",
                            "resolution_target": "",
                            "reply_to": "",
                            "resolved": false,
                            "consumed": false,
                            "message": "yes"
                        },
                        {
                            "cid": "CID-done0002",
                            "author": "aspirational",
                            "version": 1,
                            "anchor": "{}",
                            "resolution_target": "",
                            "reply_to": "",
                            "resolved": true,
                            "consumed": true,
                            "message": "already handled"
                        }
                    ]
                }"#
                .to_string(),
            ))
        } else if path.starts_with("/api/artifacts/") {
            Some((404, r#"{"error":"artifact not found"}"#.to_string()))
        } else {
            None
        }
    }

    /// Client pointed at `daemon` via the env override; the returned guards
    /// must stay alive for the duration of the test.
    fn client_for(daemon: &RecordingDaemon) -> (DaemonClient, ScopedEnv, ScopedEnv) {
        let url = format!("http://127.0.0.1:{}", daemon.port());
        let set = ScopedEnv::set(&[
            ("ORGASMIC_DAEMON_URL", url.as_str()),
            ("ORGASMIC_DAEMON_TOKEN", "test-token"),
        ]);
        let clear = ScopedEnv::clear(&["ORGASMIC_DAEMON_TOKEN_FILE"]);
        let client = DaemonClient::from_home(&Home::at(std::env::temp_dir())).unwrap();
        (client, set, clear)
    }

    #[test]
    fn comments_prints_cid_author_time_message_anchor_consumed() {
        let _env = env_guard();
        let daemon = RecordingDaemon::start(respond);
        let (client, _set, _clear) = client_for(&daemon);
        let rt = tokio::runtime::Runtime::new().unwrap();

        let out = rt
            .block_on(fetch_comments(&client, "ART-TESTA", "proj-1", false))
            .unwrap();

        assert_eq!(
            daemon.paths(),
            vec!["/api/artifacts/ART-TESTA?project=proj-1&include_consumed=false".to_string()],
            "default must ask the daemon with include_consumed=false"
        );
        let entries = out.as_array().expect("JSON array");
        assert_eq!(entries.len(), 2);
        let first = &entries[0];
        assert_eq!(first["cid"], "CID-open0001");
        assert_eq!(first["author"], "aspirational");
        assert!(first["time"].is_null());
        assert_eq!(first["message"], "yes");
        assert_eq!(first["anchor"]["questionKey"], "q1");
        assert_eq!(first["anchor"]["answer"], "yes");
        assert_eq!(first["consumed"], false);
        assert_eq!(entries[1]["consumed"], true);
    }

    /// The consumed/unconsumed split is enforced by the daemon (its own tests
    /// cover the filtering); the CLI's contract is passing the flag through.
    #[test]
    fn include_consumed_flag_is_forwarded_to_the_daemon() {
        let _env = env_guard();
        let daemon = RecordingDaemon::start(respond);
        let (client, _set, _clear) = client_for(&daemon);
        let rt = tokio::runtime::Runtime::new().unwrap();

        rt.block_on(fetch_comments(&client, "ART-TESTA", "proj-1", true))
            .unwrap();

        assert_eq!(
            daemon.paths(),
            vec!["/api/artifacts/ART-TESTA?project=proj-1&include_consumed=true".to_string()],
        );
    }

    #[test]
    fn unknown_artifact_error_names_the_id() {
        let _env = env_guard();
        let daemon = RecordingDaemon::start(respond);
        let (client, _set, _clear) = client_for(&daemon);
        let rt = tokio::runtime::Runtime::new().unwrap();

        let err = rt
            .block_on(fetch_comments(&client, "ART-GONE1", "proj-1", false))
            .expect_err("404 must surface as an error");

        assert!(
            err.to_string().contains("ART-GONE1"),
            "error must name the artifact id, got: {err}"
        );
    }
}
