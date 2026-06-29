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
import { initAndroidInsets } from '@/lib/androidInsets';

// Mirror the Android shell's window insets onto --sai-* before anything renders.
initAndroidInsets();

type RuntimeProbe = {
  cliPath?: string | null;
  cliVersion?: string | null;
  daemonState?: string | null;
  error?: string | null;
};

type UiSessionResponse = {
  path: string;
};

/** A remembered backend: a daemon origin and the token that reached it. */
type Connection = { url: string; token: string };

const CONNECTIONS_KEY = 'orgasmic:remote:connections';
// Pre-history builds stored a single backend in these two keys; migrate them.
const LEGACY_URL_KEY = 'orgasmic:remote:url';
const LEGACY_TOKEN_KEY = 'orgasmic:remote:token';
const MAX_CONNECTIONS = 6;
const UPDATE_CHECK_TIMEOUT_MS = 3500;
// Seconds the launch screen counts down before reconnecting to the last backend.
// Long enough to read the host and cancel; short enough not to feel like a stall.
const AUTO_CONNECT_SECONDS = 3;

const titleEl = document.getElementById('title') as HTMLHeadingElement;
const statusEl = document.getElementById('status') as HTMLParagraphElement;
const detailEl = document.getElementById('detail') as HTMLPreElement;
const retryEl = document.getElementById('retry') as HTMLButtonElement;
const connectEl = document.getElementById('connect') as HTMLButtonElement;
const formEl = document.getElementById('remote') as HTMLFormElement;
const urlEl = document.getElementById('url') as HTMLInputElement;
const tokenEl = document.getElementById('token') as HTMLInputElement;
const recentEl = document.getElementById('recent') as HTMLDivElement;
const recListEl = document.getElementById('reclist') as HTMLUListElement;
const autoconnectEl = document.getElementById('autoconnect') as HTMLDivElement;
const autoconnectMsgEl = document.getElementById('autoconnectmsg') as HTMLParagraphElement;
const cancelAutoEl = document.getElementById('cancelauto') as HTMLButtonElement;
const updatesEl = document.getElementById('updates') as HTMLElement;
const updateBarEl = document.getElementById('updatebar') as HTMLDivElement;
const updateMsgEl = document.getElementById('updatemsg') as HTMLParagraphElement;
const updNoteEl = document.getElementById('updnote') as HTMLParagraphElement;
const downloadEl = document.getElementById('download') as HTMLButtonElement;
const channelEl = document.getElementById('channel') as HTMLSelectElement;
const checkEl = document.getElementById('check') as HTMLButtonElement;

let pendingUpdate: AppUpdateMetadata | null = null;
let autoConnectTimer: number | null = null;

// ---- remembered backends ---------------------------------------------------

function readRaw(key: string): string {
  try {
    return localStorage.getItem(key) ?? '';
  } catch {
    return '';
  }
}

/** All remembered backends, most-recent first. Falls back to (and absorbs) the
 *  pre-history single-slot keys so an upgrade keeps the user's last daemon. */
function readConnections(): Connection[] {
  let list: Connection[] = [];
  try {
    const parsed = JSON.parse(readRaw(CONNECTIONS_KEY)) as unknown;
    if (Array.isArray(parsed)) {
      list = parsed
        .filter((c): c is Connection =>
          !!c && typeof (c as Connection).url === 'string' && typeof (c as Connection).token === 'string',
        )
        .map((c) => ({ url: c.url, token: c.token }));
    }
  } catch {
    /* corrupt/empty — fall through to legacy migration */
  }
  if (!list.length) {
    const url = readRaw(LEGACY_URL_KEY);
    const token = readRaw(LEGACY_TOKEN_KEY);
    if (url && token) list = [{ url, token }];
  }
  return list;
}

function writeConnections(list: Connection[]): void {
  try {
    localStorage.setItem(CONNECTIONS_KEY, JSON.stringify(list.slice(0, MAX_CONNECTIONS)));
  } catch {
    /* private mode / storage disabled — connecting still works this session */
  }
}

/** Promote a backend to the front of the list (deduped by origin). Called only
 *  after a connection actually succeeds, so the list stays trustworthy. */
function rememberConnection(url: string, token: string): void {
  const next = [{ url, token }, ...readConnections().filter((c) => c.url !== url)];
  writeConnections(next);
}

function forgetConnection(url: string): void {
  writeConnections(readConnections().filter((c) => c.url !== url));
}

/** Compact host[:port] for a recent-connections row. */
function displayHost(url: string): string {
  try {
    const u = new URL(url);
    return u.port ? `${u.hostname}:${u.port}` : u.hostname;
  } catch {
    return url;
  }
}

function renderRecent(): void {
  const list = readConnections();
  recListEl.replaceChildren();
  if (!list.length) {
    recentEl.hidden = true;
    return;
  }
  for (const conn of list) {
    const li = document.createElement('li');

    const use = document.createElement('button');
    use.type = 'button';
    use.className = 'recuse';
    use.textContent = displayHost(conn.url);
    use.addEventListener('click', () => {
      urlEl.value = conn.url;
      tokenEl.value = conn.token;
      void connectRemote(conn.url, conn.token);
    });

    const del = document.createElement('button');
    del.type = 'button';
    del.className = 'recdel';
    del.setAttribute('aria-label', `Forget ${displayHost(conn.url)}`);
    del.textContent = '×';
    del.addEventListener('click', (event) => {
      event.stopPropagation();
      forgetConnection(conn.url);
      renderRecent();
    });

    li.append(use, del);
    recListEl.appendChild(li);
  }
  recentEl.hidden = false;
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
      `Switch to ${update.channel}: ${update.currentVersion} → ${update.version} ` +
      `(older build; may need a reinstall).`;
    downloadEl.textContent = `Switch to ${update.channel}`;
  } else {
    updateMsgEl.textContent =
      `Update available: ${update.currentVersion} → ${update.version} (${update.channel}).`;
    downloadEl.textContent = 'Download update';
  }
  updNoteEl.textContent = '';
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
    updNoteEl.textContent = switched ? `Checking ${channel}…` : 'Checking for updates…';
  }
  // `switched` (the user flipped the channel) lets the check offer that
  // channel's build even when it's older than what's installed; routine checks
  // only surface a strictly newer build.
  const probe = checkAppUpdate(channel, { allowDowngrade: switched }).catch(() => null);
  const update = manual ? await probe : await withTimeout(probe, UPDATE_CHECK_TIMEOUT_MS);
  if (manual) checkEl.disabled = false;

  if (update) {
    showUpdateBanner(update);
    return update;
  }
  hideUpdateBanner();
  updNoteEl.textContent = manual ? `No newer ${channel} build available.` : '';
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
  renderRecent();
  if (supportsAppUpdateChecks()) {
    updatesEl.hidden = false;
    channelEl.value = savedUpdateChannel();
  }
}

/** Stop a running auto-connect countdown and clear its banner. Idempotent, so
 *  it's safe to call from any path that supersedes the countdown. */
function cancelAutoConnect(): void {
  if (autoConnectTimer !== null) {
    window.clearInterval(autoConnectTimer);
    autoConnectTimer = null;
  }
  autoconnectEl.hidden = true;
}

/** Reconnect to a remembered backend after a short, cancellable countdown, so a
 *  launch never strands the user on a remote they didn't choose. Cancel, a
 *  recent row, Connect, or editing the form all stop it (the recent/Connect/
 *  submit paths route through connectRemote, which cancels first). */
function scheduleAutoConnect(conn: Connection): void {
  let remaining = AUTO_CONNECT_SECONDS;
  const host = displayHost(conn.url);
  const tick = (): void => {
    if (remaining <= 0) {
      cancelAutoConnect();
      void connectRemote(conn.url, conn.token);
      return;
    }
    autoconnectMsgEl.textContent = `Connecting to ${host} in ${remaining}s…`;
    remaining -= 1;
  };
  autoconnectEl.hidden = false;
  tick(); // render "in 3s…" immediately, then count down once a second
  autoConnectTimer = window.setInterval(tick, 1000);
}

async function connectRemote(rawUrl: string, rawToken: string): Promise<void> {
  // A manual connect (recent row, Connect, form submit) supersedes any pending
  // countdown — clear it so the two can't race into a double navigation.
  cancelAutoConnect();
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
  rememberConnection(origin, token);
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
  updatesEl.hidden = true;
  recentEl.hidden = true;
  cancelAutoConnect();
  hideUpdateBanner();
  updNoteEl.textContent = '';
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
  // daemon. Prefill from the most-recent backend and list the rest.
  const connections = readConnections();
  if (connections.length) {
    urlEl.value = connections[0].url;
    tokenEl.value = connections[0].token;
  }
  showRemoteForm();

  // Auto-check for an app update on launch. If one is found, surface it quietly
  // and let the user choose Download or just Connect — don't auto-connect past it.
  const update = await runUpdateCheck(false);
  if (update) return;

  // Fast path: one known backend → reconnect after a short, cancellable
  // countdown so the user can intervene and pick a different remote. With
  // several, show the picker and let them choose which to open.
  if (connections.length === 1) {
    scheduleAutoConnect(connections[0]);
  }
}

formEl.addEventListener('submit', (event) => {
  event.preventDefault();
  connectFromInputs();
});

connectEl.addEventListener('click', connectFromInputs);

cancelAutoEl.addEventListener('click', cancelAutoConnect);

// Editing the form means the user wants a different remote — stop the count.
urlEl.addEventListener('input', cancelAutoConnect);
tokenEl.addEventListener('input', cancelAutoConnect);

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
    updNoteEl.textContent = err instanceof Error ? err.message : String(err);
  } finally {
    downloadEl.disabled = false;
  }
});

void start();

const style = document.createElement('style');
// Theme tokens mirror the daemon-served UI's actual themes (src/styles.css):
// `paper` (light) and `black-paper` (dark) — warm cream / warm dark-brown with
// a faint grid, teal accent in light and tan accent in dark, same Geist face.
// The bootstrap webview is a separate origin from the daemon UI, so it can't
// read the app's saved theme preference — it follows the OS like the app's
// default `system` mode, with the identical token values so the screens match.
style.textContent = `
  :root {
    color-scheme: light dark;
    /* Safe-area insets — see lib/androidInsets.ts. The shell webview is a
       separate origin from the daemon UI, so it carries its own copy. */
    --safe-top: max(env(safe-area-inset-top, 0px), var(--sai-top, 0px));
    --safe-right: max(env(safe-area-inset-right, 0px), var(--sai-right, 0px));
    --safe-bottom: max(env(safe-area-inset-bottom, 0px), var(--sai-bottom, 0px));
    --safe-left: max(env(safe-area-inset-left, 0px), var(--sai-left, 0px));
    /* paper (light) — mirrors html[data-theme='paper'] */
    --background: oklch(0.955 0.023 78);
    --foreground: oklch(0.24 0.018 72);
    --card: oklch(0.985 0.017 78 / 0.94);
    --muted-foreground: oklch(0.45 0.016 76);
    --primary: oklch(0.48 0.11 198);
    --primary-foreground: oklch(0.99 0.014 78);
    --secondary: oklch(0.91 0.022 78);
    --secondary-foreground: oklch(0.27 0.018 76);
    --accent: oklch(0.88 0.032 78);
    --border: oklch(0.81 0.021 78);
    --ring: oklch(0.48 0.11 198);
    --grid-line: oklch(0.78 0.026 78 / 0.22);
    --radius: 0.5rem;
    --sans: 'Geist Variable', system-ui, -apple-system, sans-serif;
    --mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    font-family: var(--sans);
    color: var(--foreground);
  }
  @media (prefers-color-scheme: dark) {
    :root {
      /* black-paper (dark) — mirrors html[data-theme='black-paper'] */
      --background: #160d08;
      --foreground: #e8dcd4;
      --card: #21140c;
      --muted-foreground: rgb(232 220 212 / 0.68);
      --primary: #966d4f;
      --primary-foreground: #160d08;
      --secondary: #2a1a10;
      --secondary-foreground: #e8dcd4;
      --accent: #ba977d;
      --border: rgb(232 220 212 / 0.18);
      --ring: #ba977d;
      --grid-line: rgb(186 151 125 / 0.08);
    }
  }
  * {
    box-sizing: border-box;
  }
  body {
    margin: 0;
    background-color: var(--background);
  }
  .runtime-shell {
    min-height: 100vh;
    display: grid;
    place-items: center;
    padding: calc(1.5rem + var(--safe-top)) calc(1.5rem + var(--safe-right))
      calc(1.5rem + var(--safe-bottom)) calc(1.5rem + var(--safe-left));
    background-color: var(--background);
    background-image:
      linear-gradient(to right, var(--grid-line) 1px, transparent 1px),
      linear-gradient(to bottom, var(--grid-line) 1px, transparent 1px);
    background-size: 18px 18px;
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
  /* auto-connect countdown — a cancellable pre-connect to the last backend */
  #autoconnect {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: 0.6rem;
    border: 1px solid var(--border);
    background: var(--secondary);
    border-radius: var(--radius);
    padding: 0.7rem 0.8rem;
  }
  #autoconnectmsg {
    flex: 1;
    min-width: 12rem;
    margin: 0;
    color: var(--foreground);
    font-size: 0.9rem;
  }
  #autoconnect #cancelauto {
    padding: 0.45rem 0.9rem;
    font-size: 0.85rem;
  }
  /* recent connections — tap a row to reconnect, × to forget */
  #recent {
    display: grid;
    gap: 0.45rem;
  }
  .reclabel {
    font-size: 0.8rem;
    font-weight: 600;
    color: var(--foreground);
  }
  #reclist {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 0.4rem;
  }
  #reclist li {
    display: flex;
    gap: 0.4rem;
  }
  .recuse {
    flex: 1;
    text-align: left;
    font: 0.9rem/1.3 var(--mono);
    border: 1px solid var(--border);
    background: var(--secondary);
    color: var(--secondary-foreground);
    border-radius: calc(var(--radius) - 0.125rem);
    padding: 0.55rem 0.7rem;
    cursor: pointer;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .recdel {
    flex: 0 0 auto;
    width: 2.4rem;
    font-size: 1.1rem;
    line-height: 1;
    color: var(--muted-foreground);
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
  /* collapsed, platform-neutral setup help */
  .help {
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--background);
  }
  .help > summary {
    cursor: pointer;
    list-style: none;
    padding: 0.55rem 0.7rem;
    font-size: 0.8rem;
    font-weight: 600;
    color: var(--foreground);
  }
  .help > summary::-webkit-details-marker {
    display: none;
  }
  .help > summary::before {
    content: '＋ ';
    color: var(--muted-foreground);
  }
  .help[open] > summary::before {
    content: '－ ';
  }
  .helpbody {
    display: grid;
    gap: 0.5rem;
    padding: 0 0.7rem 0.7rem;
  }
  .helpbody p {
    font-size: 0.78rem;
    line-height: 1.5;
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
  /* updates — a quiet footer, divided off from the connect flow */
  .updates {
    display: grid;
    gap: 0.6rem;
    margin-top: 0.25rem;
    padding-top: 1rem;
    border-top: 1px solid var(--border);
  }
  .updrow {
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
    color: var(--muted-foreground);
  }
  .ghost {
    white-space: nowrap;
    background: transparent;
  }
  #updatebar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: 0.6rem;
    border: 1px solid var(--border);
    background: var(--secondary);
    border-radius: var(--radius);
    padding: 0.7rem 0.8rem;
  }
  #updatemsg {
    flex: 1;
    min-width: 12rem;
    color: var(--foreground);
    font-size: 0.85rem;
  }
  #updatebar #download {
    padding: 0.45rem 0.8rem;
    font-size: 0.85rem;
  }
  .updnote {
    font-size: 0.78rem;
    color: var(--muted-foreground);
  }
  .updnote:empty {
    display: none;
  }
`;
document.head.appendChild(style);
