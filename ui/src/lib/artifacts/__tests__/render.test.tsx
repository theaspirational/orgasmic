// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, render, screen, within } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

// shiki is dynamic-imported from ui/blocks; vi.mock does not reliably
// intercept a dynamic import of a pre-bundled node_modules package under
// Vite's dep optimizer, so these tests exercise the real library (pure
// string tokenization, fine in jsdom). useTheme is mocked because it
// requires a mounted ThemeProvider this test doesn't need.
vi.mock('@/lib/theme', () => ({
  useTheme: () => ({ preference: 'system', resolved: 'paper', setPreference: vi.fn() }),
}));

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

  it('renders the SVG Diagram block into a sandboxed iframe with its <style> lifted into the srcdoc', () => {
    const { container } = render(<ArtifactRenderer content={ALL_BLOCKS_MDX} />);
    const iframes = Array.from(container.querySelectorAll('iframe'));
    const diagram = iframes.find((el) => (el.getAttribute('srcdoc') ?? '').includes('dd-arrow'));
    expect(diagram).toBeTruthy();
    const srcDoc = diagram?.getAttribute('srcdoc') ?? '';
    // The <style> child is lifted out of the fragment (the sanitizer forbids
    // it inline) and re-emitted as author CSS in the iframe head.
    expect(srcDoc).toContain('.dd-label');
    expect(srcDoc).toContain('text-anchor');
    expect(diagram?.getAttribute('sandbox')).toBe('');
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
