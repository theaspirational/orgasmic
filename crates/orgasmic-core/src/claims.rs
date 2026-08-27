use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::{parse_tx_file, TxEntry};

pub const CLAIMED: &str = "task.claimed";
pub const RELEASED: &str = "task.claim_released";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskClaim {
    pub task: String,
    pub holder: String,
    pub claimed_at: String,
    pub write_scope: Option<String>,
    pub contenders: Vec<String>,
}

/// Fold every machine's append-only claim log. Sorting the events here makes
/// the result independent of directory enumeration and ingest order.
pub fn fold_claims(mut entries: Vec<TxEntry>) -> BTreeMap<String, TaskClaim> {
    entries.retain(|entry| matches!(entry.ty.as_str(), CLAIMED | RELEASED) && entry.task.is_some());
    entries.sort_by(|a, b| {
        a.time
            .cmp(&b.time)
            .then_with(|| a.machine.cmp(&b.machine))
            .then_with(|| a.tx_id.cmp(&b.tx_id))
    });

    let mut active = BTreeMap::<(String, String), TxEntry>::new();
    for entry in entries {
        let task = entry.task.clone().expect("claim entry task checked");
        let key = (task, entry.machine.clone());
        if entry.ty == CLAIMED {
            active.insert(key, entry);
        } else {
            active.remove(&key);
        }
    }

    let mut by_task = BTreeMap::<String, Vec<TxEntry>>::new();
    for ((task, _), entry) in active {
        by_task.entry(task).or_default().push(entry);
    }
    by_task
        .into_iter()
        .map(|(task, mut claims)| {
            claims.sort_by(|a, b| {
                a.time
                    .cmp(&b.time)
                    .then_with(|| a.machine.cmp(&b.machine))
                    .then_with(|| a.tx_id.cmp(&b.tx_id))
            });
            let winner = &claims[0];
            let write_scope = winner
                .extra
                .iter()
                .find(|(key, _)| key == "WRITE_SCOPE")
                .map(|(_, value)| value.clone())
                .filter(|value| !value.is_empty());
            let contenders = claims.iter().map(|entry| entry.machine.clone()).collect();
            (
                task.clone(),
                TaskClaim {
                    task,
                    holder: winner.machine.clone(),
                    claimed_at: winner.time.clone(),
                    write_scope,
                    contenders,
                },
            )
        })
        .collect()
}

pub fn read_claims(project_root: &Path) -> Result<BTreeMap<String, TaskClaim>> {
    let machines = project_root.join(".orgasmic/machines");
    let read = match std::fs::read_dir(&machines) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", machines.display())),
    };
    let mut paths = read
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("read {}", machines.display()))?
        .into_iter()
        .map(|entry| entry.path().join("claims.org"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    paths.sort();

    let mut entries = Vec::new();
    for path in paths {
        let machine_id = path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .context("claim log path has no machine id")?;
        let source =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let parsed = parse_tx_file(&source, &path.to_string_lossy())
            .with_context(|| format!("parse {}", path.display()))?;
        if let Some(entry) = parsed.iter().find(|entry| entry.machine != machine_id) {
            anyhow::bail!(
                "claim log {} belongs to machine {machine_id}, but event {} names machine {}",
                path.display(),
                entry.tx_id,
                entry.machine
            );
        }
        entries.extend(parsed);
    }
    Ok(fold_claims(entries))
}
