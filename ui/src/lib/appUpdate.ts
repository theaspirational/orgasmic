import { getVersion } from '@tauri-apps/api/app';
import { invoke, isTauri } from '@tauri-apps/api/core';
import { openUrl } from '@tauri-apps/plugin-opener';

export const UPDATE_CHANNELS = ['stable', 'nightly'] as const;
export const UPDATE_CHANNEL_STORAGE_KEY = 'orgasmic:update-channel';
export const UPDATE_LAST_NOTIFIED_KEY = 'orgasmic:last-notified-update';
export const UPDATE_AUTO_CHECK_MS = 15 * 60 * 1000;

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
  /** True when the offered build is OLDER than what's installed and is only
   *  surfaced because the user explicitly switched channels (e.g. nightly →
   *  stable). Routine/launch checks never surface a downgrade. */
  isDowngrade?: boolean;
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

export async function checkAppUpdate(
  channel: UpdateChannel,
  options: { allowDowngrade?: boolean } = {},
): Promise<AppUpdateMetadata | null> {
  if (isAndroidTauriApp()) {
    return checkAndroidSideloadUpdate(channel, options.allowDowngrade ?? false);
  }
  if (!isDesktopTauriApp()) return null;

  const update = await invoke<DesktopAppUpdateMetadata | null>('check_app_update', { channel });
  return update ? { ...update, platform: 'desktop' } : null;
}

export async function installAppUpdate(update?: AppUpdateMetadata | null): Promise<void> {
  if (update?.platform === 'android-sideload') {
    if (!update.downloadUrl) throw new Error('Android update has no APK download URL');
    // Hand the signed APK URL to the system browser via an ACTION_VIEW intent
    // (the opener plugin). It downloads the APK; the user taps it to install.
    await openUrl(update.downloadUrl);
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

async function checkAndroidSideloadUpdate(
  channel: UpdateChannel,
  allowDowngrade: boolean,
): Promise<AppUpdateMetadata | null> {
  const currentVersion = await getVersion();
  const manifest = await fetchAndroidManifest(channel);
  if (!manifest) return null;

  const { version, apkUrl } = manifest;
  if (!version || !apkUrl) {
    throw new Error('android-latest.json must carry both version and apkUrl');
  }

  // Only nag when the channel actually has something newer. An older build is
  // surfaced solely on an explicit channel switch (e.g. nightly → stable, where
  // stable's semver is legitimately lower) — never on a routine/launch check,
  // otherwise a newer dev build gets told to "downgrade". Same version → nothing.
  const ordering = compareVersions(version, currentVersion);
  if (ordering === 0) return null;
  if (ordering < 0 && !allowDowngrade) return null;

  return {
    channel,
    currentVersion,
    version,
    platform: 'android-sideload',
    downloadUrl: apkUrl,
    notes: manifest.notes ?? undefined,
    pubDate: manifest.pubDate ?? undefined,
    versionCode: typeof manifest.versionCode === 'number' ? manifest.versionCode : undefined,
    isDowngrade: ordering < 0,
  };
}

/** Compare two semver strings by precedence: 1 if `a` > `b`, -1 if `a` < `b`,
 *  0 if equal. Build metadata (`+…`) is ignored; a release outranks a prerelease
 *  of the same core (`0.0.7` > `0.0.7-nightly.x`); prerelease identifiers compare
 *  field-by-field (numeric numerically and below alphanumeric), which orders the
 *  `…-nightly.<date>.<code>` suffixes correctly. */
export function compareVersions(a: string, b: string): number {
  const parse = (v: string) => {
    const clean = v.replace(/\+.*$/, '');
    const dash = clean.indexOf('-');
    const core = dash === -1 ? clean : clean.slice(0, dash);
    const pre = dash === -1 ? '' : clean.slice(dash + 1);
    return { nums: core.split('.').map((n) => parseInt(n, 10) || 0), pre };
  };
  const pa = parse(a);
  const pb = parse(b);

  for (let i = 0; i < Math.max(pa.nums.length, pb.nums.length); i++) {
    const d = (pa.nums[i] ?? 0) - (pb.nums[i] ?? 0);
    if (d !== 0) return d > 0 ? 1 : -1;
  }

  if (!pa.pre && !pb.pre) return 0;
  if (!pa.pre) return 1; // a is a release, b is a prerelease of the same core
  if (!pb.pre) return -1;

  const ai = pa.pre.split('.');
  const bi = pb.pre.split('.');
  for (let i = 0; i < Math.max(ai.length, bi.length); i++) {
    const x = ai[i];
    const y = bi[i];
    if (x === undefined) return -1; // a shorter prerelease has lower precedence
    if (y === undefined) return 1;
    const xn = /^\d+$/.test(x);
    const yn = /^\d+$/.test(y);
    if (xn && yn) {
      const d = parseInt(x, 10) - parseInt(y, 10);
      if (d !== 0) return d > 0 ? 1 : -1;
    } else if (xn !== yn) {
      return xn ? -1 : 1; // numeric identifiers rank below alphanumeric ones
    } else if (x !== y) {
      return x > y ? 1 : -1;
    }
  }
  return 0;
}

// Fetched in the app process via the check_android_update Tauri command (no
// webview CORS). Rust owns the release-tag mapping (stable -> apps-stable,
// nightly -> apps-nightly, mirroring app_release_tag in src-tauri/src/lib.rs)
// and 404 handling. dec_B4147 / dec_PVDP3.
async function fetchAndroidManifest(channel: UpdateChannel): Promise<AndroidManifest | null> {
  return invoke<AndroidManifest | null>('check_android_update', { channel });
}
