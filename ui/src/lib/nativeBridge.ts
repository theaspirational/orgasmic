import type { BackendProfile } from './backend';

export function isTauriRuntime(): boolean {
  return Boolean((window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__);
}

export function canUseNativeDirectoryPicker(profile: BackendProfile): boolean {
  return isTauriRuntime() && profile.id === 'local';
}

export async function pickLocalDirectory(): Promise<string | null> {
  if (!isTauriRuntime()) return null;
  const dialog = await import('@tauri-apps/plugin-dialog');
  const selected = await dialog.open({
    directory: true,
    multiple: false,
  });
  return typeof selected === 'string' ? selected : null;
}
