// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

const fetchManagerChatCatalogMock = vi.fn();

vi.mock('@/lib/api', () => ({
  fetchManagerChatCatalog: (...args: unknown[]) => fetchManagerChatCatalogMock(...args),
  fetchSkills: vi.fn().mockResolvedValue([]),
}));

import { ChatSetup } from '../ChatSetup';

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe('ChatSetup', () => {
  it('identifies an unavailable chat-catalog route as a daemon version mismatch', async () => {
    fetchManagerChatCatalogMock.mockRejectedValue(new Error('api route not found'));

    render(<ChatSetup projectId="orgasmic" readOnly={false} onStart={vi.fn()} />);

    expect(
      (await screen.findAllByText(/update or restart the Orgasmic daemon/i)).length,
    ).toBeGreaterThan(0);
    expect(screen.queryByText(/Install or sign in/i)).not.toBeInTheDocument();
  });

  it('keeps provider setup guidance for a successful but empty catalog', async () => {
    fetchManagerChatCatalogMock.mockResolvedValue({
      providers: [
        { id: 'codex', source: 'test', models: [], message: 'not signed in' },
        { id: 'claude', source: 'test', models: [], message: 'not signed in' },
        { id: 'opencode', source: 'test', models: [], message: 'not signed in' },
      ],
    });

    render(<ChatSetup projectId="orgasmic" readOnly={false} onStart={vi.fn()} />);

    expect((await screen.findAllByText(/Install or sign in/i)).length).toBeGreaterThan(0);
  });
});
