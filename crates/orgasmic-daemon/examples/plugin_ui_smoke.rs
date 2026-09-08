//! Disposable browser fixture. Run with ORGASMIC_EMBED_UI=1 and open the printed
//! one-use session URL. No production home, ledger, or daemon is touched.
use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let home = Home::at(temp.path().join("home"));
    home.ensure()?;
    let root = temp.path().join("project");
    std::fs::create_dir_all(root.join(".orgasmic"))?;
    std::fs::write(
        root.join(".orgasmic/project.org"),
        "* PROJECT demo\n:PROPERTIES:\n:ID: demo\n:END:\n",
    )?;
    std::fs::write(
        home.board(),
        format!(
            "* PROJECT demo\n:PROPERTIES:\n:ID: demo\n:PATH: {}\n:BRANCH: main\n:END:\n",
            root.display()
        ),
    )?;
    let running = Daemon::run(
        home.clone(),
        DaemonOptions {
            bind_override: Some("127.0.0.1".parse()?),
            port_override: Some(0),
            fs_watcher_enabled: false,
            ..Default::default()
        },
    )
    .await?;
    let base = format!("http://{}", running.addr);
    let token = std::fs::read_to_string(home.auth_token())?;
    let client = reqwest::Client::new();
    let session: serde_json::Value = client
        .post(format!("{base}/api/auth/ui-session"))
        .bearer_auth(token.trim())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    println!("Open once: {base}{}", session["path"].as_str().unwrap());
    println!("Fixture home: {}", home.root.display());
    println!("Fixture daemon: {base}");
    println!("Use ORGASMIC_HOME and ORGASMIC_DAEMON_URL above for plugin add/enable/run/disable. Ctrl-C removes the fixture.");
    tokio::signal::ctrl_c().await?;
    let _ = running.shutdown.send(());
    running.join.await?;
    Ok(())
}
