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
    let plugin = home.user().join("plugins/meetings");
    std::fs::create_dir_all(plugin.join("ui"))?;
    std::fs::write(plugin.join("plugin.org"), "* Plugin\n:PROPERTIES:\n:ID: meetings\n:VERSION: 0.1.0\n:PLUGIN_API: 1\n:SCHEMA: 1\n:UI: ui/index.js\n:SDK: ^1.0\n:CAPABILITIES: nodes.read\n:END:\n** Node type\n:PROPERTIES:\n:COLLECTION: meetings\n:ID_PREFIX: MEET-\n:LABEL: Meeting\n:LABEL_PLURAL: Meetings\n:END:\n")?;
    std::fs::write(
        plugin.join("ui/label.js"),
        "export const label = 'Meetings plugin';",
    )?;
    std::fs::write(
        plugin.join("ui/index.js"),
        r#"
import { createElement as h, useState } from 'react';
import { Button, useResource, fetchGraphNodes } from '@orgasmic/plugin-sdk';
import { label } from './label.js';
export function register(ctx) {
  return ctx.registerNodeView('meetings', function Meetings() {
    const [count, setCount] = useState(0);
    const result = useResource('plugin-smoke', () => fetchGraphNodes('demo', 'meetings'));
    return h('section', {'data-plugin': 'meetings'},
      h('h1', null, label),
      h('p', null, result.loading ? 'Loading nodes' : 'Nodes loaded'),
      h(Button, {onClick: () => setCount(count + 1)}, `Count ${count}`));
  });
}
"#,
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
    client.post(format!("{base}/api/plugins/meetings/activation")).bearer_auth(token.trim()).json(&serde_json::json!({"project":"demo", "enabled":true, "approved_capabilities":["nodes.read", "ui.execute"]})).send().await?.error_for_status()?;
    let session: serde_json::Value = client
        .post(format!("{base}/api/auth/ui-session"))
        .bearer_auth(token.trim())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    println!("Open once: {base}{}", session["path"].as_str().unwrap());
    println!("Import /plugins/meetings/ui/demo/index.js and call register with a one-shot registerNodeView host. Ctrl-C shuts down and removes the fixture.");
    tokio::signal::ctrl_c().await?;
    let _ = running.shutdown.send(());
    running.join.await?;
    Ok(())
}
