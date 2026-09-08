// @vitest-environment jsdom
import { webcrypto } from 'node:crypto';
import { Blob as NodeBlob, File as NodeFile } from 'node:buffer';
import { afterEach, expect, it, vi } from 'vitest';
import { mediaSearch, mediaTime, uploadAttachment } from '../nodeServices';
import { requestWithProfile } from '../transport';
import type { PluginContext } from '../pluginRuntime';

afterEach(() => vi.unstubAllGlobals());
it('resumes at the confirmed offset with bounded checksummed chunks and preserves binary transport', async () => {
  vi.stubGlobal('crypto', webcrypto);
  vi.stubGlobal('Blob', NodeBlob);
  const file = new NodeFile([new Uint8Array(9 * 1024 * 1024)], 'recording.wav', { type: 'audio/wav' }) as unknown as File;
  const post = vi.fn().mockResolvedValue({ id: 'upload' });
  const putBytes = vi.fn(async (path: string, chunk: Blob, checksum: string) => {
    expect(chunk.size).toBeLessThanOrEqual(4 * 1024 * 1024);
    expect(checksum).toMatch(/^[a-f0-9]{64}$/);
    return { offset: Number(new URL(path, 'http://test').searchParams.get('offset')) + chunk.size };
  });
  const ctx = { signal: new AbortController().signal, post, putBytes, get: vi.fn().mockResolvedValue({ offset: 4 * 1024 * 1024, complete: null }) } as unknown as PluginContext;
  await uploadAttachment(ctx, 'MEET-1', file, 'upload', vi.fn());
  expect(putBytes).toHaveBeenCalledTimes(2);
  expect(post).toHaveBeenLastCalledWith('/attachments/uploads/upload/finish', {});
  putBytes.mockClear();
  await expect(uploadAttachment(ctx, 'MEET-1', file, 'upload', vi.fn(), () => true)).rejects.toMatchObject({ name: 'AbortError' });
  expect(putBytes).not.toHaveBeenCalled();
  const fetch = vi.fn().mockResolvedValue({ ok: true, json: async () => ({}) });
  vi.stubGlobal('fetch', fetch);
  const chunk = file.slice(0, 42);
  await requestWithProfile({ baseUrl: 'http://daemon', token: 'test-token' }, '/attachments/uploads/id', { method: 'PUT', body: chunk, chunkSha256: 'digest', signal: ctx.signal });
  expect(fetch.mock.calls[0][1]).toMatchObject({ body: chunk, signal: ctx.signal, headers: { 'content-type': 'application/octet-stream', 'x-chunk-sha256': 'digest' } });
  expect(mediaTime(61000)).toBe('1:01');
  expect(mediaSearch('MEET-1', { attachment: 'a', revision: 'r', start_ms: 1000, label: '' })).toEqual({ media_node: 'MEET-1', media_attachment: 'a', media_revision: 'r', media_ms: 1000 });
});
