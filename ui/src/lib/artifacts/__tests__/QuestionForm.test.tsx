// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import type { CommentRecord } from '@/lib/types';
import { QuestionForm } from '../blocks/QuestionForm';
import { ArtifactInteractionContext, type ArtifactInteraction } from '../interaction';
import { questionKey } from '../questionKey';
import type { MdxNode } from '../types';

afterEach(cleanup);

const PROMPT = 'Which database should we use?';

function node(): Extract<MdxNode, { kind: 'element' }> {
  return {
    kind: 'element',
    name: 'QuestionForm',
    props: {
      title: 'Open Questions',
      questions: [
        {
          type: 'single',
          prompt: PROMPT,
          options: [{ label: 'Postgres' }, { label: 'SQLite' }],
          allowOther: true,
        },
      ],
    },
    children: [],
  };
}

function questionAnchor(prompt: string): string {
  return JSON.stringify({ kind: 'question', key: questionKey(prompt), prompt });
}

function comment(overrides: Partial<CommentRecord>): CommentRecord {
  return {
    cid: 'CID-x',
    author: 'ann',
    version: 1,
    anchor: '{}',
    resolution_target: '',
    reply_to: '',
    resolved: false,
    consumed: false,
    message: '',
    ...overrides,
  };
}

function interaction(overrides: Partial<ArtifactInteraction> = {}): ArtifactInteraction {
  return {
    canAnswer: true,
    comments: [],
    submitAnswer: vi.fn().mockResolvedValue(undefined),
    agree: vi.fn().mockResolvedValue(undefined),
    ...overrides,
  };
}

describe('QuestionForm', () => {
  it('renders read-only (no submit) when no interaction context is present', () => {
    render(<QuestionForm node={node()} />);
    expect(screen.getByText(PROMPT)).toBeInTheDocument();
    expect(screen.getByText('Postgres')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Submit' })).toBeNull();
  });

  it('submits a single-choice answer through the context', async () => {
    const ctx = interaction();
    render(
      <ArtifactInteractionContext.Provider value={ctx}>
        <QuestionForm node={node()} />
      </ArtifactInteractionContext.Provider>,
    );

    const submit = screen.getByRole('button', { name: 'Submit' });
    expect(submit).toBeDisabled();

    fireEvent.click(screen.getByLabelText('Postgres', { selector: 'input' }));
    expect(submit).toBeEnabled();
    fireEvent.click(submit);

    await waitFor(() => expect(ctx.submitAnswer).toHaveBeenCalled());
    expect(ctx.submitAnswer).toHaveBeenCalledWith({
      questionKey: questionKey(PROMPT),
      prompt: PROMPT,
      message: 'Postgres',
    });
  });

  it('shows the latest answer per author with Agree names', () => {
    const ctx = interaction({
      comments: [
        comment({ cid: 'a1', author: 'ann', anchor: questionAnchor(PROMPT), message: 'Postgres' }),
        comment({ cid: 'a2', author: 'bob', anchor: questionAnchor(PROMPT), message: 'SQLite' }),
        comment({ cid: 'r1', author: 'cid', reply_to: 'a1', message: 'Agree' }),
      ],
    });
    render(
      <ArtifactInteractionContext.Provider value={ctx}>
        <QuestionForm node={node()} />
      </ArtifactInteractionContext.Provider>,
    );

    expect(screen.getByText('ann')).toBeInTheDocument();
    expect(screen.getByText('bob')).toBeInTheDocument();
    expect(screen.getByText('Agreed: cid')).toBeInTheDocument();
  });

  it('agrees with an existing answer through the context', async () => {
    const ctx = interaction({
      comments: [comment({ cid: 'a1', author: 'ann', anchor: questionAnchor(PROMPT), message: 'Postgres' })],
    });
    render(
      <ArtifactInteractionContext.Provider value={ctx}>
        <QuestionForm node={node()} />
      </ArtifactInteractionContext.Provider>,
    );

    fireEvent.click(screen.getByRole('button', { name: 'Agree' }));
    await waitFor(() => expect(ctx.agree).toHaveBeenCalledWith({ cid: 'a1' }));
  });

  it('exposes the question key for navigation', () => {
    const { container } = render(<QuestionForm node={node()} />);
    expect(container.querySelector(`[data-question-key="${questionKey(PROMPT)}"]`)).not.toBeNull();
  });
});
