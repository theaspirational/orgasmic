// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, render, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';

import { ArtifactRenderer } from '../ArtifactRenderer';

afterEach(() => cleanup());

const MDX = `
<AnnotatedCode language="ts" filename="example.ts" annotations={[
  { lines: "1-2", label: "Setup", note: "Reads config once at module load." },
  { lines: "4", note: "Falls back to a default when unset." }
]} code={\`const a = 1;
const b = 2;

const c = a + b;\`} />
`;

// Shiki highlighting (useShikiHtml) is async, and the margin bubbles only
// exist once the highlighted `.line` elements are in the DOM for the
// anchoring effect to measure against — so every assertion here waits for
// that first paint instead of reading the pre-highlight synchronous render.
async function renderAnnotated() {
  const { container } = render(<ArtifactRenderer content={MDX} />);
  await waitFor(() => expect(container.querySelectorAll('[data-annotation-line]').length).toBeGreaterThan(0));
  return container;
}

describe('AnnotatedCode (inline margin bubbles)', () => {
  it('anchors one margin bubble per annotation, each tagged with the target line range', async () => {
    const container = await renderAnnotated();
    const bubbles = container.querySelectorAll('[data-annotation-line]');
    expect(bubbles.length).toBeGreaterThanOrEqual(2);
    const lineValues = Array.from(bubbles).map((el) => el.getAttribute('data-annotation-line'));
    expect(lineValues).toContain('1-2');
    expect(lineValues).toContain('4');
  });

  it('carries the label and note text into the bubble content', async () => {
    const container = await renderAnnotated();
    expect(container.textContent).toContain('Setup');
    expect(container.textContent).toContain('Reads config once at module load.');
    expect(container.textContent).toContain('Falls back to a default when unset.');
  });

  it('marks the targeted code lines (not just line 1) with the anchor class', async () => {
    const container = await renderAnnotated();
    const marked = container.querySelectorAll('.annotated-code-line');
    // lines 1, 2 (from "1-2") and 4 (from "4") => 3 marked lines.
    expect(marked.length).toBe(3);
  });

  it('every rendered bubble/legend anchor carries a well-formed line-range value', async () => {
    const container = await renderAnnotated();
    const allTagged = container.querySelectorAll('[data-annotation-line]');
    expect(allTagged.length).toBeGreaterThan(0);
    allTagged.forEach((el) => {
      expect(el.getAttribute('data-annotation-line')).toMatch(/^\d+(-\d+)?$/);
    });
  });
});
