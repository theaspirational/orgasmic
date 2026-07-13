import { createContext, useContext } from 'react';

import type { CommentRecord } from '../types';

/** Bridge that lets the read-only artifact renderer host interactive answer
 * affordances (QuestionForm) without the renderer knowing anything about the
 * comment API. `ArtifactComments` provides a concrete value around
 * `<ArtifactRenderer>`; every other embed (RunSurface, fixtures, tests) leaves
 * the context at its `null` default, and interactive blocks fall back to their
 * read-only rendering. */
export type ArtifactInteraction = {
  /** True when the viewer may post answers (can comment, not regenerating, not
   * viewing an archived version). */
  canAnswer: boolean;
  /** Current displayed-version comments — the source for existing answers and
   * their Agree replies. */
  comments: CommentRecord[];
  /** Post an answer to a question as a normal comment carrying a structured
   * `{ kind: 'question', key, prompt }` anchor. */
  submitAnswer(input: { questionKey: string; prompt: string; message: string }): Promise<void>;
  /** Agree with an existing answer: posts a `reply_to` reply of "Agree". */
  agree(input: { cid: string }): Promise<void>;
};

export const ArtifactInteractionContext = createContext<ArtifactInteraction | null>(null);

/** Access the interaction bridge, or null when the renderer is embedded outside
 * an interactive comment host. */
export function useArtifactInteraction(): ArtifactInteraction | null {
  return useContext(ArtifactInteractionContext);
}
