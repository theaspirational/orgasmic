import { describe, expect, it } from 'vitest';

import type { DecisionSummary } from '@/lib/types';

import { decisionRowPresentation } from '../decisionPresentation';

function decision(overrides: Partial<DecisionSummary> = {}): DecisionSummary {
  return {
    id: 'dec_PC6T2',
    title: 'Native run evidence and retrospective execution are explicit opt-in operations',
    tags: [],
    glossary_refs: [],
    source_file: '.orgasmic/decisions/dec_001/node.org',
    ...overrides,
  };
}

describe('decisionRowPresentation', () => {
  it('keeps the authored heading primary and strips a numbered-list marker from the preview', () => {
    expect(
      decisionRowPresentation(
        decision({
          preview: '1. Orgasmic session JSONL stores only lifecycle authority and bounded normalized semantic events.',
        }),
      ),
    ).toEqual({
      title: 'Native run evidence and retrospective execution are explicit opt-in operations',
      preview: 'Orgasmic session JSONL stores only lifecycle authority and bounded normalized semantic events.',
    });
  });

  it('shows the authored heading without a placeholder when no preview is available', () => {
    expect(decisionRowPresentation(decision({ preview: null }))).toEqual({
      title: 'Native run evidence and retrospective execution are explicit opt-in operations',
      preview: null,
    });
  });

  it('falls back to the id only when the source heading has no title', () => {
    expect(decisionRowPresentation(decision({ title: '  ', preview: 'A useful explanation.' }))).toEqual({
      title: 'dec_PC6T2',
      preview: 'A useful explanation.',
    });
  });
});
