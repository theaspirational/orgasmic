import { describe, expect, it, vi } from 'vitest';

const post = vi.fn();
vi.mock('@/lib/transport', () => ({
  get: vi.fn(),
  post: (...args: unknown[]) => post(...args),
  HttpError: class HttpError extends Error {},
}));

import { postOrgNodeRegenerate } from '../api';

describe('node regenerate api', () => {
  it('posts to the descriptor-driven node route', async () => {
    post.mockResolvedValue({ node_id: 'TASK-1', run_id: 'run-1' });
    const body = { mode: 'tmux', harness: 'codex', extraPrompt: 'Keep it narrow' };

    await postOrgNodeRegenerate('TASK/1', body, 'orgasmic');

    expect(post).toHaveBeenCalledWith(
      '/org/node/TASK%2F1/regenerate?project=orgasmic',
      body,
    );
  });
});
