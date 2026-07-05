// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('@tanstack/react-router', () => ({
  useNavigate: () => vi.fn(),
  useParams: () => ({ artifactId: 'ART-1' }),
  useSearch: () => ({}),
}));

vi.mock('@/hooks/useEventStream', () => ({
  useEventStream: () => undefined,
}));

const fetchArtifactMock = vi.fn();
const regenerateArtifactMock = vi.fn();
const postArtifactCommentMock = vi.fn();
const resolveArtifactCommentMock = vi.fn();
vi.mock('@/lib/api', () => ({
  fetchArtifact: (...args: unknown[]) => fetchArtifactMock(...args),
  regenerateArtifact: (...args: unknown[]) => regenerateArtifactMock(...args),
  postArtifactComment: (...args: unknown[]) => postArtifactCommentMock(...args),
  resolveArtifactComment: (...args: unknown[]) => resolveArtifactCommentMock(...args),
}));

import { ArtifactView } from '../ArtifactView';
import type { ArtifactDetail } from '@/lib/types';

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

function detail(overrides: Partial<ArtifactDetail> = {}): ArtifactDetail {
  return {
    id: 'ART-1',
    title: 'Login flow wireframe',
    subject_nodes: [],
    version: 1,
    state: 'submitted',
    open_comment_count: 0,
    prompt: 'Draft a login flow',
    content: '<RichText>hello</RichText>',
    comments: [],
    ...overrides,
  };
}

describe('ArtifactView regenerate', () => {
  it('threads a typed extra prompt into the regenerate POST body', async () => {
    fetchArtifactMock.mockResolvedValue(detail());
    regenerateArtifactMock.mockResolvedValue({ artifact_id: 'ART-1', run_id: 'run-1' });

    render(<ArtifactView projectId="proj1" />);
    await screen.findByText('Login flow wireframe');

    fireEvent.click(screen.getByRole('button', { name: /Regenerate/ }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.change(
      within(dialog).getByPlaceholderText('Anything extra to steer this regeneration…'),
      { target: { value: 'Make it punchier' } },
    );
    fireEvent.click(within(dialog).getByRole('button', { name: 'Regenerate' }));

    await waitFor(() => expect(regenerateArtifactMock).toHaveBeenCalled());
    expect(regenerateArtifactMock).toHaveBeenCalledWith(
      'ART-1',
      { extraPrompt: 'Make it punchier' },
      'proj1',
    );
  });

  it('sends no extraPrompt when the field is left empty', async () => {
    fetchArtifactMock.mockResolvedValue(detail());
    regenerateArtifactMock.mockResolvedValue({ artifact_id: 'ART-1', run_id: 'run-1' });

    render(<ArtifactView projectId="proj1" />);
    await screen.findByText('Login flow wireframe');

    fireEvent.click(screen.getByRole('button', { name: /Regenerate/ }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Regenerate' }));

    await waitFor(() => expect(regenerateArtifactMock).toHaveBeenCalled());
    expect(regenerateArtifactMock).toHaveBeenCalledWith('ART-1', {}, 'proj1');
  });

  it('trims whitespace-only input down to no extraPrompt', async () => {
    fetchArtifactMock.mockResolvedValue(detail());
    regenerateArtifactMock.mockResolvedValue({ artifact_id: 'ART-1', run_id: 'run-1' });

    render(<ArtifactView projectId="proj1" />);
    await screen.findByText('Login flow wireframe');

    fireEvent.click(screen.getByRole('button', { name: /Regenerate/ }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.change(
      within(dialog).getByPlaceholderText('Anything extra to steer this regeneration…'),
      { target: { value: '   ' } },
    );
    fireEvent.click(within(dialog).getByRole('button', { name: 'Regenerate' }));

    await waitFor(() => expect(regenerateArtifactMock).toHaveBeenCalled());
    expect(regenerateArtifactMock).toHaveBeenCalledWith('ART-1', {}, 'proj1');
  });

  it('locks the comment composer and shows the regenerating banner', async () => {
    fetchArtifactMock.mockResolvedValue(detail({ state: 'regenerating' }));

    render(<ArtifactView projectId="proj1" />);
    await screen.findByText('Login flow wireframe');

    expect(screen.getByRole('status')).toHaveTextContent(/regenerating/i);
    expect(screen.getByPlaceholderText('Leave a comment…')).toBeDisabled();
  });

  it('leaves the composer open and shows no banner outside regeneration', async () => {
    fetchArtifactMock.mockResolvedValue(detail({ state: 'submitted' }));

    render(<ArtifactView projectId="proj1" />);
    await screen.findByText('Login flow wireframe');

    expect(screen.queryByRole('status')).toBeNull();
    expect(screen.getByPlaceholderText('Leave a comment…')).toBeEnabled();
  });
});
