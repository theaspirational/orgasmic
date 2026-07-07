// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, render, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

// See render.test.tsx for why this mocks the real mermaid/shiki libraries
// rather than the module: useTheme just needs a mounted-provider stand-in,
// and `themeState` lets each test flip resolved theme without re-mocking.
const themeState = vi.hoisted(() => ({ resolved: 'paper' as 'paper' | 'black-paper' }));
vi.mock('@/lib/theme', () => ({
  useTheme: () => ({ preference: 'system', resolved: themeState.resolved, setPreference: vi.fn() }),
}));

if (typeof SVGElement !== 'undefined') {
  const proto = SVGElement.prototype as unknown as {
    getBBox?: () => DOMRect;
    getComputedTextLength?: () => number;
  };
  if (!proto.getBBox) {
    proto.getBBox = () => ({ x: 0, y: 0, width: 100, height: 20, top: 0, left: 0, right: 0, bottom: 0, toJSON: () => '' }) as DOMRect;
  }
  if (!proto.getComputedTextLength) {
    proto.getComputedTextLength = () => 60;
  }
}

afterEach(() => cleanup());

import { ArtifactRenderer } from '../ArtifactRenderer';

const MDX = `
<Mermaid>
flowchart LR
  A[Boot] --> B{Override stored?}
  B -- yes --> C[Apply stored theme]
</Mermaid>
`;

describe('Mermaid — token-driven theme: base (both app themes)', () => {
  it('renders a real SVG in the light (paper) theme without throwing', async () => {
    themeState.resolved = 'paper';
    const { container } = render(<ArtifactRenderer content={MDX} />);
    await waitFor(() => expect(container.querySelector('.mermaid-diagram svg')).toBeTruthy(), { timeout: 5000 });
    expect(container.textContent).not.toContain('Unrenderable block');
  });

  it('renders a real SVG in the dark (black-paper) theme without throwing', async () => {
    themeState.resolved = 'black-paper';
    const { container } = render(<ArtifactRenderer content={MDX} />);
    await waitFor(() => expect(container.querySelector('.mermaid-diagram svg')).toBeTruthy(), { timeout: 5000 });
    expect(container.textContent).not.toContain('Unrenderable block');
  });
});
