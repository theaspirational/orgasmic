import { describe, expect, it, vi } from 'vitest';
import { isValidElement } from 'react';

vi.mock('@/lib/api', () => ({ fetchGlossary: vi.fn() }));

import { decorateText } from '../richText';

function decorate(text: string) {
  return decorateText(text, {
    openEntity: () => {},
    openGlossary: () => {},
    glossaryPattern: null,
    glossaryLookup: new Map(),
  });
}

function linkedTokens(text: string): string[] {
  return decorate(text)
    .filter(isValidElement<{ children?: unknown }>)
    .map((node) => String(node.props.children));
}

/// orgasmic:TASK-RQ270.3.1
/// Every node flattened back to text, links included. `linkedTokens` alone
/// discards string nodes, so a "not linkified" assertion built on it also
/// passes when the token is DROPPED — which is the opposite of the contract
/// for a retired id. Found by that task's review.
function renderedText(text: string): string {
  return decorate(text)
    .map((node) =>
      isValidElement<{ children?: unknown }>(node) ? String(node.props.children) : String(node),
    )
    .join('');
}

describe('decorateText entity links', () => {
  it('linkifies minted task and decision IDs', () => {
    expect(linkedTokens('See TASK-ZD72S, TASK-YRK1V.1, dec_8KX2M, and arch_8KX2M.')).toEqual([
      'TASK-ZD72S',
      'TASK-YRK1V.1',
      'dec_8KX2M',
    ]);
  });

  it('leaves legacy architecture IDs as plain text', () => {
    const source = 'Regression: TASK-CJWT3, dec_X72P5, arch_C87Z9.3.';
    // Both halves of the contract. The first alone would also pass if the
    // token were erased rather than left as prose (TASK-RQ270.3.1 review).
    expect(linkedTokens(source)).toEqual(['TASK-CJWT3', 'dec_X72P5']);
    expect(renderedText(source)).toBe(source);
    expect(renderedText(source)).toContain('arch_C87Z9.3');
  });

  it('does not linkify bare uppercase five-letter words without an entity prefix', () => {
    expect(linkedTokens('HELLO should remain prose, but TASK-ZD72S should link.')).toEqual([
      'TASK-ZD72S',
    ]);
  });
});
