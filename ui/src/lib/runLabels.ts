import type { RunSummary } from './types';

type RunLabelInput = {
  task_id: string;
  driver?: string | null;
  kind?: string;
  harness?: string | null;
};

// Manager runs carry a task id under the `manager.` namespace
// (`manager.launch:<project>` since dec_061; legacy `manager.tick`). Everything
// else is a worker run keyed by its real task id (e.g. `TASK-124`).
export function isManagerRun(run: { task_id: string }): boolean {
  return run.task_id.startsWith('manager.');
}

// External manager registrations are presence-only supervisor runs. They have
// no PTY/chat transport, so the dock must reject them at every entry point.
export function isExternalManagerRun(run: { driver?: string | null }): boolean {
  return (run.driver ?? '').trim().toLowerCase() === 'external';
}

export function isRunDockEligible(run: { driver?: string | null }): boolean {
  return !isExternalManagerRun(run);
}

// Drivers whose live surface is the PTY terminal (xterm pane) rather than the
// ACP chat transcript. rmux attaches through the same daemon PTY bridge as
// tmux (`rmux attach-session`), so it renders in the terminal stack too.
export function isPtyTerminalDriver(driverTag: string): boolean {
  const normalized = driverTag.replaceAll('_', '-').toLowerCase();
  return normalized === 'tmux-tui' || normalized === 'tmux' || normalized === 'rmux';
}

export function runUsesPtyTerminal(run: { driver?: string | null; kind?: string }): boolean {
  const driver = run.driver?.trim() ?? '';
  if (driver) return isPtyTerminalDriver(driver);
  // Older runs predating the driver column fall back to the run kind, which for
  // tmux-tui managers is recorded as the kind string.
  return isPtyTerminalDriver(run.kind ?? '');
}

// Readable provider/transport from the driver tag. The tag is a transport mode
// like `tmux-tui`, `codex-stdio`, `claude-acp`; we surface a human label and
// keep the raw id out of the primary tab title (it lives in the tooltip).
const TRANSPORT_LABELS: Record<string, string> = {
  'acp-stdio': 'ACP stdio',
  'acp-ws': 'ACP websocket',
  'subprocess-stream-json': 'Subprocess JSON',
  rmux: 'rmux',
  'tmux-tui': 'Claude tmux',
  tmux: 'Claude tmux',
  'codex-stdio': 'Codex',
  'codex-appserver': 'Codex',
  'claude-acp': 'Claude',
  'cursor-acp': 'Cursor',
  hermes: 'Hermes',
  chat: 'Chat',
  // External manager self-registration (dec_3Y2E1): a presence record for a
  // manager session started outside the app, not a driver orgasmic spawned.
  external: 'External',
};

const HARNESS_LABELS: Record<string, string> = {
  claude: 'Claude',
  codex: 'Codex',
  'cursor-agent': 'Cursor',
  hermes: 'Hermes',
  external: 'External',
};

function normalizeId(value: string | null | undefined): string {
  return (value ?? '').replaceAll('_', '-').toLowerCase().trim();
}

export function transportLabel(driverTag: string | null | undefined): string {
  const tag = normalizeId(driverTag);
  if (!tag) return 'Agent';
  return TRANSPORT_LABELS[tag] ?? tag;
}

function harnessLabel(harness: string): string {
  const tag = normalizeId(harness);
  return HARNESS_LABELS[tag] ?? tag;
}

function runDriverId(run: { driver?: string | null; kind?: string }): string {
  return normalizeId(run.driver ?? run.kind);
}

function harnessFromSessionSource(source?: string | null): string | null {
  if (!source) return null;
  for (const rawLine of source.split('\n')) {
    const line = rawLine.trim();
    if (!line) continue;
    try {
      const envelope = JSON.parse(line) as {
        event?: {
          type?: unknown;
          protocol_version?: unknown;
          capabilities?: Record<string, unknown>;
        };
      };
      const event = envelope.event;
      if (event?.type !== 'ready') continue;
      const endpoint = event.capabilities?.endpoint;
      if (typeof endpoint === 'string') {
        const match = /(?:^|:)(claude|codex|cursor-agent|hermes)$/i.exec(endpoint);
        if (match?.[1]) return normalizeId(match[1]);
      }
      const protocol = event.protocol_version;
      if (typeof protocol === 'string') {
        const match = /^(claude|codex|cursor-agent|hermes)(?:[-/]|$)/i.exec(protocol);
        if (match?.[1]) return normalizeId(match[1]);
      }
    } catch {
      continue;
    }
  }
  return null;
}

function runHarness(run: { harness?: string | null }, source?: string | null): string | null {
  const explicit = normalizeId(run.harness);
  return explicit || harnessFromSessionSource(source);
}

function runProviderLabel(run: RunLabelInput, source?: string | null): string {
  const harness = runHarness(run, source);
  if (harness) return harnessLabel(harness);
  return transportLabel(runDriverId(run));
}

export function runDriverTag(run: RunLabelInput, source?: string | null): string {
  const harness = runHarness(run, source);
  if (!harness) return transportLabel(runDriverId(run));
  const provider = harnessLabel(harness);
  const driver = runDriverId(run);
  if (!driver || driver === harness || transportLabel(driver) === provider) return provider;
  return `${provider} · ${driver}`;
}

export type RunRole = 'manager' | 'worker';

export function runRole(run: { task_id: string }): RunRole {
  return isManagerRun(run) ? 'manager' : 'worker';
}

// Primary tab/peek label: role or task plus provider/transport, never the raw
// run id (which is reserved for the tooltip / secondary detail).
export function runTabTitle(run: RunLabelInput, source?: string | null): string {
  const transport = runProviderLabel(run, source);
  if (isManagerRun(run)) return `Manager · ${transport}`;
  const task = run.task_id?.trim() || 'Run';
  return `${task} · ${transport}`;
}

// Compact secondary label used inside lists where the title is already shown.
export function runSubtitle(run: RunSummary): string {
  const events = `${run.event_count} event${run.event_count === 1 ? '' : 's'}`;
  return run.sub_state ? `${events} · ${run.sub_state}` : events;
}
