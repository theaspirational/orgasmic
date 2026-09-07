//! Bounded, explicit live ACP probe. No prompt means catalog-only (no model turn).
use orgasmic_core::RuntimeIdentity;
use orgasmic_drivers::{chat_driver, DriverConfig, DriverContext, RunKind, UserInputRequest};
use serde_json::json;
use std::{path::PathBuf, time::Duration};
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let provider = args.get(1).expect("provider");
    let cwd = PathBuf::from(args.get(2).expect("disposable absolute working directory"));
    anyhow::ensure!(
        cwd.is_absolute() && cwd.is_dir(),
        "absolute existing cwd required"
    );
    let driver = chat_driver(provider).ok_or_else(|| anyhow::anyhow!("unsupported provider"))?;
    let context = DriverContext {
        identity: RuntimeIdentity {
            run_id: "acp-probe".into(),
            runtime_id: uuid::Uuid::new_v4().to_string(),
            boot_id: "probe".into(),
        },
        run_kind: RunKind::Worker,
        task_id: "probe".into(),
        worker_id: "probe".into(),
        project_id: None,
        worktree: Some(cwd.clone()),
    };
    let config = DriverConfig::from_value(
        json!({"auto_start_turn":false,"access":"auto","model":args.get(3),"cwd":cwd,"project_root":cwd,"sandbox_permissions":"allow_exec=true,allow_patch=true,allow_network=true,allow_writes_outside_cwd=false"}),
    );
    let mut session =
        tokio::time::timeout(Duration::from_secs(90), driver.acquire(context, config)).await??;
    eprintln!("PROCESS_GROUP {:?}", session.pid);
    let mut events = session.events;
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel();
    let output = tokio::spawn(async move {
        let mut seq = 0;
        while let Some(event) = events.recv().await {
            println!(
                "{}",
                json!({"seq":seq,"time":chrono::Utc::now().to_rfc3339(),"kind":"driver_event","event":event})
            );
            seq += 1;
            match &event {
                orgasmic_core::DriverEvent::Acp { message, .. }
                    if message["method"] == "session/prompt" =>
                {
                    let _ = done_tx.send(Ok(message["params"]["stopReason"].clone()));
                }
                orgasmic_core::DriverEvent::DriverError { message, .. } => {
                    let _ = done_tx.send(Err(message.clone()));
                }
                _ => {}
            }
        }
    });
    let result: anyhow::Result<()> = async {
        let catalog = tokio::time::timeout(
            Duration::from_secs(60),
            session.control.runtime_options_catalog(),
        )
        .await??;
        eprintln!("CATALOG {}", serde_json::to_string(&catalog)?);
        for prompt in args.iter().skip(4) {
            tokio::time::timeout(
                Duration::from_secs(120),
                session.control.send_input(UserInputRequest {
                    input: prompt.clone(),
                }),
            )
            .await??;
            let stop = tokio::time::timeout(Duration::from_secs(120), done_rx.recv())
                .await?
                .ok_or_else(|| anyhow::anyhow!("event stream closed before completion"))?
                .map_err(anyhow::Error::msg)?;
            anyhow::ensure!(
                stop == "end_turn" || stop == "max_tokens",
                "unexpected stop reason: {stop}"
            );
        }
        Ok(())
    }
    .await;
    let released = tokio::time::timeout(
        Duration::from_secs(15),
        session.control.release("live probe finished"),
    )
    .await;
    eprintln!("RELEASE {released:?}");
    drop(session.control);
    if let Some(mut producer) = session.producer {
        if tokio::time::timeout(Duration::from_secs(10), &mut producer)
            .await
            .is_err()
        {
            producer.abort();
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), output).await;
    released??;
    result
}
