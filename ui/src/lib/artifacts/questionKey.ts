/** Structured anchor stored on a comment that answers a QuestionForm question.
 * Lives in the comment's `anchor` slot (the daemon round-trips it verbatim). */
export type QuestionAnchor = { kind: 'question'; key: string; prompt: string };

/** Parse a comment's `anchor` string as a question-answer anchor. Returns null
 * for plain selection-text anchors, `{}`, or malformed JSON — callers treat
 * those as ordinary anchored/unanchored comments. */
export function parseQuestionAnchor(anchor: string | undefined | null): QuestionAnchor | null {
  if (!anchor) return null;
  const trimmed = anchor.trim();
  if (!trimmed || trimmed === '{}' || trimmed[0] !== '{') return null;
  try {
    const parsed = JSON.parse(trimmed) as unknown;
    if (
      parsed &&
      typeof parsed === 'object' &&
      (parsed as { kind?: unknown }).kind === 'question' &&
      typeof (parsed as { key?: unknown }).key === 'string'
    ) {
      const record = parsed as { key: string; prompt?: unknown };
      return {
        kind: 'question',
        key: record.key,
        prompt: typeof record.prompt === 'string' ? record.prompt : '',
      };
    }
  } catch {
    return null;
  }
  return null;
}

/** Whitespace-normalize a question prompt: trim ends and collapse every
 * internal run of whitespace (including newlines) to a single space. Two
 * prompts that differ only in incidental whitespace hash to the same key. */
export function normalizeQuestionPrompt(prompt: string): string {
  return prompt.trim().replace(/\s+/g, ' ');
}

/** Short stable identity for a QuestionForm question, used to (a) group answer
 * comments by the question they answer and (b) target the question element for
 * navigation. FNV-1a 32-bit over the whitespace-normalized prompt, rendered as
 * 8 lowercase hex chars. Identical prompts across forms deliberately share a
 * key — question identity is prompt text, not form position. */
export function questionKey(prompt: string): string {
  const normalized = normalizeQuestionPrompt(prompt);
  let hash = 0x811c9dc5; // FNV offset basis (2166136261)
  for (let i = 0; i < normalized.length; i += 1) {
    hash ^= normalized.charCodeAt(i) & 0xff;
    // FNV prime 16777619, kept in 32-bit unsigned space via Math.imul.
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0).toString(16).padStart(8, '0');
}
