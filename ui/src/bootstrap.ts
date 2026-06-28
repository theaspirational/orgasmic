// Match the daemon-served UI's typeface (it imports the same variable font).
import '@fontsource-variable/geist';
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
  if (update.isDowngrade) {
    // Reached only via an explicit channel switch to an older build. Say so
    // plainly — it's a channel switch, not an upgrade — and warn that Android
    // blocks installing a lower build over a higher one without a reinstall.
    updateMsgEl.textContent =
      `Switch to ${update.channel} — ${update.currentVersion} → ${update.version} ` +
      `(older build; may need a reinstall).`;
    downloadEl.textContent = `Switch to ${update.channel}`;
  } else {
    updateMsgEl.textContent =
      `Update available — ${update.currentVersion} → ${update.version} (${update.channel}).`;
    downloadEl.textContent = 'Download update';
  }
  updateBarEl.hidden = false;
}

function hideUpdateBanner(): void {
  pendingUpdate = null;
  updateBarEl.hidden = true;
}

/** Check for a newer app build on the saved channel. `manual` checks are
 *  unbounded and report their result; the launch check is time-boxed so it can
 *  gate auto-connect without stalling it. Returns the update, if any. */
async function runUpdateCheck(manual: boolean, switched = false): Promise<AppUpdateMetadata | null> {
  if (!supportsAppUpdateChecks()) return null;
  const channel = savedUpdateChannel();
  if (manual) {
    checkEl.disabled = true;
    detailEl.textContent = switched ? `Checking ${channel}…` : 'Checking for updates…';
  }
  // `switched` (the user flipped the channel) lets the check offer that
  // channel's build even when it's older than what's installed; routine checks
  // only surface a strictly newer build.
  const probe = checkAppUpdate(channel, { allowDowngrade: switched }).catch(() => null);
  const update = manual ? await probe : await withTimeout(probe, UPDATE_CHECK_TIMEOUT_MS);
  if (manual) checkEl.disabled = false;

  if (update) {
    showUpdateBanner(update);
    if (manual) detailEl.textContent = '';
    return update;
  }
  hideUpdateBanner();
  if (manual) detailEl.textContent = `No newer ${channel} build available.`;
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
  // Explicit switch: offer the chosen channel's build even if it's older than
  // what's installed (e.g. nightly → stable), surfaced as a switch, not a nag.
  void runUpdateCheck(true, true);
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
// Theme tokens mirror the daemon-served UI (src/styles.css): same Geist face,
// teal-accented shadcn palette, and radius. The bootstrap webview is a separate
// origin from the daemon UI, so it can't read the app's saved light/dark
// preference — instead it follows the OS like the app's default `system` mode,
// using the identical light + dark token values so the two screens match.
style.textContent = `
  :root {
    color-scheme: light dark;
    --background: oklch(0.99 0 0);
    --foreground: oklch(0.18 0 0);
    --card: oklch(1 0 0);
    --muted-foreground: oklch(0.45 0 0);
    --primary: oklch(0.55 0.13 200);
    --primary-foreground: oklch(0.99 0 0);
    --secondary: oklch(0.97 0 0);
    --secondary-foreground: oklch(0.205 0 0);
    --accent: oklch(0.93 0.04 200);
    --border: oklch(0.92 0 0);
    --ring: oklch(0.55 0.13 200);
    --radius: 0.5rem;
    --sans: 'Geist Variable', system-ui, -apple-system, sans-serif;
    --mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    font-family: var(--sans);
    background: var(--background);
    color: var(--foreground);
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --background: oklch(0.135 0 0);
      --foreground: oklch(0.96 0 0);
      --card: oklch(0.175 0 0);
      --muted-foreground: oklch(0.68 0 0);
      --primary: oklch(0.72 0.13 200);
      --primary-foreground: oklch(0.135 0 0);
      --secondary: oklch(0.25 0 0);
      --secondary-foreground: oklch(0.96 0 0);
      --accent: oklch(0.27 0.04 200);
      --border: oklch(0.27 0 0);
      --ring: oklch(0.72 0.13 200);
    }
  }
  * {
    box-sizing: border-box;
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
    background: var(--card);
    border: 1px solid var(--border);
    border-radius: calc(var(--radius) + 0.25rem);
    padding: 1.5rem;
  }
  .runtime-mark {
    font-size: 0.78rem;
    letter-spacing: 0;
    color: var(--primary);
    font-weight: 600;
  }
  h1 {
    margin: 0;
    font-size: 1.35rem;
    line-height: 1.25;
    color: var(--foreground);
  }
  p {
    margin: 0;
    color: var(--muted-foreground);
    line-height: 1.45;
  }
  #updatebar {
    display: grid;
    gap: 0.6rem;
    border: 1px solid var(--border);
    background: var(--accent);
    border-radius: var(--radius);
    padding: 0.85rem;
  }
  #updatemsg {
    color: var(--foreground);
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
    color: var(--foreground);
  }
  input,
  select {
    width: 100%;
    box-sizing: border-box;
    border: 1px solid var(--border);
    background: var(--background);
    color: var(--foreground);
    border-radius: var(--radius);
    padding: 0.6rem 0.7rem;
    font: 0.92rem/1.4 inherit;
  }
  input::placeholder {
    color: var(--muted-foreground);
  }
  input:focus,
  select:focus {
    outline: 2px solid var(--ring);
    outline-offset: -1px;
    border-color: var(--ring);
  }
  .updcontrols {
    display: flex;
    flex-wrap: wrap;
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
    color: var(--foreground);
  }
  .updcontrols button {
    white-space: nowrap;
  }
  .hint {
    font-size: 0.78rem;
    color: var(--muted-foreground);
  }
  code {
    font: 0.82em var(--mono);
    color: var(--foreground);
  }
  pre {
    margin: 0;
    overflow: auto;
    white-space: pre-wrap;
    color: var(--muted-foreground);
    background: var(--card);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    padding: 0.85rem;
    font: 0.82rem/1.45 var(--mono);
  }
  pre:empty {
    display: none;
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    justify-content: flex-end;
    gap: 0.5rem;
  }
  button {
    border: 1px solid var(--border);
    background: var(--secondary);
    color: var(--secondary-foreground);
    border-radius: calc(var(--radius) - 0.125rem);
    padding: 0.55rem 0.9rem;
    font: inherit;
    cursor: pointer;
  }
  #connect,
  #download {
    border-color: var(--primary);
    background: var(--primary);
    color: var(--primary-foreground);
  }
  button:disabled {
    opacity: 0.6;
    cursor: default;
  }
`;
document.head.appendChild(style);
