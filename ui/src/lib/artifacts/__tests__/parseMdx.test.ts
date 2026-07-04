import { describe, expect, it } from 'vitest';

import { parseArtifactMdx, textBody } from '../parseMdx';
import { BLOCK_TYPES } from '../types';
import { ALL_BLOCKS_MDX } from '../__fixtures__/all-blocks';

function elements(nodes: ReturnType<typeof parseArtifactMdx>) {
  return nodes.filter((n) => n.kind === 'element');
}

describe('parseArtifactMdx', () => {
  it('parses a simple self-closing block with string/number/boolean/JSON props', () => {
    const nodes = parseArtifactMdx(
      '<Checklist items={[{"label":"Do it","done":true}]} maxLines={3} filename="x.ts" flag />',
    );
    expect(nodes).toHaveLength(1);
    const node = nodes[0]!;
    expect(node.kind).toBe('element');
    if (node.kind !== 'element') throw new Error('unreachable');
    expect(node.name).toBe('Checklist');
    expect(node.props.items).toEqual([{ label: 'Do it', done: true }]);
    expect(node.props.maxLines).toBe(3);
    expect(node.props.filename).toBe('x.ts');
    expect(node.props.flag).toBe(true);
  });

  it('treats lowercase HTML closing tags in children as literal text, not stray closing tags', () => {
    // Regression: Wireframe/Diagram/Screen html is authored as children, and
    // a naive closing-tag scan that doesn't check case (mirroring the
    // opening-tag rule) misreports every `</div>`/`</span>`/etc. inside real
    // markup as "unexpected closing tag" — this would break every wireframe
    // and diagram render in practice, since real HTML is full of them.
    const source =
      '<Wireframe surface="panel"><div class="wf-card"><span>Hi</span><h3>Title</h3></div></Wireframe>';
    const nodes = parseArtifactMdx(source);
    expect(nodes).toHaveLength(1);
    const wireframe = nodes[0]!;
    if (wireframe.kind !== 'element') throw new Error('unreachable');
    expect(wireframe.name).toBe('Wireframe');
    expect(wireframe.children.some((n) => n.kind === 'error')).toBe(false);
    expect(textBody(wireframe, 'html')).toContain('<span>Hi</span>');
    expect(textBody(wireframe, 'html')).toContain('</div>');
  });

  it('handles same-name nesting by real recursion, not first-match', () => {
    const source = '<Section title="Outer"><Section title="Inner">inner text</Section> outer tail</Section>';
    const nodes = parseArtifactMdx(source);
    const outer = elements(nodes)[0]!;
    if (outer.kind !== 'element') throw new Error('unreachable');
    expect(outer.props.title).toBe('Outer');
    const innerElements = elements(outer.children);
    expect(innerElements).toHaveLength(1);
    const inner = innerElements[0]!;
    if (inner.kind !== 'element') throw new Error('unreachable');
    expect(inner.props.title).toBe('Inner');
    // The outer tail text must survive as a sibling AFTER the inner block —
    // a first-match scan (the daemon's own validate_mdx) would have closed
    // Outer at the inner block's </Section> and left this text orphaned.
    const tailText = outer.children.find((n) => n.kind === 'text' && n.markdown.includes('outer tail'));
    expect(tailText).toBeTruthy();
  });

  it('reads a Code body with a literal `</Code>`-shaped substring from the code attribute unharmed', () => {
    const source = '<Code language="mdx" code={`<Code>const done = true;</Code>`} />';
    const nodes = parseArtifactMdx(source);
    const node = nodes[0]!;
    if (node.kind !== 'element') throw new Error('unreachable');
    expect(node.name).toBe('Code');
    expect(textBody(node, 'code')).toBe('<Code>const done = true;</Code>');
  });

  it('does not throw on a malformed (unclosed) block and instead reports an error node', () => {
    expect(() => parseArtifactMdx('<Callout tone="info">unterminated')).not.toThrow();
    const nodes = parseArtifactMdx('<Callout tone="info">unterminated');
    // The tag itself still parses (it has a valid open tag); the "unclosed"
    // error surfaces inside its children, where the missing </Callout> would
    // have been — not as a top-level sibling.
    expect(nodes).toHaveLength(1);
    const callout = nodes[0]!;
    if (callout.kind !== 'element') throw new Error('unreachable');
    expect(callout.children.some((n) => n.kind === 'error')).toBe(true);
  });

  it('reports (without throwing) an unknown top-level tag', () => {
    const nodes = parseArtifactMdx('<NotARealBlock foo="bar" />');
    expect(nodes).toHaveLength(1);
    expect(nodes[0]!.kind).toBe('element');
    // The registry check happens at render dispatch, not the parser — the
    // parser only needs to produce a well-formed node for an unknown tag,
    // never throw. Confirm it round-trips the name/props for the renderer
    // to reject.
    if (nodes[0]!.kind !== 'element') throw new Error('unreachable');
    expect(nodes[0]!.name).toBe('NotARealBlock');
  });

  it('recovers from one malformed nested block without losing later siblings', () => {
    const source = '<Section><Callout tone="info">ok</Callout><Broken attr={unterminated</Section><Table headers={["a"]} rows={[["1"]]} />';
    const nodes = parseArtifactMdx(source);
    // The top-level scan must still find the trailing Table even though a
    // block earlier in the document failed to parse.
    const names = elements(nodes).map((n) => (n.kind === 'element' ? n.name : ''));
    expect(names).toContain('Table');
  });

  it('parses the all-blocks fixture with all 22 registered tags represented at some depth, no throw', () => {
    expect(() => parseArtifactMdx(ALL_BLOCKS_MDX)).not.toThrow();
    const nodes = parseArtifactMdx(ALL_BLOCKS_MDX);

    function collectNames(list: typeof nodes, out: Set<string>) {
      for (const node of list) {
        if (node.kind === 'element') {
          out.add(node.name);
          collectNames(node.children, out);
        }
      }
    }
    const found = new Set<string>();
    collectNames(nodes, found);
    for (const blockType of BLOCK_TYPES) {
      expect(found.has(blockType)).toBe(true);
    }
    // No parse-error nodes anywhere in the well-formed fixture.
    function collectErrors(list: typeof nodes, out: string[]) {
      for (const node of list) {
        if (node.kind === 'error') out.push(node.message);
        if (node.kind === 'element') collectErrors(node.children, out);
      }
    }
    const errors: string[] = [];
    collectErrors(nodes, errors);
    expect(errors).toEqual([]);
  });

  it('parses a nested Columns/Tabs structure inside a Section', () => {
    const nodes = parseArtifactMdx(ALL_BLOCKS_MDX);
    function find(list: typeof nodes, name: string): (typeof nodes)[number] | undefined {
      for (const node of list) {
        if (node.kind === 'element') {
          if (node.name === name) return node;
          const nested = find(node.children, name);
          if (nested) return nested;
        }
      }
      return undefined;
    }
    const section = find(nodes, 'Section');
    expect(section).toBeTruthy();
    if (!section || section.kind !== 'element') throw new Error('unreachable');
    const columns = find(section.children, 'Columns');
    expect(columns).toBeTruthy();
  });
});
