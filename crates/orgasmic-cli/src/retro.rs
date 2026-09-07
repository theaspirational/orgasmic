//! Foreground manual retrospective. The SDK worker gets a scoped stdio broker,
//! never the daemon bearer token or a general-purpose command tool.
use anyhow::{ensure, Context, Result};
use clap::Args;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::{daemon_client::DaemonClient, home::Home};

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("scope").required(true).multiple(true).args(["run", "task", "task_sequence"])))]
pub struct RetroArgs {
    #[arg(long)]
    project: String,
    #[arg(long, action = clap::ArgAction::Append)]
    run: Vec<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    task: Vec<String>,
    /// Comma-separated tasks, preserving this order then run chronology within each task.
    #[arg(long, value_delimiter = ',', conflicts_with_all = ["run", "task"])]
    task_sequence: Vec<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    question: Vec<String>,
    /// Claude model id passed unchanged to the SDK. Authentication requires
    /// ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN; ambient configs are not loaded.
    #[arg(long, required_unless_present = "prepare_only")]
    model: Option<String>,
    /// Pin and print the scope without starting a provider turn.
    #[arg(long)]
    prepare_only: bool,
}

pub fn cmd_retro(home: &Home, args: RetroArgs) -> Result<()> {
    tokio::runtime::Runtime::new()?.block_on(async {
        // A diagnostic never starts/restarts the operator daemon.
        let client = DaemonClient::from_home(home)?;
        let project = crate::daemon_client::path_segment(&args.project);
        let prepared: Value = client.post_json(&format!("/projects/{project}/retro"), &json!({
            "runs":args.run,"tasks":args.task,"task_sequence":args.task_sequence,"questions":args.question,
        })).await?;
        if args.prepare_only {
            println!("{}", serde_json::to_string_pretty(&prepared)?);
            return Ok(());
        }
        ensure!(std::env::var_os("ANTHROPIC_API_KEY").is_some() || std::env::var_os("CLAUDE_CODE_OAUTH_TOKEN").is_some(), "read-only retro requires ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN; prepared scope: {}", prepared["scope"]);
        let id = prepared["id"].as_str().context("missing retrospective id")?;
        let digest = prepared["scope_sha256"].as_str().context("missing scope digest")?;
        let scope_path = prepared["scope"].as_str().context("missing scope path")?;
        eprintln!("Retrospective scope: {scope_path}");
        let bytes = std::fs::read(scope_path)?;
        ensure!(bytes.len() <= 256 * 1024, "scope exceeds byte limit");
        use sha2::{Digest, Sha256};
        ensure!(format!("{:x}", Sha256::digest(&bytes)) == digest, "scope changed before dispatch");
        let scope: Value = serde_json::from_slice(&bytes)?;
        let invocation = orgasmic_drivers::adapters::chat_sdk::provider_host_invocation().map_err(anyhow::Error::msg)?;
        let directory = prepared["directory"].as_str().context("missing scope directory")?;
        let mut command = tokio::process::Command::new(invocation.binary);
        command.args(invocation.leading_args).arg("retro").current_dir(directory)
            .env_clear().stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped()).kill_on_drop(true);
        // Only provider auth and executable discovery survive. No daemon token,
        // project instructions, proxy hooks, or inherited Node loader options.
        for key in ["PATH", "SYSTEMROOT", "CLAUDE_BIN", "ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"] {
            if let Some(value) = std::env::var_os(key) { command.env(key,value); }
        }
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().context("start read-only retrospector")?;
        #[cfg(unix)]
        let worker_pid = child.id();
        let mut input = child.stdin.take().context("worker stdin missing")?;
        input.write_all(format!("{}\n",json!({"prompt":scope["prompt"],"model":args.model,"directory":directory})).as_bytes()).await?;
        let mut output = BufReader::new(child.stdout.take().context("worker stdout missing")?);
        // Bounded drain avoids pipe deadlock and raw native logs becoming durable evidence.
        let stderr = child.stderr.take().context("worker stderr missing")?;
        let drain = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut chunk = [0u8;4096];
            while reader.read(&mut chunk).await.unwrap_or(0) > 0 {}
        });
        let endpoint = format!("/projects/{project}/retro/{id}/tool");
        let work = tokio::time::timeout(std::time::Duration::from_secs(600), async {
            let mut submitted = None;
            for _ in 0..128 {
                let mut line = Vec::new();
                let n = (&mut output).take(128 * 1024 + 1).read_until(b'\n', &mut line).await?;
                if n == 0 { break; }
                ensure!(line.len() <= 128 * 1024 && line.ends_with(b"\n"), "worker request exceeds limit");
                let message: Value = serde_json::from_slice(&line)?;
                let request: orgasmic_daemon::retro::Request = serde_json::from_value(message["request"].clone())?;
                let response: Result<Value> = client.post_json(&endpoint,&json!({"scope_sha256":digest,"request":request})).await;
                let reply = match response {
                    Ok(value) => {
                        if value["status"] == "report_submitted" { submitted = Some(value.clone()); }
                        json!({"id":message["id"],"result":value})
                    }
                    Err(error) => json!({"id":message["id"],"error":error.to_string()}),
                };
                input.write_all(format!("{reply}\n").as_bytes()).await?;
                if submitted.is_some() { break; }
            }
            submitted.context("retrospector exited or exceeded tool budget without submitting a report")
        });
        let result = tokio::select! {
            result = work => result.map_err(|_| anyhow::anyhow!("retrospector timed out; immutable scope: {scope_path}")),
            _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("retrospective interrupted; immutable scope: {scope_path}")),
        };
        // Submit is terminal. Stop only this foreground SDK process, never a source run.
        drop(input);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
        #[cfg(unix)]
        if let Some(pid) = worker_pid {
            // The process group belongs exclusively to this foreground retrospective.
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
        drain.abort();
        let report = result??;
        println!("{}",serde_json::to_string_pretty(&report)?);
        Ok(())
    })
}
