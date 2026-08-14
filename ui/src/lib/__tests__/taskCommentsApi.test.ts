import { describe, expect, it, vi } from 'vitest';

const post = vi.fn();
vi.mock('@/lib/transport', () => ({
  get: vi.fn(),
  post: (...args: unknown[]) => post(...args),
  HttpError: class HttpError extends Error {},
}));

import { postTaskComment } from '../api';

describe('task comments api', () => {
  it('posts a project-scoped comment without client-supplied attribution', async () => {
    post.mockResolvedValueOnce({ tx_id: 'tx-1' });

    await postTaskComment('vscode-orsl', 'TASK-A/B', { body: 'Please check this.' });

    expect(post).toHaveBeenCalledWith(
      '/tasks/TASK-A%2FB/comments?project=vscode-orsl',
      expect.objectContaining({
        body: 'Please check this.',
        request_id: expect.stringMatching(/^ui-comment-TASK-A\/B-/),
      }),
    );
    expect(post.mock.calls[0]?.[1]).not.toHaveProperty('actor');
  });
});
