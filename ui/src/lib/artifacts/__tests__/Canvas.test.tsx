// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, render } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';

import { ArtifactRenderer } from '../ArtifactRenderer';

afterEach(() => cleanup());

const GALLERY_MDX = `
<Canvas>
<Screen surface="mobile" label="Before"><div>Before</div></Screen>
<Screen surface="mobile" label="After"><div>After</div></Screen>
</Canvas>
`;

const BOARD_MDX = `
<Canvas>
<Screen id="list" surface="browser" label="List" x={0} y={0}><div>List screen</div></Screen>
<Screen id="detail" surface="browser" label="Detail" x={1300} y={0}><div>Detail screen</div></Screen>
<Screen id="empty" surface="mobile" label="Empty state" x={0} y={1500}><div>Empty state</div></Screen>
<Connector from="list" to="detail" label="Open item" />
<Connector from="ghost" to="detail" />
<Annotation targetId="detail" placement="top" label="Heads up">Loads the full record on open.</Annotation>
</Canvas>
`;

describe('Canvas — gallery mode (no coordinates)', () => {
  it('falls back to the flex-wrap artboard gallery, unchanged from before this task', () => {
    const { container } = render(<ArtifactRenderer content={GALLERY_MDX} />);
    const wireframes = container.querySelectorAll('.orgasmic-wireframe');
    expect(wireframes.length).toBe(2);
    expect(container.querySelector('svg[aria-hidden="true"] marker')).toBeNull();
  });
});

describe('Canvas — board mode (x/y coordinates present)', () => {
  it('renders every screen as a positioned artboard', () => {
    const { container } = render(<ArtifactRenderer content={BOARD_MDX} />);
    const wireframes = container.querySelectorAll('.orgasmic-wireframe');
    expect(wireframes.length).toBe(3);
  });

  it('draws a connector line between named screens, tagged with from/to', () => {
    const { container } = render(<ArtifactRenderer content={BOARD_MDX} />);
    const connector = container.querySelector('[data-connector-from="list"][data-connector-to="detail"]');
    expect(connector).toBeTruthy();
    expect(connector?.querySelector('line')).toBeTruthy();
  });

  it('renders the connector label as SVG text', () => {
    const { container } = render(<ArtifactRenderer content={BOARD_MDX} />);
    expect(container.textContent).toContain('Open item');
  });

  it('silently skips a connector referencing an unknown screen id', () => {
    const { container } = render(<ArtifactRenderer content={BOARD_MDX} />);
    expect(container.querySelector('[data-connector-from="ghost"]')).toBeNull();
    expect(container.textContent).not.toContain('Unrenderable block');
  });

  it('anchors the annotation to its target screen with the requested placement', () => {
    const { container } = render(<ArtifactRenderer content={BOARD_MDX} />);
    const note = container.querySelector('[data-annotation-target="detail"][data-annotation-placement="top"]');
    expect(note).toBeTruthy();
    expect(note?.textContent).toContain('Heads up');
    expect(note?.textContent).toContain('Loads the full record on open.');
  });
});
