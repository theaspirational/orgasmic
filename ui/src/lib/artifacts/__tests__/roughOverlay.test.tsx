// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, render, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { ArtifactRenderer } from '../ArtifactRenderer';

// useRoughOverlay (blocks/shared.tsx) skips any [data-rough] element whose
// getBoundingClientRect() is smaller than 4x4 — real layout in a browser,
// but jsdom has no layout engine and always reports a zero-size rect. Stub a
// realistic box so the overlay's real "measure, then draw" path actually
// runs instead of silently no-op'ing on every element in this file.
const originalRect = HTMLElement.prototype.getBoundingClientRect;
beforeEach(() => {
  HTMLElement.prototype.getBoundingClientRect = function stubbedRect() {
    return { x: 0, y: 0, top: 0, left: 0, right: 120, bottom: 48, width: 120, height: 48, toJSON: () => '' } as DOMRect;
  };
});
afterEach(() => {
  HTMLElement.prototype.getBoundingClientRect = originalRect;
  cleanup();
});

const MDX = `
<Wireframe surface="panel">
<div class="wf-card" data-rough style="display:flex;align-items:center;gap:8px">
  <span data-icon="check"></span>
  <small class="wf-muted">Saved a moment ago</small>
</div>
</Wireframe>
`;

async function expectRoughOverlay(container: HTMLElement) {
  await waitFor(
    () => {
      const target = container.querySelector('[data-rough]');
      expect(target).toBeTruthy();
      expect(target?.querySelector('svg[data-rough-overlay]')).toBeTruthy();
    },
    { timeout: 2000 },
  );
}

describe('rough.js sketch overlay — data-rough fixture coverage', () => {
  it('draws the hand-drawn overlay svg over a [data-rough] element in the light theme', async () => {
    document.documentElement.dataset.theme = 'paper';
    const { container } = render(<ArtifactRenderer content={MDX} />);
    await expectRoughOverlay(container);
  });

  it('draws the hand-drawn overlay svg over a [data-rough] element in the dark theme', async () => {
    document.documentElement.dataset.theme = 'black-paper';
    const { container } = render(<ArtifactRenderer content={MDX} />);
    await expectRoughOverlay(container);
  });

  it('skips elements below the 4x4 minimum size instead of drawing a degenerate overlay', async () => {
    HTMLElement.prototype.getBoundingClientRect = function tinyRect() {
      return { x: 0, y: 0, top: 0, left: 0, right: 2, bottom: 2, width: 2, height: 2, toJSON: () => '' } as DOMRect;
    };
    const { container } = render(<ArtifactRenderer content={MDX} />);
    // No waitFor target to succeed on — give the same 30ms-timer + dynamic
    // import path a chance to run, then assert it stayed a no-op.
    await new Promise((resolve) => setTimeout(resolve, 200));
    expect(container.querySelector('svg[data-rough-overlay]')).toBeNull();
  });
});
