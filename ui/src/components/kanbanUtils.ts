import type { LifecycleStage } from '../lib/types';
import { LIFECYCLE_ACTIVE_STAGES } from '../lib/types';

export const KANBAN_COLUMNS: LifecycleStage[] = LIFECYCLE_ACTIVE_STAGES;
export const KANBAN_COLUMN_SET = new Set<string>(KANBAN_COLUMNS);

export function kanbanStage(stage: string | null | undefined): LifecycleStage | null {
  if (stage === 'cancelled') return null;
  return KANBAN_COLUMN_SET.has(stage ?? '') ? (stage as LifecycleStage) : 'backlog';
}
