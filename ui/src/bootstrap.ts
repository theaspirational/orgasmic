import { invoke } from '@tauri-apps/api/core';

type RuntimeProbe = {
  cliPath?: string | null;
  cliVersion?: string | null;
  daemonState?: string | null;
  error?: string | null;
};

const statusEl = document.getElementById('status') as HTMLParagraphElement;
const detailEl = document.getElementById('detail') as HTMLPreElement;
const retryEl = document.getElementById('retry') as HTMLButtonElement;

async function launch() {
  retryEl.disabled = true;
  statusEl.textContent = 'Checking for the orgasmic CLI.';
  detailEl.textContent = '';
  try {
    const probe = await invoke<RuntimeProbe>('runtime_probe');
    if (!probe.cliPath) {
      statusEl.textContent = 'The orgasmic CLI is not installed.';
      detailEl.textContent = [
        'Install the orgasmic CLI, then reopen this app.',
        '',
        'Expected locations:',
        '  $ORGASMIC_HOME/bin/orgasmic',
        '  orgasmic on PATH',
      ].join('\n');
      return;
    }
    statusEl.textContent = 'Starting daemon UI.';
    detailEl.textContent = `${probe.cliVersion ?? 'orgasmic'}\n${probe.cliPath}`;
    const url = await invoke<string>('runtime_launch_url');
    window.location.assign(url);
  } catch (err) {
    statusEl.textContent = 'Unable to open the daemon UI.';
    detailEl.textContent = err instanceof Error ? err.message : String(err);
  } finally {
    retryEl.disabled = false;
  }
}

retryEl.addEventListener('click', () => {
  void launch();
});

void launch();

const style = document.createElement('style');
style.textContent = `
  :root {
    color-scheme: dark light;
    font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    background: #101214;
    color: #f4f5f5;
  }
  body {
    margin: 0;
  }
  .runtime-shell {
    min-height: 100vh;
    display: grid;
    place-items: center;
    background: #101214;
  }
  .runtime-panel {
    width: min(30rem, calc(100vw - 3rem));
    display: grid;
    gap: 1rem;
  }
  .runtime-mark {
    font-size: 0.78rem;
    letter-spacing: 0;
    color: #77d0d8;
    font-weight: 600;
  }
  h1 {
    margin: 0;
    font-size: 1.35rem;
    line-height: 1.25;
  }
  p {
    margin: 0;
    color: #b6bec2;
    line-height: 1.45;
  }
  pre {
    min-height: 5rem;
    margin: 0;
    overflow: auto;
    white-space: pre-wrap;
    color: #d9dee1;
    background: #191c1f;
    border: 1px solid #343a3f;
    border-radius: 0.5rem;
    padding: 0.85rem;
    font: 0.82rem/1.45 ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
  }
  button {
    border: 1px solid #3f474c;
    background: #20252a;
    color: #f4f5f5;
    border-radius: 0.45rem;
    padding: 0.55rem 0.8rem;
    font: inherit;
  }
  button:disabled {
    opacity: 0.6;
  }
`;
document.head.appendChild(style);
