import type { DecisionSummary } from '@/lib/types';

import { firstSentence } from './node-views/orgNodes';

const LEADING_LIST_MARKER = /^(?:(?:\d+[.)]|\(\d+\))|[-*+])\s+/;

export function decisionRowPresentation(decision: DecisionSummary): {
  title: string;
  preview: string | null;
} {
  const title = decision.title.trim() || decision.id;
  const body = decision.preview
    ?.replace(/\s+/g, ' ')
    .trim()
    .replace(LEADING_LIST_MARKER, '')
    .trim();
  const preview = body ? firstSentence(body) : null;

  return {
    title,
    preview: preview && preview !== title ? preview : null,
  };
}
