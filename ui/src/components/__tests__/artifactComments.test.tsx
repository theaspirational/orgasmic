// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

const apiMocks = vi.hoisted(() => ({
  postArtifactComment: vi.fn(),
  resolveArtifactComment: vi.fn(),
}));

vi.mock('@/lib/api', async (importOriginal) => ({
  ...(await importOriginal<typeof import('@/lib/api')>()),
  postArtifactComment: (...args: unknown[]) => apiMocks.postArtifactComment(...args),
  resolveArtifactComment: (...args: unknown[]) => apiMocks.resolveArtifactComment(...args),
}));

import { ArtifactComments, CommentCard } from '../ArtifactView';
import type { ArtifactDetail, CommentRecord } from '@/lib/types';

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  window.getSelection()?.removeAllRanges();
});

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

function detail(overrides: Partial<ArtifactDetail> = {}): ArtifactDetail {
  return {
    id: 'ART-1',
    title: 'Selection artifact',
    subject_nodes: [],
    version: 1,
    state: 'submitted',
    open_comment_count: 0,
    prompt: 'Draft a test artifact',
    content: 'Selected artifact text is here.',
    comments: [],
    ...overrides,
  };
}

function mockRangeRect() {
  Object.defineProperty(window.Range.prototype, 'getBoundingClientRect', {
    configurable: true,
    value: () =>
      ({
        x: 20,
        y: 30,
        top: 30,
        right: 120,
        bottom: 48,
        left: 20,
        width: 100,
        height: 18,
        toJSON: () => ({}),
      }) as DOMRect,
  });
}

function selectTextIn(element: HTMLElement, selectedText: string) {
  const walker = document.createTreeWalker(element, NodeFilter.SHOW_TEXT);
  const node = walker.nextNode();
  if (!node?.textContent) throw new Error('No selectable text node found');
  const start = node.textContent.indexOf(selectedText);
  if (start < 0) throw new Error(`Missing text "${selectedText}"`);
  const range = document.createRange();
  range.setStart(node, start);
  range.setEnd(node, start + selectedText.length);
  const selection = window.getSelection();
  selection?.removeAllRanges();
  selection?.addRange(range);
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

describe('ArtifactComments inline selection composer', () => {
  it('opens automatically for an in-artifact selection and posts the selected anchor', async () => {
    mockRangeRect();
    apiMocks.postArtifactComment.mockResolvedValue({});
    const onChanged = vi.fn();

    render(
      <ArtifactComments
        data={detail()}
        projectId="proj1"
        artifactId="ART-1"
        canComment
        onChanged={onChanged}
      />,
    );

    const artifactText = screen.getByText(/Selected artifact text is here/);
    selectTextIn(artifactText, 'Selected artifact text');
    fireEvent.pointerUp(artifactText, { pointerType: 'mouse' });

    const inlineComposer = await screen.findByRole('form', { name: 'Comment on selected text' });
    expect(within(inlineComposer).getByText('Selected artifact text')).toBeInTheDocument();

    fireEvent.change(within(inlineComposer).getByLabelText('Selection comment'), {
      target: { value: 'Please tighten this sentence.' },
    });
    fireEvent.click(within(inlineComposer).getByRole('button', { name: 'Comment' }));

    await waitFor(() => expect(apiMocks.postArtifactComment).toHaveBeenCalled());
    expect(apiMocks.postArtifactComment).toHaveBeenCalledWith(
      'ART-1',
      { message: 'Please tighten this sentence.', anchor: 'Selected artifact text' },
      'proj1',
    );
    expect(onChanged).toHaveBeenCalled();
    await waitFor(() =>
      expect(screen.queryByRole('form', { name: 'Comment on selected text' })).toBeNull(),
    );
    expect(screen.getByPlaceholderText('Leave a comment…')).toBeEnabled();
  });

  it('does not open for collapsed selections or while regeneration pauses comments', async () => {
    mockRangeRect();
    const { rerender } = render(
      <ArtifactComments
        data={detail()}
        projectId="proj1"
        artifactId="ART-1"
        canComment
        onChanged={vi.fn()}
      />,
    );

    const artifactText = screen.getByText(/Selected artifact text is here/);
    const node = artifactText.firstChild;
    if (!node) throw new Error('No selectable text node found');
    const range = document.createRange();
    range.setStart(node, 0);
    range.collapse(true);
    window.getSelection()?.removeAllRanges();
    window.getSelection()?.addRange(range);
    fireEvent.pointerUp(artifactText, { pointerType: 'mouse' });

    await waitFor(() =>
      expect(screen.queryByRole('form', { name: 'Comment on selected text' })).toBeNull(),
    );

    rerender(
      <ArtifactComments
        data={detail({ state: 'regenerating' })}
        projectId="proj1"
        artifactId="ART-1"
        canComment
        onChanged={vi.fn()}
      />,
    );
    selectTextIn(artifactText, 'Selected artifact text');
    fireEvent.pointerUp(artifactText, { pointerType: 'mouse' });

    await waitFor(() =>
      expect(screen.queryByRole('form', { name: 'Comment on selected text' })).toBeNull(),
    );
    expect(apiMocks.postArtifactComment).not.toHaveBeenCalled();
  });
});
