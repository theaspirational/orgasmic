// orgasmic:TASK-WPC6G
//! `orgasmic gotcha add|list`: `.orgasmic/gotchas.org` on the CLI write
//! surface. Entries are bare `**` headings under the file's single top
//! heading, addressed by title; the write goes through the daemon's generic
//! org-file rewrite (`org.file_rewritten` tx) like any other org file.

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde::Deserialize;

use crate::daemon_client::DaemonClient;
use crate::home::Home;

const GOTCHAS_PATH: &str = ".orgasmic/gotchas.org";

#[derive(Subcommand, Debug)]
pub enum GotchaCmd {
    /// Append one `** <title>` entry to `.orgasmic/gotchas.org` through the daemon.
    Add {
        /// Project id; defaults to the project containing the cwd.
        #[arg(long)]
        project: Option<String>,
        /// Entry heading (the text after `**`). Must not repeat an existing title.
        #[arg(long)]
        title: String,
        /// Entry body in Org markup: evidence, cause, fix, exit condition.
        /// Must not contain `*` headings of its own.
        #[arg(long)]
        body: String,
        /// Stable idempotency key. Replaying the same value returns the
        /// original result instead of writing twice.
        #[arg(long = "request-id")]
        request_id: Option<String>,
    },
    /// Print the `**` entry titles in `.orgasmic/gotchas.org`.
    List {
        /// Project id; defaults to the project containing the cwd.
        #[arg(long)]
        project: Option<String>,
    },
}

#[derive(Deserialize)]
struct OrgFileResponse {
    contents: String,
    #[serde(default)]
    tx_id: Option<String>,
}

fn titles(contents: &str) -> impl Iterator<Item = &str> {
    contents
        .lines()
        .filter_map(|line| line.strip_prefix("** ").map(str::trim))
}

/// The new file text, or a refusal. Pure so the tests need no daemon.
fn appended(contents: &str, title: &str, body: &str, stamp: &str) -> Result<String> {
    let title = title.trim();
    if title.is_empty() || title.contains('\n') {
        bail!("gotcha title must be one non-empty line");
    }
    if let Some(line) = body
        .lines()
        .find(|line| line.starts_with('*') && line.trim_start_matches('*').starts_with(' '))
    {
        bail!("gotcha body must not contain headings; found `{line}` — use plain prose or lists");
    }
    if titles(contents).any(|existing| existing == title) {
        bail!("gotcha `{title}` already exists in {GOTCHAS_PATH}; entries are addressed by title");
    }
    let mut out = contents.trim_end().to_string();
    out.push_str(&format!(
        "\n\n** {title}\n:PROPERTIES:\n:CREATED: {stamp}\n:END:\n{}\n",
        body.trim_end()
    ));
    Ok(out)
}

async fn fetch(client: &DaemonClient, project: &str) -> Result<OrgFileResponse> {
    client
        .get(&format!("/org/file?project={project}&path={GOTCHAS_PATH}"))
        .await
        .map_err(|e| {
            if format!("{e:#}").contains("404") {
                anyhow::anyhow!(
                    "{GOTCHAS_PATH} is missing in project {project}; scaffold it from \
                     shipped/project-scaffold/gotchas.org before adding entries"
                )
            } else {
                e
            }
        })
}

pub fn cmd_gotcha(home: &Home, cmd: GotchaCmd) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        match cmd {
            GotchaCmd::List { project } => {
                let project = crate::manager::resolve_project(project)?;
                let file = fetch(&client, &project).await?;
                for title in titles(&file.contents) {
                    println!("{title}");
                }
            }
            GotchaCmd::Add {
                project,
                title,
                body,
                request_id,
            } => {
                let project = crate::manager::resolve_project(project)?;
                // ponytail: read-modify-write with no base version; the
                // org-file rewrite endpoint has no OCC. Add one when two
                // writers actually race on gotchas.org.
                let file = fetch(&client, &project).await?;
                let stamp = chrono::Utc::now()
                    .format("[%Y-%m-%d %a %H:%M:%S]")
                    .to_string();
                let contents = appended(&file.contents, &title, &body, &stamp)?;
                let written: OrgFileResponse = client
                    .post_json(
                        "/org/file",
                        &serde_json::json!({
                            "project": project,
                            "path": GOTCHAS_PATH,
                            "contents": contents,
                            "request_id": request_id,
                        }),
                    )
                    .await?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "path": GOTCHAS_PATH,
                        "title": title.trim(),
                        "tx_id": written.tx_id,
                    }))?
                );
            }
        }
        Ok::<(), anyhow::Error>(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "#+title: x gotchas\n\n* Gotchas\n\n** First\nBody.\n";

    #[test]
    fn append_adds_exactly_one_entry_with_a_created_stamp() {
        let out = appended(FILE, "Second", "Why.\n", "[2026-09-02 Wed 10:00:00]").unwrap();
        assert_eq!(titles(&out).collect::<Vec<_>>(), ["First", "Second"]);
        assert!(out.ends_with(
            "** Second\n:PROPERTIES:\n:CREATED: [2026-09-02 Wed 10:00:00]\n:END:\nWhy.\n"
        ));
    }

    #[test]
    fn append_refuses_duplicate_titles_and_headings_in_the_body() {
        assert!(appended(FILE, "First", "x", "[t]").is_err());
        assert!(appended(FILE, "Third", "ok\n** sneaky\n", "[t]").is_err());
        assert!(appended(FILE, "Third", "*bold* is fine", "[t]").is_ok());
    }
}
