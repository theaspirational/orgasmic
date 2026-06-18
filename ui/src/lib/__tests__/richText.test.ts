import { describe, expect, it, vi } from 'vitest';
import { isValidElement } from 'react';

vi.mock('@/lib/api', () => ({ fetchGlossary: vi.fn() }));

import { decorateText } from '../richText';

function linkedTokens(text: string): string[] {
  const nodes = decorateText(text, {
    openEntity: () => {},
    openGlossary: () => {},
    glossaryPattern: null,
    glossaryLookup: new Map(),
  });
  return nodes
    .filter(isValidElement<{ children?: unknown }>)
    .map((node) => String(node.props.children));
}

describe('decorateText entity links', () => {
  it('linkifies minted entity IDs and task/architecture suffixes', () => {
    expect(linkedTokens('See TASK-ZD72S, TASK-YRK1V.1, dec_8KX2M, and arch_8KX2M.')).toEqual([
      'TASK-ZD72S',
      'TASK-YRK1V.1',
      'dec_8KX2M',
      'arch_8KX2M',
    ]);
  });

  it('keeps linkifying legacy and existing minted-looking entity IDs', () => {
    expect(linkedTokens('Regression: TASK-CJWT3, dec_X72P5, arch_C87Z9.3.')).toEqual([
      'TASK-CJWT3',
      'dec_X72P5',
      'arch_C87Z9.3',
    ]);
  });

  it('does not linkify bare uppercase five-letter words without an entity prefix', () => {
    expect(linkedTokens('HELLO should remain prose, but TASK-ZD72S should link.')).toEqual([
      'TASK-ZD72S',
    ]);
  });
});
