import { beforeEach, describe, expect, it, vi } from 'vitest';

const { get, post } = vi.hoisted(() => ({ get: vi.fn(), post: vi.fn() }));
vi.mock('@/lib/transport', () => ({
  get: (...args: unknown[]) => get(...args),
  post: (...args: unknown[]) => post(...args),
  getWithHeader: vi.fn(),
  HttpError: class HttpError extends Error {},
}));

import { findNodeConversation, postConversationCreate, postConversationInput } from '../api';

beforeEach(() => {
  get.mockReset();
  post.mockReset();
});

describe('conversation routes', () => {
  it('creates with the project in the query and a request id in the body', async () => {
    post.mockResolvedValue({ id: 'CONV-1', run_id: 'run-1', mode: 'cold' });
    await postConversationCreate('demo', { purpose: 'discuss', node: 'dec_1', provider: 'claude', message: 'hi' });
    expect(post).toHaveBeenCalledWith('/conversations?project=demo', expect.objectContaining({
      purpose: 'discuss', node: 'dec_1', provider: 'claude', message: 'hi', request_id: expect.stringMatching(/^ui-conversation-/),
    }));
  });

  it('continues through /conversations/:id/input with the scope chip', async () => {
    post.mockResolvedValue({ run_id: 'run-2', mode: 'resumed' });
    const result = await postConversationInput('CONV-1', 'demo', { message: 'more', context: [{ kind: 'node', id: 'dec_1' }] });
    expect(result.mode).toBe('resumed');
    expect(post).toHaveBeenCalledWith('/conversations/CONV-1/input?project=demo', expect.objectContaining({
      message: 'more', context: [{ kind: 'node', id: 'dec_1' }], request_id: expect.any(String),
    }));
  });
});

describe('findNodeConversation', () => {
  const conversation = (id: string, todo: string, created: string) => ({
    id, todo, properties: [{ key: 'CREATED_AT', value: created }],
  });

  it('follows incoming CONV- backlinks to the newest OPEN conversation', async () => {
    get.mockImplementation(async (path: string) => {
      if (path === '/links?project=demo&node=dec_1&incoming=true') {
        return [
          { id: 'l1', source: 'CONV-OLD', target: 'dec_1', kind: 'RELATES_TO', deleted: false, anchors: [] },
          { id: 'l2', source: 'TASK-9', target: 'dec_1', kind: 'RELATES_TO', deleted: false, anchors: [] },
          { id: 'l3', source: 'CONV-NEW', target: 'dec_1', kind: 'RELATES_TO', deleted: false, anchors: [] },
          { id: 'l4', source: 'CONV-GONE', target: 'dec_1', kind: 'RELATES_TO', deleted: true, anchors: [] },
        ];
      }
      if (path === '/org/node?project=demo&id=CONV-OLD') return conversation('CONV-OLD', 'OPEN', '2026-09-01T00:00:00Z');
      if (path === '/org/node?project=demo&id=CONV-NEW') return conversation('CONV-NEW', 'OPEN', '2026-09-09T00:00:00Z');
      throw new Error(`unexpected ${path}`);
    });
    expect(await findNodeConversation('demo', 'dec_1')).toBe('CONV-NEW');
    expect(get).not.toHaveBeenCalledWith('/org/node?project=demo&id=TASK-9');
    expect(get).not.toHaveBeenCalledWith('/org/node?project=demo&id=CONV-GONE');
  });

  it('returns null when every conversation is archived or missing so the caller shows setup', async () => {
    get.mockImplementation(async (path: string) => {
      if (path.startsWith('/links')) return [
        { id: 'l1', source: 'CONV-A', target: 'dec_1', kind: 'RELATES_TO', deleted: false, anchors: [] },
        { id: 'l2', source: 'CONV-B', target: 'dec_1', kind: 'RELATES_TO', deleted: false, anchors: [] },
      ];
      if (path.endsWith('CONV-A')) return conversation('CONV-A', 'ARCHIVED', '2026-09-09T00:00:00Z');
      throw new Error('404');
    });
    expect(await findNodeConversation('demo', 'dec_1')).toBeNull();
  });
});
