import { beforeEach, describe, expect, it, vi } from 'vitest';

const post = vi.fn();
vi.mock('@/lib/transport', () => ({
  get: vi.fn(),
  post: (...args: unknown[]) => post(...args),
  HttpError: class HttpError extends Error {},
}));

import { deleteTaskComment, editTaskComment, postTaskComment } from '../api';

describe('task comments api', () => {
  beforeEach(() => post.mockReset());

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

  it('edits and deletes a journal comment with the current body as the OCC token', async () => {
    post.mockResolvedValue({ entry_id: 'tx-1', action: 'edited' });

    await editTaskComment('orgasmic', 'TASK-1', 'tx/1', 'Old body', 'New body');
    await deleteTaskComment('orgasmic', 'TASK-1', 'tx/1', 'New body');

    expect(post).toHaveBeenNthCalledWith(
      1,
      '/tasks/TASK-1/comments/tx%2F1/edit?project=orgasmic',
      { expected_body: 'Old body', body: 'New body' },
    );
    expect(post).toHaveBeenNthCalledWith(
      2,
      '/tasks/TASK-1/comments/tx%2F1/delete?project=orgasmic',
      { expected_body: 'New body' },
    );
  });
});
