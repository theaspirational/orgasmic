import { useEffect, useState, type ReactNode } from 'react';
import { relaunch } from '@tauri-apps/plugin-process';
import {
  Download,
  HardDrive,
  RefreshCw,
} from 'lucide-react';
import { toast } from 'sonner';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import { fetchRecoveryStatus } from '@/lib/api';
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
import { cn } from '@/lib/utils';

import { PageHeader } from './Primitives';

type UpdateStatus = 'idle' | 'checking' | 'available' | 'not-available' | 'blocked' | 'installing' | 'error';

function activeRunLabel(runId: string, index: number): string {
  return index < 3 ? runId : '';
}

export function SettingsView({ projectId: _projectId }: { projectId: string | null }) {
  const { profiles, activeProfile, activeProfileId, setActiveProfile, updateProfile, addProfile, testConnection } =
    useBackendProfiles();
  const { preference, setPreference } = useTheme();
  useRefreshToken();
  const [testResult, setTestResult] = useState<string | null>(null);
  const [newName, setNewName] = useState('');
  const [newUrl, setNewUrl] = useState('http://127.0.0.1:8739');
  const [updateStatus, setUpdateStatus] = useState<UpdateStatus>('idle');
  const [updateMessage, setUpdateMessage] = useState<string | null>(null);
  const [pendingUpdate, setPendingUpdate] = useState<AppUpdateMetadata | null>(null);
  const [updateChannel, setUpdateChannel] = useState<UpdateChannel>(() => savedUpdateChannel());

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

  async function runTest() {
    const result = await testConnection();
    setTestResult(result.ok ? `Connected (${result.latencyMs} ms)` : result.error ?? 'Failed');
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

  return (
    <div className="flex flex-col gap-6">
      <PageHeader title="Settings" description="Local app preferences and daemon connection." />

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
            {updateStatus === 'available' && pendingUpdate?.notes ? (
              <p className="line-clamp-6 max-w-md whitespace-pre-wrap text-xs leading-relaxed text-muted-foreground">
                <span className="font-medium text-foreground">
                  Running {pendingUpdate.currentVersion}; {pendingUpdate.version} adds:
                </span>{' '}
                {pendingUpdate.notes}
              </p>
            ) : null}
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
