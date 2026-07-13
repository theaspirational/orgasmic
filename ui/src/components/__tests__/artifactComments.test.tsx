// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
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
    reply_to: '',
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

  it('renders the quote as a navigation button and invokes the handler', () => {
    const onQuoteNavigate = vi.fn();
    render(
      <CommentCard
        comment={comment()}
        canComment={false}
        resolving={false}
        onQuoteNavigate={onQuoteNavigate}
      />,
    );
    const quote = screen.getByRole('button', { name: 'the second paragraph' });
    fireEvent.click(quote);
    expect(onQuoteNavigate).toHaveBeenCalledWith(expect.objectContaining({ cid: 'cmt_1' }));
  });

  it('shows an answer badge and the question prompt for a question-answer comment', () => {
    const questionComment = comment({
      anchor: JSON.stringify({ kind: 'question', key: 'abcd1234', prompt: 'Which stack?' }),
      message: 'Postgres',
    });
    render(<CommentCard comment={questionComment} canComment={false} resolving={false} />);
    expect(screen.getByText('answer')).toBeInTheDocument();
    expect(screen.getByText('Which stack?')).toBeInTheDocument();
  });

  it('posts a reply with reply_to when the reply composer is used', async () => {
    apiMocks.postArtifactComment.mockResolvedValue(true);
    const onReply = vi.fn(async (parentCid: string, message: string) => {
      await apiMocks.postArtifactComment('ART-1', { message, reply_to: parentCid }, 'proj1');
      return true;
    });
    render(
      <CommentCard comment={comment()} canComment resolving={false} onReply={onReply} />,
    );
    fireEvent.click(screen.getByRole('button', { name: 'Reply' }));
    const form = screen.getByRole('form', { name: 'Reply to Dana Reviewer' });
    fireEvent.change(within(form).getByLabelText('Reply'), { target: { value: 'Good point.' } });
    fireEvent.click(within(form).getByRole('button', { name: 'Reply' }));

    await waitFor(() => expect(onReply).toHaveBeenCalledWith('cmt_1', 'Good point.'));
    expect(apiMocks.postArtifactComment).toHaveBeenCalledWith(
      'ART-1',
      { message: 'Good point.', reply_to: 'cmt_1' },
      'proj1',
    );
  });

  it('nests replies under their root comment', () => {
    render(
      <CommentCard
        comment={comment({ cid: 'root', message: 'Root comment' })}
        replies={[comment({ cid: 'reply', author: 'Ravi', message: 'A reply', reply_to: 'root' })]}
        canComment={false}
        resolving={false}
      />,
    );
    expect(screen.getByText('Root comment')).toBeInTheDocument();
    expect(screen.getByText('A reply')).toBeInTheDocument();
    expect(screen.getByText('Ravi')).toBeInTheDocument();
  });
});

describe('ArtifactComments threaded replies', () => {
  it('renders a reply nested under its root and posts reply_to', async () => {
    apiMocks.postArtifactComment.mockResolvedValue({ cid: 'CID-new' });
    const onChanged = vi.fn();
    render(
      <ArtifactComments
        data={detail({
          comments: [
            comment({ cid: 'root1', author: 'Ana', anchor: '{}', message: 'Root here' }),
            comment({ cid: 'rep1', author: 'Bo', anchor: '{}', message: 'Nested reply', reply_to: 'root1' }),
          ],
        })}
        projectId="proj1"
        artifactId="ART-1"
        canComment
        onChanged={onChanged}
      />,
    );

    expect(screen.getByText('Root here')).toBeInTheDocument();
    expect(screen.getByText('Nested reply')).toBeInTheDocument();

    // Reply to the root: open its Reply composer and submit.
    const replyButtons = screen.getAllByRole('button', { name: 'Reply' });
    fireEvent.click(replyButtons[0]);
    const form = screen.getByRole('form', { name: 'Reply to Ana' });
    fireEvent.change(within(form).getByLabelText('Reply'), { target: { value: 'Me too' } });
    fireEvent.click(within(form).getByRole('button', { name: 'Reply' }));

    await waitFor(() => expect(apiMocks.postArtifactComment).toHaveBeenCalled());
    expect(apiMocks.postArtifactComment).toHaveBeenCalledWith(
      'ART-1',
      { message: 'Me too', reply_to: 'root1' },
      'proj1',
    );
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

describe('ArtifactComments selectionchange sync (native handle drag)', () => {
  // jsdom never auto-fires selectionchange on selection mutation, so every
  // capture in these tests is driven purely by an explicit selectionchange —
  // i.e. no pointerup/touchend reaches the page, exactly like a mobile native
  // handle drag.
  function collapseSelectionIn(element: HTMLElement) {
    const walker = document.createTreeWalker(element, NodeFilter.SHOW_TEXT);
    const node = walker.nextNode();
    if (!node) throw new Error('No selectable text node found');
    const range = document.createRange();
    range.setStart(node, 0);
    range.collapse(true);
    const selection = window.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(range);
  }

  function fireSelectionChange() {
    act(() => {
      document.dispatchEvent(new Event('selectionchange'));
      vi.advanceTimersByTime(150);
    });
  }

  function inlineComposer() {
    return screen.getByRole('form', { name: 'Comment on selected text' });
  }

  it('updates the quote when the selection is extended, with no pointerup', () => {
    vi.useFakeTimers();
    try {
      mockRangeRect();
      render(
        <ArtifactComments
          data={detail()}
          projectId="proj1"
          artifactId="ART-1"
          canComment
          onChanged={vi.fn()}
        />,
      );

      const artifactText = screen.getByText(/Selected artifact text is here/);
      // Mobile long-press selects one word; the composer opens quoting it.
      selectTextIn(artifactText, 'Selected');
      fireSelectionChange();
      expect(within(inlineComposer()).getByText('Selected')).toBeInTheDocument();

      // Drag the native handle to extend the selection: only selectionchange
      // fires — no pointerup/touchend reaches the page.
      selectTextIn(artifactText, 'Selected artifact text is here');
      fireSelectionChange();
      expect(within(inlineComposer()).getByText('Selected artifact text is here')).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it('preserves the captured quote when the selection collapses (focusing the textarea)', () => {
    vi.useFakeTimers();
    try {
      mockRangeRect();
      render(
        <ArtifactComments
          data={detail()}
          projectId="proj1"
          artifactId="ART-1"
          canComment
          onChanged={vi.fn()}
        />,
      );

      const artifactText = screen.getByText(/Selected artifact text is here/);
      selectTextIn(artifactText, 'Selected artifact text');
      fireSelectionChange();
      expect(within(inlineComposer()).getByText('Selected artifact text')).toBeInTheDocument();

      // Tapping into the composer collapses the artifact selection; the quote
      // must survive.
      collapseSelectionIn(artifactText);
      fireSelectionChange();
      expect(within(inlineComposer()).getByText('Selected artifact text')).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it('keeps a typed draft when the quote updates on re-capture', () => {
    vi.useFakeTimers();
    try {
      mockRangeRect();
      render(
        <ArtifactComments
          data={detail()}
          projectId="proj1"
          artifactId="ART-1"
          canComment
          onChanged={vi.fn()}
        />,
      );

      const artifactText = screen.getByText(/Selected artifact text is here/);
      selectTextIn(artifactText, 'Selected');
      fireSelectionChange();

      fireEvent.change(within(inlineComposer()).getByLabelText('Selection comment'), {
        target: { value: 'Half a thought' },
      });

      selectTextIn(artifactText, 'Selected artifact text is here');
      fireSelectionChange();

      expect(within(inlineComposer()).getByText('Selected artifact text is here')).toBeInTheDocument();
      expect(within(inlineComposer()).getByLabelText('Selection comment')).toHaveValue('Half a thought');
    } finally {
      vi.useRealTimers();
    }
  });

  it('starts a fresh capture with an empty draft', () => {
    vi.useFakeTimers();
    try {
      mockRangeRect();
      render(
        <ArtifactComments
          data={detail()}
          projectId="proj1"
          artifactId="ART-1"
          canComment
          onChanged={vi.fn()}
        />,
      );

      const artifactText = screen.getByText(/Selected artifact text is here/);
      selectTextIn(artifactText, 'Selected artifact text');
      fireSelectionChange();

      expect(within(inlineComposer()).getByLabelText('Selection comment')).toHaveValue('');
    } finally {
      vi.useRealTimers();
    }
  });

  it('does not disturb an open composer or draft on an identical re-selection (no-op guard)', () => {
    vi.useFakeTimers();
    try {
      mockRangeRect();
      render(
        <ArtifactComments
          data={detail()}
          projectId="proj1"
          artifactId="ART-1"
          canComment
          onChanged={vi.fn()}
        />,
      );

      const artifactText = screen.getByText(/Selected artifact text is here/);
      selectTextIn(artifactText, 'Selected artifact text');
      fireSelectionChange();

      fireEvent.change(within(inlineComposer()).getByLabelText('Selection comment'), {
        target: { value: 'Steady draft' },
      });

      // Re-selecting identical text (selectionchange churn) must be a no-op.
      selectTextIn(artifactText, 'Selected artifact text');
      fireSelectionChange();

      expect(within(inlineComposer()).getByText('Selected artifact text')).toBeInTheDocument();
      expect(within(inlineComposer()).getByLabelText('Selection comment')).toHaveValue('Steady draft');
    } finally {
      vi.useRealTimers();
    }
  });

  it('does not sync the selection once the viewer can no longer comment', () => {
    vi.useFakeTimers();
    try {
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

      rerender(
        <ArtifactComments
          data={detail()}
          projectId="proj1"
          artifactId="ART-1"
          canComment={false}
          onChanged={vi.fn()}
        />,
      );

      const artifactText = screen.getByText(/Selected artifact text is here/);
      selectTextIn(artifactText, 'Selected artifact text');
      fireSelectionChange();

      expect(screen.queryByRole('form', { name: 'Comment on selected text' })).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });
});
