import { invoke } from '@tauri-apps/api/core';
import {
  checkAppUpdate,
  installAppUpdate,
  savedUpdateChannel,
  saveUpdateChannel,
  supportsAppUpdateChecks,
  type AppUpdateMetadata,
  type UpdateChannel,
} from '@/lib/appUpdate';

type RuntimeProbe = {
  cliPath?: string | null;
  cliVersion?: string | null;
  daemonState?: string | null;
  error?: string | null;
};

type UiSessionResponse = {
  path: string;
};

const URL_KEY = 'orgasmic:remote:url';
const TOKEN_KEY = 'orgasmic:remote:token';
const UPDATE_CHECK_TIMEOUT_MS = 3500;

const titleEl = document.getElementById('title') as HTMLHeadingElement;
const statusEl = document.getElementById('status') as HTMLParagraphElement;
const detailEl = document.getElementById('detail') as HTMLPreElement;
const retryEl = document.getElementById('retry') as HTMLButtonElement;
const connectEl = document.getElementById('connect') as HTMLButtonElement;
const formEl = document.getElementById('remote') as HTMLFormElement;
const urlEl = document.getElementById('url') as HTMLInputElement;
const tokenEl = document.getElementById('token') as HTMLInputElement;
const updateBarEl = document.getElementById('updatebar') as HTMLDivElement;
const updateMsgEl = document.getElementById('updatemsg') as HTMLParagraphElement;
const downloadEl = document.getElementById('download') as HTMLButtonElement;
const dismissEl = document.getElementById('dismiss') as HTMLButtonElement;
const updateControlsEl = document.getElementById('updatecontrols') as HTMLDivElement;
const channelEl = document.getElementById('channel') as HTMLSelectElement;
const checkEl = document.getElementById('check') as HTMLButtonElement;

let pendingUpdate: AppUpdateMetadata | null = null;

function readStored(key: string): string {
  try {
    return localStorage.getItem(key) ?? '';
  } catch {
    return '';
  }
}

function writeStored(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
    /* private mode / storage disabled — connection still proceeds */
  }
}

/** Resolve a promise to null if it rejects or outruns `ms` — a slow/failed
 *  update check (e.g. offline LAN) must never block connecting. */
function withTimeout<T>(promise: Promise<T>, ms: number): Promise<T | null> {
  return new Promise((resolve) => {
    const timer = window.setTimeout(() => resolve(null), ms);
    promise
      .then((value) => {
        window.clearTimeout(timer);
        resolve(value);
      })
      .catch(() => {
        window.clearTimeout(timer);
        resolve(null);
      });
  });
}

/** Normalize a user-entered host into a daemon origin. Bare hosts get http://
 *  and the default daemon port; tunnel/https URLs are left intact. */
function normalizeDaemonOrigin(raw: string): string {
  const trimmed = raw.trim().replace(/\/+$/, '');
  if (!trimmed) return '';
  const withScheme = /^https?:\/\//i.test(trimmed) ? trimmed : `http://${trimmed}`;
  let parsed: URL;
  try {
    parsed = new URL(withScheme);
  } catch {
    return '';
  }
  if (parsed.protocol === 'http:' && !parsed.port) parsed.port = '4848';
  return parsed.origin;
}

// ---- app update (Android sideload / desktop updater) -----------------------

function showUpdateBanner(update: AppUpdateMetadata): void {
  pendingUpdate = update;
  updateMsgEl.textContent = `Update available — ${update.currentVersion} → ${update.version} (${update.channel}).`;
  updateBarEl.hidden = false;
}

function hideUpdateBanner(): void {
  pendingUpdate = null;
  updateBarEl.hidden = true;
}

/** Check for a newer app build on the saved channel. `manual` checks are
 *  unbounded and report their result; the launch check is time-boxed so it can
 *  gate auto-connect without stalling it. Returns the update, if any. */
async function runUpdateCheck(manual: boolean): Promise<AppUpdateMetadata | null> {
  if (!supportsAppUpdateChecks()) return null;
  const channel = savedUpdateChannel();
  if (manual) {
    checkEl.disabled = true;
    detailEl.textContent = 'Checking for updates…';
  }
  const probe = checkAppUpdate(channel).catch(() => null);
  const update = manual ? await probe : await withTimeout(probe, UPDATE_CHECK_TIMEOUT_MS);
  if (manual) checkEl.disabled = false;

  if (update) {
    showUpdateBanner(update);
    if (manual) detailEl.textContent = '';
    return update;
  }
  hideUpdateBanner();
  if (manual) detailEl.textContent = `You're on the latest ${channel} build.`;
  return null;
}

// ---- desktop: local CLI runtime --------------------------------------------

async function launchLocal(): Promise<void> {
  retryEl.disabled = true;
  titleEl.textContent = 'Opening local runtime';
  statusEl.textContent = 'Starting daemon UI.';
  const url = await invoke<string>('runtime_launch_url');
  window.location.assign(url);
}

// ---- remote: connect to a daemon over the network --------------------------

function showRemoteForm(error?: string): void {
  titleEl.textContent = 'Connect to runtime';
  statusEl.textContent = 'Point this app at an orgasmic daemon.';
  formEl.hidden = false;
  connectEl.hidden = false;
  retryEl.hidden = true;
  connectEl.disabled = false;
  detailEl.textContent = error ?? '';
  if (supportsAppUpdateChecks()) {
    updateControlsEl.hidden = false;
    channelEl.value = savedUpdateChannel();
  }
}

async function connectRemote(rawUrl: string, rawToken: string): Promise<void> {
  const origin = normalizeDaemonOrigin(rawUrl);
  const token = rawToken.trim();
  if (!origin) {
    showRemoteForm('Enter a daemon URL, e.g. http://192.168.1.50:4848');
    return;
  }
  if (!token) {
    showRemoteForm('Enter the daemon bearer token.');
    return;
  }

  connectEl.disabled = true;
  detailEl.textContent = '';
  statusEl.textContent = `Connecting to ${origin}…`;

  // Mint a one-time UI-session ticket against the remote daemon, then navigate
  // to it: the daemon redeems the ticket, sets the session cookie, and serves
  // its own UI same-origin — the same handshake the desktop shell uses locally.
  let response: Response;
  try {
    response = await fetch(`${origin}/api/auth/ui-session`, {
      method: 'POST',
      headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
      body: '{}',
    });
  } catch (err) {
    showRemoteForm(
      `Can't reach ${origin}.\n${err instanceof Error ? err.message : String(err)}\n\n` +
        'Check the daemon is running, bound to 0.0.0.0 with LAN enabled, and reachable from this device (same Wi-Fi, VPN, or tunnel).',
    );
    return;
  }

  if (response.status === 401) {
    showRemoteForm('Invalid bearer token for this daemon.');
    return;
  }
  if (!response.ok) {
    showRemoteForm(`Daemon returned ${response.status}.\n${await response.text()}`);
    return;
  }

  const session = (await response.json()) as UiSessionResponse;
  writeStored(URL_KEY, origin);
  writeStored(TOKEN_KEY, token);
  statusEl.textContent = 'Opening daemon UI.';
  window.location.assign(`${origin}${session.path}`);
}

function connectFromInputs(): void {
  void connectRemote(urlEl.value, tokenEl.value);
}

// ---- entry -----------------------------------------------------------------

async function start(): Promise<void> {
  retryEl.disabled = true;
  formEl.hidden = true;
  connectEl.hidden = true;
  retryEl.hidden = false;
  updateControlsEl.hidden = true;
  hideUpdateBanner();
  titleEl.textContent = 'Opening runtime';
  statusEl.textContent = 'Checking for the orgasmic CLI.';
  detailEl.textContent = '';

  let probe: RuntimeProbe;
  try {
    probe = await invoke<RuntimeProbe>('runtime_probe');
  } catch {
    probe = { cliPath: null };
  }

  // A local CLI runtime is present (desktop): open its served UI directly. The
  // served SPA runs on the whitelisted 127.0.0.1 origin and carries its own
  // update checks, so the bootstrap doesn't duplicate them here.
  if (probe.cliPath) {
    detailEl.textContent = `${probe.cliVersion ?? 'orgasmic'}\n${probe.cliPath}`;
    try {
      await launchLocal();
    } catch (err) {
      statusEl.textContent = 'Unable to open the local daemon UI.';
      detailEl.textContent = err instanceof Error ? err.message : String(err);
      retryEl.disabled = false;
    }
    return;
  }

  // No local runtime (mobile, or a host without the CLI): connect to a remote
  // daemon. Prefill from the last successful connection.
  const savedUrl = readStored(URL_KEY);
  const savedToken = readStored(TOKEN_KEY);
  urlEl.value = savedUrl;
  tokenEl.value = savedToken;
  showRemoteForm();

  // Auto-check for an app update on launch. If one is found, surface it and let
  // the user choose Download or Continue — do NOT auto-connect past the banner.
  const update = await runUpdateCheck(false);
  if (update) return;

  if (savedUrl && savedToken) {
    void connectRemote(savedUrl, savedToken);
  }
}

formEl.addEventListener('submit', (event) => {
  event.preventDefault();
  connectFromInputs();
});

connectEl.addEventListener('click', connectFromInputs);

retryEl.addEventListener('click', () => {
  void start();
});

checkEl.addEventListener('click', () => {
  void runUpdateCheck(true);
});

channelEl.addEventListener('change', () => {
  saveUpdateChannel(channelEl.value as UpdateChannel);
  void runUpdateCheck(true);
});

downloadEl.addEventListener('click', async () => {
  if (!pendingUpdate) return;
  downloadEl.disabled = true;
  try {
    await installAppUpdate(pendingUpdate);
  } catch (err) {
    detailEl.textContent = err instanceof Error ? err.message : String(err);
  } finally {
    downloadEl.disabled = false;
  }
});

dismissEl.addEventListener('click', () => {
  hideUpdateBanner();
  const url = urlEl.value;
  const token = tokenEl.value;
  if (normalizeDaemonOrigin(url) && token.trim()) {
    void connectRemote(url, token);
  }
});

void start();

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
    padding: 1.5rem;
    box-sizing: border-box;
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
  #updatebar {
    display: grid;
    gap: 0.6rem;
    border: 1px solid #2f6f76;
    background: #122a2e;
    border-radius: 0.6rem;
    padding: 0.85rem;
  }
  #updatemsg {
    color: #aeeef4;
    font-size: 0.9rem;
  }
  #remote {
    display: grid;
    gap: 0.85rem;
  }
  .field {
    display: grid;
    gap: 0.35rem;
  }
  .field > span {
    font-size: 0.8rem;
    font-weight: 600;
    color: #d9dee1;
  }
  input,
  select {
    width: 100%;
    box-sizing: border-box;
    border: 1px solid #343a3f;
    background: #191c1f;
    color: #f4f5f5;
    border-radius: 0.5rem;
    padding: 0.6rem 0.7rem;
    font: 0.92rem/1.4 inherit;
  }
  input::placeholder {
    color: #7d868b;
  }
  input:focus,
  select:focus {
    outline: none;
    border-color: #77d0d8;
  }
  .updcontrols {
    display: flex;
    align-items: flex-end;
    gap: 0.6rem;
  }
  .chan {
    display: grid;
    gap: 0.35rem;
    flex: 1;
  }
  .chan > span {
    font-size: 0.8rem;
    font-weight: 600;
    color: #d9dee1;
  }
  .updcontrols button {
    white-space: nowrap;
  }
  .hint {
    font-size: 0.78rem;
    color: #8a9296;
  }
  code {
    font: 0.82em ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    color: #c7ccd0;
  }
  pre {
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
  pre:empty {
    display: none;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.5rem;
  }
  button {
    border: 1px solid #3f474c;
    background: #20252a;
    color: #f4f5f5;
    border-radius: 0.45rem;
    padding: 0.55rem 0.9rem;
    font: inherit;
    cursor: pointer;
  }
  #connect,
  #download {
    border-color: #2f6f76;
    background: #14343a;
    color: #aeeef4;
  }
  button:disabled {
    opacity: 0.6;
    cursor: default;
  }
`;
document.head.appendChild(style);
