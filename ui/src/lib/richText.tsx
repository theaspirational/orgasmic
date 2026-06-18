import { createContext, Fragment, useContext, useMemo, type ReactNode } from 'react';
import { useNavigate } from '@tanstack/react-router';

import { fetchGlossary } from '@/lib/api';
import { appendDrawerStack, routeSearch } from '@/lib/searchState';
import { useResource } from '@/lib/useResource';

/**
 * Shared inline decoration for plain-text prose across the app (manager feed,
 * task descriptions, activity comments). Recognizes three token classes without
 * requiring authored org markup:
 *   - `:UPPER_SNAKE:` org property/config keys  -> inline code
 *   - TASK-/dec_/arch_ node ids                 -> clickable entity links
 *   - glossary canonical phrases                -> clickable glossary links
 *
 * The matchers live behind a context so any surface can decorate without prop
 * drilling; when no provider is mounted, `:KEY:` styling still applies and the
 * link classes degrade to plain text.
 */

type RichTextValue = {
  openEntity: (token: string) => void;
  openGlossary: (id: string) => void;
  /** Regex alternation source (no flags/anchors) of glossary phrases, or null. */
  glossaryPattern: string | null;
  /** Lowercased phrase -> glossary node id. */
  glossaryLookup: Map<string, string>;
};

const RichTextContext = createContext<RichTextValue | null>(null);

export function useRichText(): RichTextValue | null {
  return useContext(RichTextContext);
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

export function RichTextProvider({
  projectId,
  children,
}: {
  projectId: string | null;
  children: ReactNode;
}) {
  const navigate = useNavigate();
  const glossary = useResource(
    `richtext-glossary:${projectId ?? 'none'}`,
    () => fetchGlossary(projectId!),
    { enabled: Boolean(projectId) },
  );

  const value = useMemo<RichTextValue | null>(() => {
    if (!projectId) return null;

    const openEntity = (token: string) => {
      if (/^TASK-/.test(token)) {
        void navigate({
          to: '/projects/$projectId/tasks',
          params: { projectId },
          search: routeSearch((prev) => ({ ...prev, task: token })),
        });
      } else if (/^dec_/.test(token)) {
        void navigate({
          to: '/projects/$projectId/decisions',
          params: { projectId },
          search: routeSearch((prev) => appendDrawerStack(prev, token)),
        });
      } else if (/^arch_/.test(token)) {
        void navigate({
          to: '/projects/$projectId/architecture',
          params: { projectId },
          search: routeSearch((prev) => appendDrawerStack(prev, token)),
        });
      }
    };

    const openGlossary = (id: string) => {
      void navigate({
        to: '/projects/$projectId/glossary',
        params: { projectId },
        search: routeSearch((prev) => appendDrawerStack(prev, id)),
      });
    };

    // Build the glossary matcher: distinctive canonical phrases only (>= 4
    // chars), longest-first so the alternation prefers the most specific match.
    const lookup = new Map<string, string>();
    const phrases: string[] = [];
    for (const term of glossary.data ?? []) {
      const label = (term.canonical ?? term.id).trim();
      if (label.length < 4) continue;
      const lower = label.toLowerCase();
      if (lookup.has(lower)) continue;
      lookup.set(lower, term.id);
      phrases.push(label);
    }
    phrases.sort((a, b) => b.length - a.length);
    const glossaryPattern = phrases.length > 0 ? phrases.map(escapeRegExp).join('|') : null;

    return { openEntity, openGlossary, glossaryPattern, glossaryLookup: lookup };
  }, [projectId, navigate, glossary.data]);

  return <RichTextContext.Provider value={value}>{children}</RichTextContext.Provider>;
}

const ENTITY_STEM = String.raw`(?:\d+|(?=[0-9A-HJKMNP-TV-Z]{0,4}[A-HJKMNP-TV-Z])[0-9A-HJKMNP-TV-Z]{5})`;
const KEY_OR_ENTITY_RE = new RegExp(
  String.raw`(:[A-Z][A-Z0-9_]{2,}:)|\b(TASK-${ENTITY_STEM}(?:\.\d+)*|dec_${ENTITY_STEM}|arch_${ENTITY_STEM}(?:\.\d+)*)\b`,
  'g',
);

const KEY_CLASS = 'rounded bg-muted px-1 py-0.5 font-mono text-[0.9em] text-muted-foreground';
const LINK_CLASS =
  'font-mono text-[0.95em] font-medium text-primary underline-offset-2 hover:underline';
const GLOSSARY_CLASS = 'text-primary underline decoration-dotted underline-offset-2 hover:decoration-solid';

/**
 * Turns a plain-text run into React nodes with the three decoration classes.
 * Safe on free-form prose: only well-delimited token patterns are matched, so
 * bare slashes (file paths, ratios) and asterisks are left untouched.
 */
export function decorateText(text: string, ctx: RichTextValue | null): ReactNode[] {
  if (!text) return [text];
  const out: ReactNode[] = [];
  const seenGlossary = new Set<string>();
  const counter = { n: 0 };

  const pushGlossary = (slice: string) => {
    if (!slice) return;
    if (!ctx?.glossaryPattern) {
      out.push(slice);
      return;
    }
    const re = new RegExp(`\\b(${ctx.glossaryPattern})\\b`, 'gi');
    let last = 0;
    let match: RegExpExecArray | null;
    while ((match = re.exec(slice)) !== null) {
      const id = ctx.glossaryLookup.get(match[0].toLowerCase());
      // Link only the first hit per term per run to keep dense prose readable.
      if (!id || seenGlossary.has(id)) continue;
      // Skip terms embedded in a path or identifier (e.g. "orgasmic" inside
      // crates/orgasmic-daemon) — `\b` treats / and - as boundaries, so guard
      // them. Note: not `.`, or a term ending a sentence ("manager agent.")
      // would wrongly be suppressed.
      const before = slice[match.index - 1];
      const after = slice[match.index + match[0].length];
      if (before && '/-'.includes(before)) continue;
      if (after && '/-'.includes(after)) continue;
      seenGlossary.add(id);
      if (match.index > last) out.push(slice.slice(last, match.index));
      out.push(
        <button
          key={`g${counter.n++}`}
          type="button"
          className={GLOSSARY_CLASS}
          onClick={() => ctx.openGlossary(id)}
        >
          {match[0]}
        </button>,
      );
      last = match.index + match[0].length;
    }
    if (last < slice.length) out.push(slice.slice(last));
  };

  let last = 0;
  let match: RegExpExecArray | null;
  const re = new RegExp(KEY_OR_ENTITY_RE);
  while ((match = re.exec(text)) !== null) {
    if (match.index > last) pushGlossary(text.slice(last, match.index));
    if (match[1]) {
      out.push(
        <code key={`k${counter.n++}`} className={KEY_CLASS}>
          {match[1]}
        </code>,
      );
    } else {
      const token = match[2]!;
      out.push(
        ctx ? (
          <button
            key={`e${counter.n++}`}
            type="button"
            className={LINK_CLASS}
            onClick={() => ctx.openEntity(token)}
          >
            {token}
          </button>
        ) : (
          token
        ),
      );
    }
    last = match.index + match[0].length;
  }
  pushGlossary(text.slice(last));
  return out;
}

/** Convenience wrapper so callers can drop decorated prose into JSX directly. */
export function DecoratedText({ text }: { text: string }): ReactNode {
  const ctx = useRichText();
  return <Fragment>{decorateText(text, ctx)}</Fragment>;
}
