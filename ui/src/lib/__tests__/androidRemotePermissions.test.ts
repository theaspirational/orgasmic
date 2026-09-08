import { readFileSync } from 'node:fs';
import { expect, it } from 'vitest';

it('lets remote Android backends check updates without local runtime authority', () => {
  const capability = JSON.parse(readFileSync(new URL('../../../../src-tauri/capabilities/android-remote-updates.json', import.meta.url), 'utf8'));
  expect(capability.platforms).toEqual(['android']);
  expect(capability.windows).toEqual(['main']);
  expect(capability.local).toBe(false);
  expect(capability.remote.urls).toEqual(['http://*/*', 'https://*/*']);
  expect(capability.permissions).toEqual([
    'core:app:allow-version',
    'allow-check-android-update',
    {
      identifier: 'opener:allow-open-url',
      allow: ['stable', 'nightly'].map(channel => ({
        url: `https://github.com/theaspirational/orgasmic/releases/download/apps-${channel}/orgasmic_android_aarch64.apk`,
      })),
    },
  ]);
});
