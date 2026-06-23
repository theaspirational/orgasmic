import { getVersion } from '@tauri-apps/api/app';
import { invoke, isTauri } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-shell';

export const UPDATE_CHANNELS = ['stable', 'nightly'] as const;
export const UPDATE_CHANNEL_STORAGE_KEY = 'orgasmic:update-channel';
export const UPDATE_LAST_NOTIFIED_KEY = 'orgasmic:last-notified-update';
export const UPDATE_AUTO_CHECK_MS = 15 * 60 * 1000;
const UPDATE_REPO = 'theaspirational/orgasmic';

export type UpdateChannel = (typeof UPDATE_CHANNELS)[number];
export type AppUpdatePlatform = 'desktop' | 'android-sideload';

export type AppUpdateMetadata = {
  channel: UpdateChannel;
  currentVersion: string;
  version: string;
  platform: AppUpdatePlatform;
  downloadUrl?: string;
  notes?: string;
  pubDate?: string;
  apkSha256?: string;
  versionCode?: number;
};

type DesktopAppUpdateMetadata = Omit<AppUpdateMetadata, 'platform'>;

function isMobileUserAgent(): boolean {
  return typeof navigator !== 'undefined' && /Android|iPhone|iPad|iPod/i.test(navigator.userAgent);
}

export function isAndroidTauriApp(): boolean {
  return isTauri() && typeof navigator !== 'undefined' && /Android/i.test(navigator.userAgent);
}

export function isDesktopTauriApp(): boolean {
  if (!isTauri()) return false;
  return !isMobileUserAgent();
}

export function supportsAppUpdateChecks(): boolean {
  return isDesktopTauriApp() || isAndroidTauriApp();
}

export function savedUpdateChannel(): UpdateChannel {
  const saved = window.localStorage.getItem(UPDATE_CHANNEL_STORAGE_KEY);
  return UPDATE_CHANNELS.includes(saved as UpdateChannel) ? (saved as UpdateChannel) : 'stable';
}

export function saveUpdateChannel(channel: UpdateChannel) {
  window.localStorage.setItem(UPDATE_CHANNEL_STORAGE_KEY, channel);
}

export async function checkAppUpdate(channel: UpdateChannel): Promise<AppUpdateMetadata | null> {
  if (isAndroidTauriApp()) return checkAndroidSideloadUpdate(channel);
  if (!isDesktopTauriApp()) return null;

  const update = await invoke<DesktopAppUpdateMetadata | null>('check_app_update', { channel });
  return update ? { ...update, platform: 'desktop' } : null;
}

export async function installAppUpdate(update?: AppUpdateMetadata | null): Promise<void> {
  if (update?.platform === 'android-sideload') {
    if (!update.downloadUrl) throw new Error('Android update has no APK download URL');
    await open(update.downloadUrl);
    return;
  }

  await invoke('install_app_update');
  // Move the runtime onto the same channel the app was just updated on, so the
  // app + its CLI/daemon runtime track one channel together.
  if (isDesktopTauriApp()) {
    await invoke<string>('update_runtime', { channel: update?.channel ?? 'stable' });
  }
}

// Version lives in android-latest.json (dec_B4147), not the APK filename — the
// APK is the version-less orgasmic_android_aarch64.apk. Read the manifest for the
// version + signed apkUrl rather than scraping release assets by name.
type AndroidManifest = {
  version?: string;
  versionCode?: number;
  apkUrl?: string;
  notes?: string;
  pubDate?: string;
};

async function checkAndroidSideloadUpdate(channel: UpdateChannel): Promise<AppUpdateMetadata | null> {
  const currentVersion = await getVersion();
  const manifest = await fetchAndroidManifest(channel);
  if (!manifest) return null;

  const { version, apkUrl } = manifest;
  if (!version || !apkUrl) {
    throw new Error('android-latest.json must carry both version and apkUrl');
  }
  if (version === currentVersion) return null;

  return {
    channel,
    currentVersion,
    version,
    platform: 'android-sideload',
    downloadUrl: apkUrl,
    notes: manifest.notes ?? undefined,
    pubDate: manifest.pubDate ?? undefined,
    versionCode: typeof manifest.versionCode === 'number' ? manifest.versionCode : undefined,
  };
}

async function fetchAndroidManifest(channel: UpdateChannel): Promise<AndroidManifest | null> {
  const url = `https://github.com/${UPDATE_REPO}/releases/download/${channel}/android-latest.json`;
  const response = await fetch(url, { cache: 'no-store' });
  if (response.status === 404) return null;
  if (!response.ok) {
    throw new Error(`Android manifest request failed: ${response.status} ${response.statusText}`);
  }
  return response.json() as Promise<AndroidManifest>;
}
