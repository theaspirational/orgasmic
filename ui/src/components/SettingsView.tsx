import { useEffect, useMemo, useState, type ReactNode } from 'react';
import { relaunch } from '@tauri-apps/plugin-process';
import {
  AlertTriangle,
  ArrowDown,
  ArrowUp,
  CheckCircle2,
  Download,
  GitBranch,
  HardDrive,
  Plus,
  RefreshCw,
  Trash2,
} from 'lucide-react';
import { toast } from 'sonner';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import {
  fetchOrgFile,
  fetchRecoveryStatus,
  fetchWorkerValidation,
  fetchWorkers,
  postOrgFile,
} from '@/lib/api';
import {
  type AppUpdateMetadata,
  type UpdateChannel,
  UPDATE_CHANNELS,
  checkAppUpdate,
  installAppUpdate,
  savedUpdateChannel,
  saveUpdateChannel,
  supportsAppUpdateChecks,
} from '@/lib/appUpdate';
import { useBackendProfiles } from '@/lib/backend';
import { THEME_OPTIONS, useTheme, type ThemePreference } from '@/lib/theme';
import { useResource } from '@/lib/useResource';
import type { WorkerSummary, WorkerValidationResult } from '@/lib/types';
import { cn } from '@/lib/utils';

import { PageHeader } from './Primitives';
import { type ProjectConfig, emptyConfig, parseConfig, spliceConfig } from './configSplice';

type UpdateStatus = 'idle' | 'checking' | 'available' | 'not-available' | 'blocked' | 'installing' | 'error';

function groupByKind(workers: WorkerSummary[]): Array<[string, WorkerSummary[]]> {
  const out = new Map<string, WorkerSummary[]>();
  for (const w of [...workers].sort((a, b) => a.id.localeCompare(b.id))) {
    const list = out.get(w.kind) ?? [];
    list.push(w);
    out.set(w.kind, list);
  }
  return [...out.entries()].sort((a, b) => a[0].localeCompare(b[0]));
}

function activeRunLabel(runId: string, index: number): string {
  return index < 3 ? runId : '';
}

export function SettingsView({ projectId }: { projectId: string | null }) {
  const { profiles, activeProfile, activeProfileId, setActiveProfile, updateProfile, addProfile, testConnection } =
    useBackendProfiles();
  const { preference, setPreference } = useTheme();
  const refresh = useRefreshToken();
  const configFile = useResource(
    `settings-project:${projectId ?? 'default'}:${refresh}`,
    () => fetchOrgFile('.orgasmic/config.org', projectId),
    { enabled: Boolean(projectId) },
  );
  const workers = useResource(`settings-workers:${refresh}`, () => fetchWorkers());
  const workerValidation = useResource(
    `settings-worker-validation:${refresh}`,
    () => fetchWorkerValidation(),
  );
  const [config, setConfig] = useState<ProjectConfig>(() => emptyConfig());
  const [savingConfig, setSavingConfig] = useState(false);
  const [testResult, setTestResult] = useState<string | null>(null);
  const [newName, setNewName] = useState('');
  const [newUrl, setNewUrl] = useState('http://127.0.0.1:8739');
  const [updateStatus, setUpdateStatus] = useState<UpdateStatus>('idle');
  const [updateMessage, setUpdateMessage] = useState<string | null>(null);
  const [pendingUpdate, setPendingUpdate] = useState<AppUpdateMetadata | null>(null);
  const [updateChannel, setUpdateChannel] = useState<UpdateChannel>(() => savedUpdateChannel());

  const parsedConfig = useMemo(() => parseConfig(configFile.data?.contents ?? ''), [configFile.data?.contents]);
  const dirty = useMemo(() => JSON.stringify(config) !== JSON.stringify(parsedConfig), [config, parsedConfig]);
  const workerGroups = useMemo(() => groupByKind(workers.data ?? []), [workers.data]);
  const knownWorkers = useMemo(() => new Set((workers.data ?? []).map((w) => w.id)), [workers.data]);
  const validationByWorker = useMemo(() => {
    const out = new Map<string, WorkerValidationResult>();
    for (const result of workerValidation.data ?? []) {
      if (result.id) out.set(result.id, result);
    }
    return out;
  }, [workerValidation.data]);
  const invalidWorkers = useMemo(
    () => (workerValidation.data ?? []).filter((result) => !result.ok),
    [workerValidation.data],
  );

  useEffect(() => {
    setConfig(parsedConfig);
  }, [parsedConfig]);

  useEffect(() => {
    saveUpdateChannel(updateChannel);
    setPendingUpdate(null);
    setUpdateStatus('idle');
    setUpdateMessage(null);
  }, [updateChannel]);

  useEffect(() => {
    if (!supportsAppUpdateChecks()) return;
    void checkForUpdate({ silent: true });
  }, [updateChannel]);

  function setPipelineWorker(index: number, value: string) {
    setConfig((c) => ({
      ...c,
      pipeline: c.pipeline.map((worker, i) => (i === index ? value : worker)),
    }));
  }

  function movePipelineWorker(index: number, direction: -1 | 1) {
    setConfig((c) => {
      const nextIndex = index + direction;
      if (nextIndex < 0 || nextIndex >= c.pipeline.length) return c;
      const pipeline = [...c.pipeline];
      [pipeline[index], pipeline[nextIndex]] = [pipeline[nextIndex], pipeline[index]];
      return { ...c, pipeline };
    });
  }

  function removePipelineWorker(index: number) {
    setConfig((c) => ({ ...c, pipeline: c.pipeline.filter((_, i) => i !== index) }));
  }

  function addPipelineWorker() {
    setConfig((c) => ({ ...c, pipeline: [...c.pipeline, workers.data?.[0]?.id ?? ''] }));
  }

  async function runTest() {
    const result = await testConnection();
    setTestResult(result.ok ? `Connected (${result.latencyMs} ms)` : result.error ?? 'Failed');
  }

  async function saveProjectConfig() {
    if (!projectId) return;
    setSavingConfig(true);
    try {
      const fresh = await fetchOrgFile('.orgasmic/config.org', projectId);
      const contents = spliceConfig(fresh.contents, config);
      const result = await postOrgFile('.orgasmic/config.org', contents, projectId);
      toast.success('Project config saved', { description: result.tx_id });
      await configFile.refresh();
    } catch (err) {
      toast.error('Project config save failed', {
        description: err instanceof Error ? err.message : String(err),
      });
    } finally {
      setSavingConfig(false);
    }
  }

  async function checkForUpdate({ silent = false }: { silent?: boolean } = {}) {
    if (!supportsAppUpdateChecks()) {
      setUpdateStatus('error');
      setUpdateMessage('Packaged app only');
      return;
    }

    setUpdateStatus('checking');
    setUpdateMessage(null);
    setPendingUpdate(null);
    try {
      const update = await checkAppUpdate(updateChannel);
      if (!update) {
        setUpdateStatus('not-available');
        setUpdateMessage('No update available');
        return;
      }
      setPendingUpdate(update);
      setUpdateStatus('available');
      const versionCode = update.versionCode ? ` (${update.versionCode})` : '';
      setUpdateMessage(`${updateChannel}: ${update.currentVersion} -> ${update.version}${versionCode}`);
    } catch (err) {
      setUpdateStatus('error');
      setUpdateMessage(err instanceof Error ? err.message : String(err));
      if (!silent) {
        toast.error('Update check failed', {
          description: err instanceof Error ? err.message : String(err),
        });
      }
    }
  }

  async function installUpdate() {
    if (!pendingUpdate) return;

    const isAndroidSideload = pendingUpdate.platform === 'android-sideload';
    if (!isAndroidSideload) {
      setUpdateStatus('checking');
      setUpdateMessage('Checking active runs');
    }

    try {
      if (!isAndroidSideload) {
        const recovery = await fetchRecoveryStatus();
        if (recovery.live_runs.length > 0) {
          const runIds = recovery.live_runs
            .map((run, index) => activeRunLabel(run.run_id, index))
            .filter(Boolean)
            .join(', ');
          const suffix = recovery.live_runs.length > 3 ? `, +${recovery.live_runs.length - 3} more` : '';
          setUpdateStatus('blocked');
          setUpdateMessage(`Update blocked: ${recovery.live_runs.length} active run(s)`);
          toast.error('Update blocked', {
            description: `Active runs: ${runIds}${suffix}`,
          });
          return;
        }
      }

      setUpdateStatus('installing');
      setUpdateMessage(`${isAndroidSideload ? 'Opening APK' : 'Installing'} ${pendingUpdate.version}`);
      await installAppUpdate(pendingUpdate);
      if (isAndroidSideload) {
        setUpdateStatus('available');
        setUpdateMessage(`APK opened for ${pendingUpdate.version}`);
        toast.success('APK download opened', { description: 'Confirm the Android installer after download.' });
        return;
      }

      toast.success('Update installed', { description: 'Restarting orgasmic' });
      await relaunch();
    } catch (err) {
      setUpdateStatus('error');
      setUpdateMessage(err instanceof Error ? err.message : String(err));
      toast.error('Update failed', {
        description: err instanceof Error ? err.message : String(err),
      });
    }
  }

  function renderWorkerSelect(index: number) {
    const value = config.pipeline[index] ?? '';
    return (
      <Select
        value={value || '__unset__'}
        onValueChange={(v) => setPipelineWorker(index, v === '__unset__' ? '' : v)}
        disabled={!projectId}
      >
        <SelectTrigger className="h-8 w-full font-mono text-xs">
          <SelectValue placeholder="(unset)" />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="__unset__" className="font-mono text-xs text-muted-foreground">
            (unset)
          </SelectItem>
          {value && !knownWorkers.has(value) ? (
            <SelectItem value={value} className="font-mono text-xs">
              {value} · missing
            </SelectItem>
          ) : null}
          {workerGroups.map(([kind, list]) => (
            <SelectGroup key={kind}>
              <SelectLabel className="text-[10px] uppercase tracking-wide text-muted-foreground">{kind}</SelectLabel>
              {list.map((w) => (
                <SelectItem key={w.id} value={w.id} className="font-mono text-xs">
                  {w.id}
                  {validationByWorker.get(w.id)?.ok === false ? ' · invalid' : ''}
                </SelectItem>
              ))}
            </SelectGroup>
          ))}
        </SelectContent>
      </Select>
    );
  }

  return (
    <div className="flex flex-col gap-6">
      <PageHeader title="Settings" description="Per-project configuration and local app preferences." />

      <ScopeSection
        tone="project"
        icon={<GitBranch className="size-4" />}
        title="Project"
        badge={projectId ?? undefined}
        provenance={
          <>
            Saved to <code className="rounded bg-background/70 px-1 py-0.5 font-mono text-[11px]">.orgasmic/config.org</code>{' '}
            · versioned in this repo, shared with your team.
          </>
        }
      >
        {!projectId ? (
          <EmptyRow>Select a project to edit its configuration.</EmptyRow>
        ) : (
          <>
            <div className="px-4 py-4">
              <div className="flex items-center gap-2 text-sm font-medium text-foreground">
                <span>Worker pipeline</span>
                {invalidWorkers.length > 0 ? (
                  <Badge variant="destructive" className="gap-1">
                    <AlertTriangle className="size-3" />
                    {invalidWorkers.length} invalid
                  </Badge>
                ) : workerValidation.data ? (
                  <Badge variant="outline" className="gap-1">
                    <CheckCircle2 className="size-3 text-emerald-600" />
                    valid
                  </Badge>
                ) : null}
              </div>
              <p className="mt-0.5 text-xs text-muted-foreground">
                Ordered workers used for the project default <code className="font-mono">:PIPELINE:</code>.
              </p>
              {invalidWorkers.length > 0 ? (
                <div className="mt-3 flex flex-col gap-2 rounded-md border border-destructive/35 bg-destructive/5 px-3 py-2">
                  {invalidWorkers.map((result) => (
                    <div key={result.id ?? result.source_path ?? 'unknown-worker'} className="min-w-0 text-xs">
                      <div className="font-mono font-medium text-destructive">
                        {result.id ?? result.source_path ?? 'unknown-worker'}
                      </div>
                      <div className="mt-1 flex flex-col gap-1 text-muted-foreground">
                        {result.errors.map((error) => (
                          <span key={`${error.code}:${error.message}`}>
                            {error.code}: {error.message}
                          </span>
                        ))}
                      </div>
                    </div>
                  ))}
                </div>
              ) : null}
              <div className="mt-3 flex flex-col gap-2">
                {config.pipeline.length === 0 ? (
                  <div className="rounded-md border border-dashed bg-background/40 px-3 py-3 text-xs text-muted-foreground">
                    No pipeline entries.
                  </div>
                ) : (
                  <ol className="flex flex-col gap-2">
                    {config.pipeline.map((worker, index) => (
                      <li key={`${index}:${worker}`} className="flex items-center gap-2">
                        <span className="w-6 text-right font-mono text-[11px] text-muted-foreground">
                          {index + 1}.
                        </span>
                        <div className="min-w-0 flex-1">{renderWorkerSelect(index)}</div>
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon"
                          className="size-8"
                          disabled={index === 0 || savingConfig}
                          onClick={() => movePipelineWorker(index, -1)}
                          aria-label={`Move pipeline entry ${index + 1} up`}
                        >
                          <ArrowUp className="size-3.5" />
                        </Button>
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon"
                          className="size-8"
                          disabled={index === config.pipeline.length - 1 || savingConfig}
                          onClick={() => movePipelineWorker(index, 1)}
                          aria-label={`Move pipeline entry ${index + 1} down`}
                        >
                          <ArrowDown className="size-3.5" />
                        </Button>
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon"
                          className="size-8 text-destructive hover:text-destructive"
                          disabled={savingConfig}
                          onClick={() => removePipelineWorker(index)}
                          aria-label={`Remove pipeline entry ${index + 1}`}
                        >
                          <Trash2 className="size-3.5" />
                        </Button>
                      </li>
                    ))}
                  </ol>
                )}
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  className="w-fit"
                  onClick={addPipelineWorker}
                  disabled={savingConfig}
                >
                  <Plus data-icon="inline-start" />
                  Add worker
                </Button>
              </div>
            </div>

            <SettingRow
              title="Test command"
              hint="Injected as {{project.test_cmd}} in implementer prompts."
              htmlFor="cfg-test"
            >
              <input
                id="cfg-test"
                className="input mono"
                placeholder="cargo test"
                value={config.testCmd}
                onChange={(e) => setConfig((c) => ({ ...c, testCmd: e.target.value }))}
              />
            </SettingRow>
            <SettingRow title="Lint command" htmlFor="cfg-lint">
              <input
                id="cfg-lint"
                className="input mono"
                placeholder="cargo clippy"
                value={config.lintCmd}
                onChange={(e) => setConfig((c) => ({ ...c, lintCmd: e.target.value }))}
              />
            </SettingRow>
            <SettingRow title="Build command" htmlFor="cfg-build">
              <input
                id="cfg-build"
                className="input mono"
                placeholder="cargo build"
                value={config.buildCmd}
                onChange={(e) => setConfig((c) => ({ ...c, buildCmd: e.target.value }))}
              />
            </SettingRow>
            <SettingRow title="Default branch" htmlFor="cfg-branch">
              <input
                id="cfg-branch"
                className="input mono"
                placeholder="main"
                value={config.defaultBranch}
                onChange={(e) => setConfig((c) => ({ ...c, defaultBranch: e.target.value }))}
              />
            </SettingRow>

            <div className="flex items-center justify-between gap-3 px-4 py-3">
              <span className="text-xs text-muted-foreground">
                {dirty ? 'Unsaved changes' : 'No changes'}
              </span>
              <div className="flex items-center gap-2">
                {dirty ? (
                  <Button type="button" variant="ghost" size="sm" onClick={() => setConfig(parsedConfig)} disabled={savingConfig}>
                    Discard
                  </Button>
                ) : null}
                <Button type="button" size="sm" onClick={() => void saveProjectConfig()} disabled={!dirty || savingConfig}>
                  {savingConfig ? 'Saving…' : 'Save to config.org'}
                </Button>
              </div>
            </div>
          </>
        )}
      </ScopeSection>

      <ScopeSection
        tone="device"
        icon={<HardDrive className="size-4" />}
        title="This device"
        provenance="Local preferences stored on this machine. Not saved to any project or shared."
      >
        <SettingRow title="Theme" htmlFor="set-theme">
          <select
            id="set-theme"
            className="input"
            value={preference}
            onChange={(e) => setPreference(e.target.value as ThemePreference)}
          >
            {THEME_OPTIONS.map((option) => (
              <option key={option.value} value={option.value}>
                {option.label}
              </option>
            ))}
          </select>
        </SettingRow>

        <SettingRow
          title="Update"
          hint={updateMessage ?? 'Check for a newer orgasmic build and runtime.'}
        >
          <div className="flex flex-col gap-2">
            <div className="flex items-center gap-2">
              <Select
                value={updateChannel}
                onValueChange={(value) => setUpdateChannel(value as UpdateChannel)}
              >
                <SelectTrigger className="h-8 w-36 text-xs">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent align="start">
                  {UPDATE_CHANNELS.map((channel) => (
                    <SelectItem key={channel} value={channel} className="text-xs capitalize">
                      {channel}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              {updateStatus === 'available' ? <Badge>Available</Badge> : null}
              {updateStatus === 'blocked' ? <Badge variant="outline">Blocked</Badge> : null}
            </div>
            <div className="flex flex-wrap gap-2">
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={updateStatus === 'checking' || updateStatus === 'installing'}
                onClick={() => void checkForUpdate()}
              >
                <RefreshCw data-icon="inline-start" />
                {updateStatus === 'checking' ? 'Checking…' : 'Check for updates'}
              </Button>
              <Button
                type="button"
                size="sm"
                disabled={!pendingUpdate || updateStatus === 'checking' || updateStatus === 'installing'}
                onClick={() => void installUpdate()}
              >
                <Download data-icon="inline-start" />
                {updateStatus === 'installing'
                  ? pendingUpdate?.platform === 'android-sideload'
                    ? 'Opening…'
                    : 'Installing…'
                  : pendingUpdate?.platform === 'android-sideload'
                    ? 'Download APK'
                    : 'Install update'}
              </Button>
            </div>
          </div>
        </SettingRow>

        <SettingRow
          title="Backend connection"
          hint={
            testResult ?? (
              <>
                Daemon the UI talks to. Dev server proxies to{' '}
                <code className="font-mono text-[11px]">127.0.0.1:8739</code>.
              </>
            )
          }
        >
          <div className="flex flex-col gap-2">
            <div className="flex items-center gap-2">
              <select
                className="input"
                value={activeProfileId}
                onChange={(e) => setActiveProfile(e.target.value)}
              >
                {profiles.map((profile) => (
                  <option key={profile.id} value={profile.id}>
                    {profile.name}
                  </option>
                ))}
              </select>
              <Button type="button" variant="outline" size="sm" onClick={() => void runTest()}>
                Test
              </Button>
            </div>
            <input
              className="input"
              aria-label="Base URL"
              placeholder="Base URL"
              value={activeProfile.baseUrl}
              onChange={(e) => updateProfile(activeProfile.id, { baseUrl: e.target.value })}
            />
            <input
              className="input mono"
              type="password"
              aria-label="Bearer token"
              placeholder="Bearer token — from $ORGASMIC_HOME/user/auth/token"
              value={activeProfile.token ?? ''}
              onChange={(e) => updateProfile(activeProfile.id, { token: e.target.value || null })}
            />
            {activeProfile.lastConnectedAt ? (
              <p className="text-xs text-muted-foreground">Last connected: {activeProfile.lastConnectedAt}</p>
            ) : null}
          </div>
        </SettingRow>

        <SettingRow title="Add remote profile" hint="Point at another daemon by URL.">
          <div className="flex flex-col gap-2">
            <input
              className="input"
              aria-label="Profile name"
              placeholder="Name"
              value={newName}
              onChange={(e) => setNewName(e.target.value)}
            />
            <input
              className="input"
              aria-label="Remote base URL"
              placeholder="Base URL"
              value={newUrl}
              onChange={(e) => setNewUrl(e.target.value)}
            />
            <Button
              type="button"
              size="sm"
              variant="outline"
              disabled={!newName.trim()}
              onClick={() => {
                addProfile({ name: newName.trim(), baseUrl: newUrl.trim(), token: null });
                setNewName('');
              }}
            >
              Add profile
            </Button>
          </div>
        </SettingRow>
      </ScopeSection>
    </div>
  );
}

function ScopeSection({
  tone,
  icon,
  title,
  badge,
  provenance,
  children,
}: {
  tone: 'project' | 'device';
  icon: ReactNode;
  title: string;
  badge?: string;
  provenance: ReactNode;
  children: ReactNode;
}) {
  const isProject = tone === 'project';
  return (
    <section className="overflow-hidden rounded-lg border bg-card">
      <header className={cn('border-b px-4 py-3', isProject ? 'bg-accent/60' : 'bg-muted/60')}>
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <span className={isProject ? 'text-primary' : 'text-muted-foreground'}>{icon}</span>
          <h3 className="text-sm font-semibold tracking-tight text-foreground">{title}</h3>
          {badge ? (
            <Badge variant="outline" className="h-5 bg-background/70 px-1.5 font-mono text-[11px]">
              {badge}
            </Badge>
          ) : null}
        </div>
        <p className="mt-1 text-xs text-muted-foreground">{provenance}</p>
      </header>
      <div className="divide-y divide-border">{children}</div>
    </section>
  );
}

function SettingRow({
  title,
  hint,
  htmlFor,
  children,
}: {
  title: string;
  hint?: ReactNode;
  htmlFor?: string;
  children: ReactNode;
}) {
  return (
    <div className="grid gap-x-6 gap-y-2 px-4 py-3.5 md:grid-cols-[minmax(0,1fr)_minmax(0,22rem)] md:items-start">
      <div className="min-w-0">
        <label htmlFor={htmlFor} className="text-sm font-medium text-foreground">
          {title}
        </label>
        {hint ? <p className="mt-0.5 text-xs text-muted-foreground">{hint}</p> : null}
      </div>
      <div className="md:w-full">{children}</div>
    </div>
  );
}

function EmptyRow({ children }: { children: ReactNode }) {
  return <div className="px-4 py-8 text-center text-sm text-muted-foreground">{children}</div>;
}
