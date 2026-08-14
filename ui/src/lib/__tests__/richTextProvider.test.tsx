// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const mocks = vi.hoisted(() => ({
  fetchGlossary: vi.fn(),
  navigate: vi.fn(),
}));

vi.mock('@tanstack/react-router', () => ({
  useNavigate: () => mocks.navigate,
  useRouterState: ({ select }: { select: (state: { location: { pathname: string } }) => unknown }) =>
    select({ location: { pathname: '/projects/vscode-orsl/tasks' } }),
}));

vi.mock('@/lib/api', () => ({ fetchGlossary: mocks.fetchGlossary }));

import { useRefreshBump, RefreshProvider } from '@/hooks/useRefreshBus';
import { DecoratedText, RichTextProvider } from '../richText';

function Harness() {
  const bumpRefresh = useRefreshBump();
  return (
    <>
      <p>
        <DecoratedText text="Remove the emission settings." />
      </p>
      <button type="button" onClick={bumpRefresh}>
        Refresh
      </button>
    </>
  );
}

describe('RichTextProvider glossary freshness', () => {
  beforeEach(() => {
    mocks.fetchGlossary.mockReset();
    mocks.navigate.mockReset();
  });

  afterEach(() => cleanup());

  it('stops linking a glossary phrase after the shared refresh signal', async () => {
    mocks.fetchGlossary
      .mockResolvedValueOnce([{ id: 'term_REMOVE', canonical: 'Remove' }])
      .mockResolvedValueOnce([]);

    render(
      <RefreshProvider>
        <RichTextProvider projectId="vscode-orsl">
          <Harness />
        </RichTextProvider>
      </RefreshProvider>,
    );

    expect(await screen.findByRole('button', { name: 'Remove' })).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Refresh' }));

    await waitFor(() => expect(mocks.fetchGlossary).toHaveBeenCalledTimes(2));
    await waitFor(() =>
      expect(screen.queryByRole('button', { name: 'Remove' })).not.toBeInTheDocument(),
    );
    expect(screen.getByText('Remove the emission settings.')).toBeInTheDocument();
  });
});
