// @vitest-environment jsdom

import '@testing-library/jest-dom/vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';

import { OrgInlineText } from '../orgBody';

afterEach(cleanup);

describe('OrgInlineText', () => {
  it('renders compact Org emphasis without exposing source markers', () => {
    const { container } = render(
      <div>
        <OrgInlineText
          source="Use =board.org= with *explicit* registration."
          interactive={false}
          compact
        />
      </div>,
    );

    expect(screen.getByText('board.org').tagName).toBe('CODE');
    expect(screen.getByText('board.org')).toHaveAttribute(
      'style',
      'font-size: inherit; line-height: inherit;',
    );
    expect(screen.getByText('explicit').tagName).toBe('STRONG');
    expect(container).toHaveTextContent('Use board.org with explicit registration.');
    expect(container).not.toHaveTextContent('=board.org=');
  });

  it('keeps compact Org links presentational inside a clickable parent row', () => {
    const { container } = render(
      <OrgInlineText source="Read [[file:board.org][the board file]]." interactive={false} />,
    );

    expect(screen.getByText('the board file').tagName).toBe('SPAN');
    expect(container.querySelector('a, button')).not.toBeInTheDocument();
  });
});
