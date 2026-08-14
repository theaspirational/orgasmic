// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';

import { OrgBody, parseBlocks, parseInlines } from '../orgBody';

afterEach(cleanup);

describe('OrgBody', () => {
  it('keeps separator and path slashes out of emphasis while preserving code spans', () => {
    const source =
      'Build: =cargo build=. Run (=npm run typecheck= / tests under =ui/=) when the change touches =ui/=';
    const inlines = parseInlines(source);

    expect(inlines.filter((inline) => inline.kind === 'italic')).toEqual([]);
    expect(inlines.filter((inline) => inline.kind === 'code').map((inline) => inline.value)).toEqual([
      'cargo build',
      'npm run typecheck',
      'ui/',
      'ui/',
    ]);
  });

  it('recognizes deliberate emphasis without treating a spaced slash as markup', () => {
    const inlines = parseInlines('Use /deliberate emphasis/, but keep product / tool as prose.');

    expect(inlines.filter((inline) => inline.kind === 'italic')).toEqual([
      { kind: 'italic', value: 'deliberate emphasis' },
    ]);
  });

  it('separates a paragraph from a following list without requiring a blank line', () => {
    const source = [
      'The visual system uses =ui/src/styles.css= (light =:root= / dark =.dark=).',
      '- Color: token driven.',
      '- Typography: =Geist Variable= and system mono.',
    ].join('\n');

    const blocks = parseBlocks(source);
    expect(blocks.map((block) => block.kind)).toEqual(['paragraph', 'list']);

    render(<OrgBody source={source} />);
    const paragraph = screen.getByText(/The visual system uses/);
    expect(paragraph.tagName).toBe('P');
    expect(paragraph).not.toHaveTextContent('Color: token driven.');
    expect(screen.getAllByRole('listitem')).toHaveLength(2);
    expect(document.querySelectorAll('em')).toHaveLength(0);
    expect(Array.from(document.querySelectorAll('code')).map((node) => node.textContent)).toEqual([
      'ui/src/styles.css',
      ':root',
      '.dark',
      'Geist Variable',
    ]);
  });
});
