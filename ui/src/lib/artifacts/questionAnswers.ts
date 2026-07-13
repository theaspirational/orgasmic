import type { CommentRecord } from '../types';
import { parseQuestionAnchor } from './questionKey';

/** A member's in-progress answer to one question. `other` is null unless the
 * "Other" write-in choice is active; when active it holds the typed text. */
export type AnswerSelection =
  | { type: 'single'; label: string | null; other: string | null }
  | { type: 'multi'; labels: string[]; other: string | null }
  | { type: 'freeform'; text: string };

/** Human-readable comment body for an answer (a normal comment, so it must read
 * naturally). single → the chosen label, or `Other: <text>`; multi → selected
 * labels joined with `; ` (plus `Other: <text>` when the write-in is used);
 * freeform → the text. */
export function buildAnswerMessage(selection: AnswerSelection): string {
  if (selection.type === 'freeform') return selection.text.trim();
  if (selection.type === 'single') {
    if (selection.other != null) return `Other: ${selection.other.trim()}`;
    return selection.label ?? '';
  }
  const parts = [...selection.labels];
  if (selection.other != null) parts.push(`Other: ${selection.other.trim()}`);
  return parts.join('; ');
}

/** Whether the answer is complete enough to submit. Drives the Submit button's
 * disabled state: a chosen "Other" must carry text before it counts. */
export function isAnswerComplete(selection: AnswerSelection): boolean {
  if (selection.type === 'freeform') return selection.text.trim().length > 0;
  if (selection.type === 'single') {
    if (selection.other != null) return selection.other.trim().length > 0;
    return selection.label != null;
  }
  // multi: an active-but-empty "Other" blocks submission.
  if (selection.other != null && selection.other.trim().length === 0) return false;
  return selection.labels.length > 0 || selection.other != null;
}

/** The latest answer comment per author for one question key, in first-seen
 * author order. Comments arrive in append order, so the last matching comment
 * from an author wins (a re-answer supersedes their earlier one). */
export function latestAnswersPerAuthor(comments: CommentRecord[], key: string): CommentRecord[] {
  const byAuthor = new Map<string, CommentRecord>();
  for (const comment of comments) {
    const anchor = parseQuestionAnchor(comment.anchor);
    if (anchor && anchor.key === key) {
      byAuthor.set(comment.author, comment);
    }
  }
  return Array.from(byAuthor.values());
}

/** Distinct author names who replied "Agree" to the given answer comment, in
 * first-seen order. */
export function agreeAuthors(comments: CommentRecord[], answerCid: string): string[] {
  const names: string[] = [];
  const seen = new Set<string>();
  for (const comment of comments) {
    if (comment.reply_to === answerCid && comment.message.trim() === 'Agree' && !seen.has(comment.author)) {
      seen.add(comment.author);
      names.push(comment.author);
    }
  }
  return names;
}
