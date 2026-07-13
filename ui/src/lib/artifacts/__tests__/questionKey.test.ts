import { describe, expect, it } from 'vitest';

import { normalizeQuestionPrompt, parseQuestionAnchor, questionKey } from '../questionKey';

describe('questionKey', () => {
  it('is a stable 8-char hex hash', () => {
    const key = questionKey('Which database should we use?');
    expect(key).toMatch(/^[0-9a-f]{8}$/);
    expect(questionKey('Which database should we use?')).toBe(key);
  });

  it('ignores incidental whitespace differences', () => {
    expect(questionKey('Which  database\nshould we use?')).toBe(
      questionKey('Which database should we use?'),
    );
    expect(questionKey('  trimmed  ')).toBe(questionKey('trimmed'));
  });

  it('differs for different prompts', () => {
    expect(questionKey('Question A')).not.toBe(questionKey('Question B'));
  });

  it('normalizes prompts by trimming and collapsing whitespace', () => {
    expect(normalizeQuestionPrompt('  a\t b\n c  ')).toBe('a b c');
  });
});

describe('parseQuestionAnchor', () => {
  it('parses a question anchor', () => {
    const anchor = JSON.stringify({ kind: 'question', key: 'abc12345', prompt: 'Pick one' });
    expect(parseQuestionAnchor(anchor)).toEqual({ kind: 'question', key: 'abc12345', prompt: 'Pick one' });
  });

  it('returns null for plain text, empty, {}, and malformed JSON', () => {
    expect(parseQuestionAnchor('the second paragraph')).toBeNull();
    expect(parseQuestionAnchor('')).toBeNull();
    expect(parseQuestionAnchor('{}')).toBeNull();
    expect(parseQuestionAnchor('{not json')).toBeNull();
    expect(parseQuestionAnchor(JSON.stringify({ kind: 'selection', text: 'x' }))).toBeNull();
  });
});
