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

type GitHubReleaseAsset = {
  name: string;
  browser_download_url: string;
};

type GitHubRelease = {
  body?: string | null;
  published_at?: string | null;
  assets?: GitHubReleaseAsset[];
};

const ANDROID_APK_ASSET_PATTERN = /^orgasmic_(?<version>[^_]+)_(?<versionCode>\d+)_android_[^/]+\.apk$/;

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

async function checkAndroidSideloadUpdate(channel: UpdateChannel): Promise<AppUpdateMetadata | null> {
  const currentVersion = await getVersion();
  const release = await fetchGitHubRelease(channel);
  if (!release) return null;

  const apk = findAndroidApkAsset(release.assets ?? []);
  if (!apk) return null;

  const match = ANDROID_APK_ASSET_PATTERN.exec(apk.name);
  const version = match?.groups?.version;
  if (!version) {
    throw new Error(`Android APK asset name must match orgasmic_<version>_<versionCode>_android_<target>.apk`);
  }
  if (version === currentVersion) return null;

  return {
    channel,
    currentVersion,
    version,
    platform: 'android-sideload',
    downloadUrl: apk.browser_download_url,
    notes: release.body ?? undefined,
    pubDate: release.published_at ?? undefined,
    versionCode: Number(match?.groups?.versionCode),
  };
}

async function fetchGitHubRelease(channel: UpdateChannel): Promise<GitHubRelease | null> {
  const url = `https://api.github.com/repos/${UPDATE_REPO}/releases/tags/${channel}`;
  const response = await fetch(url, { cache: 'no-store' });
  if (response.status === 404) return null;
  if (!response.ok) {
    throw new Error(`GitHub release request failed: ${response.status} ${response.statusText}`);
  }
  return response.json() as Promise<GitHubRelease>;
}

function findAndroidApkAsset(assets: GitHubReleaseAsset[]): GitHubReleaseAsset | null {
  return (
    assets.find((asset) => ANDROID_APK_ASSET_PATTERN.test(asset.name)) ??
    assets.find((asset) => asset.name.includes('_android_') && asset.name.endsWith('.apk')) ??
    null
  );
}
