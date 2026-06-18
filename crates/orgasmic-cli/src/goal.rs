use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Deserialize;

use crate::daemon_client::DaemonClient;
use crate::home::Home;

#[derive(Subcommand, Debug)]
pub enum GoalCmd {
    /// Set a new active goal (supersedes any prior active goal).
    Set {
        /// Project id; defaults to the project containing the cwd.
        #[arg(long)]
        project: Option<String>,
        /// Goal id; omitted → daemon mints goal-YYYYMMDD-slug from the title.
        #[arg(long)]
        id: Option<String>,
        /// Short goal title (the heading prose after the GOAL keyword).
        #[arg(long)]
        title: String,
        /// Statement body (required `** Statement` section).
        #[arg(long)]
        statement: String,
        /// Optional `** Reached When` section body.
        #[arg(long = "reached-when")]
        reached_when: Option<String>,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long = "request-id")]
        request_id: Option<String>,
    },
    /// Clear the active goal (GOAL → CLEARED).
    Clear {
        /// Project id; defaults to the project containing the cwd.
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long = "request-id")]
        request_id: Option<String>,
    },
    /// Supersede the active goal without setting a replacement.
    Supersede {
        /// Project id; defaults to the project containing the cwd.
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long = "request-id")]
        request_id: Option<String>,
    },
}

#[derive(Deserialize)]
struct GoalMutationResponse {
    goal_id: String,
    tx_id: String,
    tx_path: String,
}

fn resolve_project(project: Option<String>) -> Result<String> {
    crate::manager::resolve_project(project)
}

pub fn cmd_goal(home: &Home, cmd: GoalCmd) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        let (route, body) = match cmd {
            GoalCmd::Set {
                project,
                id,
                title,
                statement,
                reached_when,
                reason,
                request_id,
            } => (
                format!("/projects/{}/goal/set", resolve_project(project)?),
                serde_json::json!({
                    "id": id,
                    "title": title,
                    "statement": statement,
                    "reached_when": reached_when,
                    "reason": reason,
                    "request_id": request_id,
                }),
            ),
            GoalCmd::Clear {
                project,
                reason,
                request_id,
            } => (
                format!("/projects/{}/goal/clear", resolve_project(project)?),
                serde_json::json!({
                    "reason": reason,
                    "request_id": request_id,
                }),
            ),
            GoalCmd::Supersede {
                project,
                reason,
                request_id,
            } => (
                format!("/projects/{}/goal/supersede", resolve_project(project)?),
                serde_json::json!({
                    "reason": reason,
                    "request_id": request_id,
                }),
            ),
        };
        let response: GoalMutationResponse = client.post_json(&route, &body).await?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "goal_id": response.goal_id,
                "tx_id": response.tx_id,
                "tx_path": response.tx_path,
            }))?
        );
        Ok::<(), anyhow::Error>(())
    })
}
