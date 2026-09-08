import type { PluginContext } from './pluginRuntime';

export type MediaAnchor = { attachment: string; revision: string; start_ms: number; end_ms?: number | null; label: string };
export type NodeLink = { id: string; source: string; target: string; kind: string; revision: number; deleted: boolean; anchors: MediaAnchor[] };
export type Attachment = { id: string; node: string; name: string; revision: string; size: number; media_type: string };

export function mediaTime(ms: number): string {
  const seconds = Math.floor(ms / 1000);
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
}

export function mediaSearch(source: string, anchor?: MediaAnchor) {
  return { media_node: source, media_attachment: anchor?.attachment, media_revision: anchor?.revision, media_ms: anchor?.start_ms };
}

// The id is a resume handle, not a credential. A lost chunk reply is resolved
// by GET on retry; the server owns the confirmed offset.
export async function uploadAttachment(ctx: PluginContext, node: string, file: File, id: string,
  progress: (offset: number) => void, cancelled: () => boolean = () => false): Promise<Attachment> {
  const path = `/attachments/uploads/${encodeURIComponent(id)}`;
  const mediaType = file.type === 'audio/x-wav' || (!file.type && /\.wav$/i.test(file.name)) ? 'audio/wav' : file.type;
  await ctx.post('/attachments/uploads', { node, name: file.name, size: file.size, media_type: mediaType, request_id: id });
  const upload = await ctx.get<{ offset: number; complete: Attachment | null }>(path);
  if (upload.complete) return upload.complete;
  let offset = upload.offset;
  if (!Number.isSafeInteger(offset) || offset < 0 || offset > file.size) throw new Error('Invalid confirmed upload offset');
  progress(offset);
  while (offset < file.size) {
    ctx.signal.throwIfAborted();
    if (cancelled()) throw new DOMException('Upload paused', 'AbortError');
    const chunk = file.slice(offset, Math.min(offset + 4 * 1024 * 1024, file.size));
    const hash = await crypto.subtle.digest('SHA-256', await chunk.arrayBuffer());
    const checksum = Array.from(new Uint8Array(hash), (b) => b.toString(16).padStart(2, '0')).join('');
    const confirmed = await ctx.putBytes<{ offset: number }>(`${path}?offset=${offset}`, chunk, checksum);
    if (confirmed.offset !== offset + chunk.size) throw new Error('Unexpected upload offset; retry to resume');
    offset = confirmed.offset;
    progress(offset);
  }
  if (cancelled()) throw new DOMException('Upload paused', 'AbortError');
  return ctx.post<Attachment>(`${path}/finish`, {});
}
