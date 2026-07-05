// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { CommentCard } from '../ArtifactView';
import type { CommentRecord } from '@/lib/types';

afterEach(cleanup);

function comment(overrides: Partial<CommentRecord> = {}): CommentRecord {
  return {
    cid: 'cmt_1',
    author: 'Dana Reviewer',
    version: 2,
    anchor: 'the second paragraph',
    resolution_target: '',
    resolved: false,
    consumed: false,
    message: 'This section needs a diagram.',
    ...overrides,
  };
}

describe('CommentCard', () => {
  it('renders the comment author name and message', () => {
    render(
      <CommentCard comment={comment()} canComment={false} resolving={false} onToggleResolved={vi.fn()} />,
    );
    expect(screen.getByText('Dana Reviewer')).toBeInTheDocument();
    expect(screen.getByText('This section needs a diagram.')).toBeInTheDocument();
    // Anchored comments quote the pinned selection.
    expect(screen.getByText('the second paragraph')).toBeInTheDocument();
  });

  it('hides the resolve control when the viewer cannot comment', () => {
    render(
      <CommentCard comment={comment()} canComment={false} resolving={false} onToggleResolved={vi.fn()} />,
    );
    expect(screen.queryByRole('button', { name: 'Resolve' })).toBeNull();
  });

  it('offers Resolve for an open comment and Reopen for a resolved one when allowed', () => {
    const { rerender } = render(
      <CommentCard comment={comment()} canComment resolving={false} onToggleResolved={vi.fn()} />,
    );
    expect(screen.getByRole('button', { name: 'Resolve' })).toBeInTheDocument();

    rerender(
      <CommentCard
        comment={comment({ resolved: true })}
        canComment
        resolving={false}
        onToggleResolved={vi.fn()}
      />,
    );
    expect(screen.getByRole('button', { name: 'Reopen' })).toBeInTheDocument();
  });
});
