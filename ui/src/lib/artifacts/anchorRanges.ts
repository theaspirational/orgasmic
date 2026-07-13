/** Locate a stored quote string inside a rendered artifact body as a DOM
 * `Range`, matching on whitespace-normalized text across element boundaries.
 *
 * Anchors are captured as normalized plain text (see `captureSelection` in
 * ArtifactView), so the match must be whitespace-insensitive: the rendered
 * markup may break a phrase across multiple text nodes and re-collapse
 * whitespace differently than the raw selection did. We walk every text node
 * under `root`, build a normalized character stream with a back-map to the
 * originating `(Text, offset)`, find the normalized quote as a substring, then
 * reconstitute a Range spanning the matched source positions. Pure DOM read —
 * no mutation — so it is safe to call on every render. */

type CharSource = { node: Text; offset: number };

function isWhitespace(ch: string): boolean {
  return ch === ' ' || ch === '\t' || ch === '\n' || ch === '\r' || ch === '\f' || ch === '\v';
}

function collectTextNodes(root: Node): Text[] {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  const nodes: Text[] = [];
  let current = walker.nextNode();
  while (current) {
    nodes.push(current as Text);
    current = walker.nextNode();
  }
  return nodes;
}

/** Find the first Range under `root` whose text (whitespace-normalized) equals
 * `quote` (also normalized). Returns null when the quote is empty or absent. */
export function findQuoteRange(root: Node, quote: string): Range | null {
  const target = quote.trim().replace(/\s+/g, ' ');
  if (!target) return null;

  let normalized = '';
  const sources: CharSource[] = [];
  let prevWasWhitespace = false;

  for (const node of collectTextNodes(root)) {
    const text = node.data;
    for (let i = 0; i < text.length; i += 1) {
      const ch = text[i];
      if (isWhitespace(ch)) {
        // Collapse a run of whitespace to a single space, anchored to the run's
        // first character (so a Range end lands right after it, excluding the
        // rest of the collapsed run).
        if (prevWasWhitespace) continue;
        prevWasWhitespace = true;
        normalized += ' ';
      } else {
        prevWasWhitespace = false;
        normalized += ch;
      }
      sources.push({ node, offset: i });
    }
  }

  const start = normalized.indexOf(target);
  if (start < 0) return null;
  const end = start + target.length - 1;
  const startSource = sources[start];
  const endSource = sources[end];
  if (!startSource || !endSource) return null;

  const range = document.createRange();
  range.setStart(startSource.node, startSource.offset);
  range.setEnd(endSource.node, endSource.offset + 1);
  return range;
}
