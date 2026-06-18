// orgasmic:task_W43NY,dec_QWEQ8,dec_P7EY8

export type PerformerPillDescriptor = {
  label: string;
  performer: string;
  verb: string;
};

function capitalize(s: string): string {
  if (!s) return s;
  return s.charAt(0).toUpperCase() + s.slice(1);
}

function resolveVerb(subState: string | null | undefined): string {
  if (!subState) return 'working';
  const dot = subState.indexOf('.');
  return dot >= 0 && dot < subState.length - 1 ? subState.slice(dot + 1) : subState || 'working';
}

/**
 * Collapse a live run's role + worker into one performer pill descriptor.
 * Returns null when hasLiveRun is false (caller renders idle chips as-is).
 * The worker id headlines when it carries more info than the bare role
 * (role "implementer", worker "implementer-claude-rmux" → the worker);
 * when they coincide ("manager", dispatch worker "reviewer") the role shows.
 * When live: "Reviewer · working" with dot separator per dec_041.
 */
export function buildPerformerPill(
  role: string | null | undefined,
  workerId: string | null | undefined,
  subState: string | null | undefined,
  hasLiveRun: boolean,
): PerformerPillDescriptor | null {
  if (!hasLiveRun) return null;
  const base = role || 'agent';
  const performer = workerId && workerId !== base ? workerId : base;
  const verb = resolveVerb(subState);
  return { label: `${capitalize(performer)} · ${verb}`, performer, verb };
}
