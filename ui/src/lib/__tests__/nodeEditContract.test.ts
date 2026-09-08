// @vitest-environment jsdom
import { expect, it, vi } from 'vitest';
const { post } = vi.hoisted(() => ({ post: vi.fn() }));
vi.mock('../transport', async () => ({ ...await vi.importActual<typeof import('../transport')>('../transport'), post }));
import { postOrgNodeEdit } from '../api';

it('requests the full document consumed by the generic editor, not a compact mutation', async () => {
  await postOrgNodeEdit('MEET-1', { baseVersion: 'v1', ops: [{ op: 'set_title', title: 'Revised' }] }, 'demo', 'meetings');
  expect(post).toHaveBeenCalledWith('/org/node/MEET-1/edit?json=true', expect.objectContaining({
    project: 'demo', kind: 'meetings', base_version: 'v1', ops: [{ op: 'set_title', title: 'Revised' }],
  }));
});
