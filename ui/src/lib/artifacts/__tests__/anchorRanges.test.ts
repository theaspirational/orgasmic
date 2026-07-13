// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';

import { findQuoteRange } from '../anchorRanges';

function mount(html: string): HTMLElement {
  const root = document.createElement('div');
  root.innerHTML = html;
  document.body.appendChild(root);
  return root;
}

afterEach(() => {
  document.body.innerHTML = '';
});

describe('findQuoteRange', () => {
  it('matches within a single text node', () => {
    const root = mount('<p>The quick brown fox jumps.</p>');
    const range = findQuoteRange(root, 'quick brown fox');
    expect(range).not.toBeNull();
    expect(range?.toString()).toBe('quick brown fox');
  });

  it('matches across multiple text nodes and inline elements', () => {
    const root = mount('<p>The <strong>quick brown</strong> fox jumps.</p>');
    const range = findQuoteRange(root, 'quick brown fox');
    expect(range).not.toBeNull();
    // The range spans the <strong> boundary; its text content is the match.
    expect(range?.toString().replace(/\s+/g, ' ')).toBe('quick brown fox');
  });

  it('matches with collapsed/newline whitespace in the DOM', () => {
    const root = mount('<p>The   quick\n   brown\tfox jumps.</p>');
    const range = findQuoteRange(root, 'quick brown fox');
    expect(range).not.toBeNull();
  });

  it('returns null when the quote is absent', () => {
    const root = mount('<p>Nothing to see here.</p>');
    expect(findQuoteRange(root, 'quick brown fox')).toBeNull();
  });

  it('returns null for an empty quote', () => {
    const root = mount('<p>Some text.</p>');
    expect(findQuoteRange(root, '   ')).toBeNull();
  });
});
