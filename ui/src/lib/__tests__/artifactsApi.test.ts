import { describe, expect, it, vi } from 'vitest';

const get = vi.fn();
const post = vi.fn();
vi.mock('@/lib/transport', () => ({
  get: (...args: unknown[]) => get(...args),
  post: (...args: unknown[]) => post(...args),
  HttpError: class HttpError extends Error {},
}));

import { fetchArtifact, fetchArtifacts, generateArtifact, regenerateArtifact } from '../api';

describe('artifacts api', () => {
  it('fetchArtifacts builds the project-scoped path', async () => {
    get.mockResolvedValueOnce([]);
    await fetchArtifacts('orgasmic');
    expect(get).toHaveBeenCalledWith('/artifacts?project=orgasmic');
  });

  it('fetchArtifact omits ?version for the latest read', async () => {
    get.mockResolvedValueOnce({});
    await fetchArtifact('ART-1', 'orgasmic');
    expect(get).toHaveBeenCalledWith('/artifacts/ART-1?project=orgasmic');
  });

  it('fetchArtifact adds ?version for an archived read and encodes the id', async () => {
    get.mockResolvedValueOnce({});
    await fetchArtifact('ART 1/2', 'orgasmic', 3);
    expect(get).toHaveBeenCalledWith('/artifacts/ART%201%2F2?project=orgasmic&version=3');
  });

  it('generateArtifact posts nodes+prompt to the project-scoped generate route', async () => {
    post.mockResolvedValueOnce({ artifact_id: 'ART-1', run_id: 'run-1' });
    await generateArtifact({ nodes: ['dec_ABC12'], prompt: 'Summarize the decision' }, 'orgasmic');
    expect(post).toHaveBeenCalledWith('/artifacts/generate?project=orgasmic', {
      nodes: ['dec_ABC12'],
      prompt: 'Summarize the decision',
    });
  });

  it('generateArtifact allows an empty node set (prompt-only artifact)', async () => {
    post.mockResolvedValueOnce({ artifact_id: 'ART-2', run_id: 'run-2' });
    await generateArtifact({ nodes: [], prompt: 'Prompt only' }, 'orgasmic');
    expect(post).toHaveBeenCalledWith('/artifacts/generate?project=orgasmic', { nodes: [], prompt: 'Prompt only' });
  });

  it('regenerateArtifact posts to the artifact-scoped regenerate route', async () => {
    post.mockResolvedValueOnce({ artifact_id: 'ART-1', run_id: 'run-3' });
    await regenerateArtifact('ART-1', { extraPrompt: 'Add more detail' }, 'orgasmic');
    expect(post).toHaveBeenCalledWith('/artifacts/ART-1/regenerate?project=orgasmic', {
      extraPrompt: 'Add more detail',
    });
  });
});
