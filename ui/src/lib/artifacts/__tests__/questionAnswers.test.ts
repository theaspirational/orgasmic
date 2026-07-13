import { describe, expect, it } from 'vitest';

import type { CommentRecord } from '@/lib/types';
import {
  agreeAuthors,
  buildAnswerMessage,
  isAnswerComplete,
  latestAnswersPerAuthor,
} from '../questionAnswers';

function comment(overrides: Partial<CommentRecord> = {}): CommentRecord {
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

function questionAnchor(key: string, prompt = 'Q'): string {
  return JSON.stringify({ kind: 'question', key, prompt });
}

describe('buildAnswerMessage', () => {
  it('formats a single choice', () => {
    expect(buildAnswerMessage({ type: 'single', label: 'Postgres', other: null })).toBe('Postgres');
  });

  it('formats a single Other write-in', () => {
    expect(buildAnswerMessage({ type: 'single', label: null, other: 'DuckDB' })).toBe('Other: DuckDB');
  });

  it('joins multi choices with semicolons and appends Other', () => {
    expect(buildAnswerMessage({ type: 'multi', labels: ['A', 'B'], other: null })).toBe('A; B');
    expect(buildAnswerMessage({ type: 'multi', labels: ['A'], other: 'C' })).toBe('A; Other: C');
  });

  it('returns the freeform text', () => {
    expect(buildAnswerMessage({ type: 'freeform', text: '  hello  ' })).toBe('hello');
  });
});

describe('isAnswerComplete', () => {
  it('single requires a choice or non-empty Other', () => {
    expect(isAnswerComplete({ type: 'single', label: null, other: null })).toBe(false);
    expect(isAnswerComplete({ type: 'single', label: 'A', other: null })).toBe(true);
    expect(isAnswerComplete({ type: 'single', label: null, other: '' })).toBe(false);
    expect(isAnswerComplete({ type: 'single', label: null, other: 'x' })).toBe(true);
  });

  it('multi requires a selection; an empty active Other blocks', () => {
    expect(isAnswerComplete({ type: 'multi', labels: [], other: null })).toBe(false);
    expect(isAnswerComplete({ type: 'multi', labels: ['A'], other: null })).toBe(true);
    expect(isAnswerComplete({ type: 'multi', labels: ['A'], other: '' })).toBe(false);
    expect(isAnswerComplete({ type: 'multi', labels: [], other: 'x' })).toBe(true);
  });

  it('freeform requires text', () => {
    expect(isAnswerComplete({ type: 'freeform', text: '   ' })).toBe(false);
    expect(isAnswerComplete({ type: 'freeform', text: 'hi' })).toBe(true);
  });
});

describe('latestAnswersPerAuthor', () => {
  it('keeps the latest answer per author for the matching key', () => {
    const key = 'aaaa1111';
    const comments = [
      comment({ cid: 'c1', author: 'ann', anchor: questionAnchor(key), message: 'first' }),
      comment({ cid: 'c2', author: 'bob', anchor: questionAnchor(key), message: 'bob-ans' }),
      comment({ cid: 'c3', author: 'ann', anchor: questionAnchor(key), message: 'ann-newer' }),
      // Different question key — excluded.
      comment({ cid: 'c4', author: 'ann', anchor: questionAnchor('other999'), message: 'nope' }),
      // Plain comment — excluded.
      comment({ cid: 'c5', author: 'cid', anchor: 'plain text', message: 'nope' }),
    ];
    const answers = latestAnswersPerAuthor(comments, key);
    expect(answers.map((a) => a.cid)).toEqual(['c3', 'c2']);
    expect(answers.find((a) => a.author === 'ann')?.message).toBe('ann-newer');
  });
});

describe('agreeAuthors', () => {
  it('lists distinct authors who replied Agree to an answer', () => {
    const comments = [
      comment({ cid: 'ans1', author: 'ann' }),
      comment({ cid: 'r1', author: 'bob', reply_to: 'ans1', message: 'Agree' }),
      comment({ cid: 'r2', author: 'cid', reply_to: 'ans1', message: 'Agree' }),
      comment({ cid: 'r3', author: 'bob', reply_to: 'ans1', message: 'Agree' }), // dupe author
      comment({ cid: 'r4', author: 'dan', reply_to: 'ans1', message: 'Disagree' }), // not Agree
      comment({ cid: 'r5', author: 'eve', reply_to: 'other', message: 'Agree' }), // other parent
    ];
    expect(agreeAuthors(comments, 'ans1')).toEqual(['bob', 'cid']);
  });
});
