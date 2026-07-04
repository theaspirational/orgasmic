// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

// shiki/mermaid are dynamic-imported from ui/blocks; vi.mock does not
// reliably intercept a dynamic import of a pre-bundled node_modules package
// under Vite's dep optimizer, so these tests exercise the real libraries
// (both work fine in jsdom: shiki is pure string tokenization, and mermaid's
// own parser/error-diagram path never throws uncaught). useTheme is mocked
// because it requires a mounted ThemeProvider this test doesn't need.
vi.mock('@/lib/theme', () => ({
  useTheme: () => ({ preference: 'system', resolved: 'paper', setPreference: vi.fn() }),
}));

// jsdom has no SVG layout engine (no getBBox/getComputedTextLength), which
// mermaid's dagre-based layout needs to measure node/label boxes. Polyfill
// with a fixed box — good enough for "did it render", not pixel-accurate.
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
import { ALL_BLOCKS_MDX } from '../__fixtures__/all-blocks';

describe('ArtifactRenderer (fixture render smoke test)', () => {
  it('renders the all-blocks fixture without throwing', () => {
    expect(() => render(<ArtifactRenderer content={ALL_BLOCKS_MDX} />)).not.toThrow();
  });

  it('renders the default-active Code tab with no unrenderable-block error', () => {
    const { container } = render(<ArtifactRenderer content={ALL_BLOCKS_MDX} />);
    expect(container.textContent).toContain('ThemeField');
    expect(container.textContent).not.toContain('Unrenderable block');
  });

  it('renders a Code block whose body carries a literal `</Code>`-shaped substring, unmangled', () => {
    // Structural proof that the parser resolves this via the code={`...`}
    // template-literal attribute (not children) lives in parseMdx.test.ts;
    // this is the render-level confirmation that it reaches the DOM intact
    // and does not trip the unrenderable-block fallback.
    const { container } = render(<ArtifactRenderer content={ALL_BLOCKS_MDX} />);
    expect(container.textContent).toContain('const done = true');
    expect(container.textContent).not.toContain('Unrenderable block');
  });

  it('renders a Table with its headers and rows', () => {
    render(<ArtifactRenderer content={ALL_BLOCKS_MDX} />);
    expect(screen.getByText('widget_theme_overrides table')).toBeInTheDocument();
    // "widget_id" also appears as a DataModel field name elsewhere in the
    // fixture, so assert presence rather than uniqueness.
    expect(screen.getAllByText('widget_id').length).toBeGreaterThanOrEqual(1);
  });

  it('renders a Checklist with its items', () => {
    render(<ArtifactRenderer content={ALL_BLOCKS_MDX} />);
    expect(screen.getByText('Settings panel exposes a theme selector')).toBeInTheDocument();
  });

  it('renders a Wireframe artboard container using semantic wireframe classes', () => {
    const { container } = render(<ArtifactRenderer content={ALL_BLOCKS_MDX} />);
    const wireframe = container.querySelector('.orgasmic-wireframe');
    expect(wireframe).toBeTruthy();
    expect(wireframe?.innerHTML).toContain('Appearance');
  });

  it('mounts a real Mermaid SVG for the diagram blocks (Mermaid + SequenceDiagram + FlowChart)', async () => {
    const { container } = render(<ArtifactRenderer content={ALL_BLOCKS_MDX} />);
    await waitFor(
      () => {
        const svgs = container.querySelectorAll('.mermaid-diagram svg');
        expect(svgs.length).toBe(3);
      },
      { timeout: 5000 },
    );
  });

  it('renders the Prototype block inside a sandboxed iframe with no allow-same-origin', () => {
    const { container } = render(<ArtifactRenderer content={ALL_BLOCKS_MDX} />);
    const iframes = Array.from(container.querySelectorAll('iframe'));
    const sandboxed = iframes.filter((el) => el.hasAttribute('sandbox'));
    expect(sandboxed.length).toBeGreaterThan(0);
    for (const iframe of sandboxed) {
      expect(iframe.getAttribute('sandbox')).not.toContain('allow-same-origin');
    }
  });

  it('renders a Canvas with multiple labeled artboards', () => {
    render(<ArtifactRenderer content={ALL_BLOCKS_MDX} />);
    // "Before"/"After" label both a Columns comparison and a Canvas artboard
    // pair in this fixture — both are legitimate, so assert at least 2 of
    // each rather than a single unique match.
    expect(screen.getAllByText('Before').length).toBeGreaterThanOrEqual(2);
    expect(screen.getAllByText('After').length).toBeGreaterThanOrEqual(2);
  });

  it('renders nested Columns inside a Section', () => {
    render(<ArtifactRenderer content={ALL_BLOCKS_MDX} />);
    expect(screen.getByText('Before / after')).toBeInTheDocument();
  });

  it('theme smoke: the wireframe container carries no raw hex color, only token-driven classes/styles', () => {
    const { container } = render(<ArtifactRenderer content={ALL_BLOCKS_MDX} />);
    document.documentElement.dataset.theme = 'paper';
    const lightHtml = container.querySelector('.orgasmic-wireframe')?.outerHTML ?? '';
    document.documentElement.dataset.theme = 'black-paper';
    const darkHtml = container.querySelector('.orgasmic-wireframe')?.outerHTML ?? '';
    expect(lightHtml).not.toMatch(/#[0-9a-fA-F]{3,6}\b/);
    expect(darkHtml).not.toMatch(/#[0-9a-fA-F]{3,6}\b/);
  });

  it('does not blow up the document when an unknown block type appears', () => {
    const { container } = render(
      <ArtifactRenderer content={'<NotARealBlock foo="bar" /><Callout tone="info">still renders</Callout>'} />,
    );
    expect(within(container).getByText('still renders')).toBeInTheDocument();
    expect(container.textContent).toContain('Unrenderable block');
  });
});
