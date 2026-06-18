import { describe, it, expect } from 'vitest';
import { buildPerformerPill } from '../performerPill';

describe('buildPerformerPill', () => {
  // idle → no pill
  it('returns null when hasLiveRun is false', () => {
    expect(buildPerformerPill('implementer', 'implementer-claude-rmux', 'implementer.working', false)).toBeNull();
  });

  // live + worker id matching the bare role (dispatch worker named after it)
  it('worker id equal to role — pill shows role label only', () => {
    const pill = buildPerformerPill('reviewer', 'reviewer', 'reviewer.working', true);
    expect(pill).not.toBeNull();
    expect(pill?.label).toBe('Reviewer · working');
    expect(pill?.performer).toBe('reviewer');
    expect(pill?.verb).toBe('working');
  });

  // live + named worker (adds info beyond the role)
  it('named worker adds info — pill shows full worker id', () => {
    const pill = buildPerformerPill('implementer', 'implementer-claude-rmux', 'implementer.working', true);
    expect(pill).not.toBeNull();
    expect(pill?.label).toBe('Implementer-claude-rmux · working');
    expect(pill?.performer).toBe('implementer-claude-rmux');
  });

  it('reviewer role with named reviewer worker headlines the worker', () => {
    const pill = buildPerformerPill('reviewer', 'reviewer-codex-acp', 'reviewer.working', true);
    expect(pill?.label).toBe('Reviewer-codex-acp · working');
  });

  it('null worker falls back to role', () => {
    const pill = buildPerformerPill('reviewer', null, 'reviewer.working', true);
    expect(pill?.label).toBe('Reviewer · working');
  });

  it('null subState defaults verb to working', () => {
    const pill = buildPerformerPill('implementer', 'implementer', null, true);
    expect(pill?.verb).toBe('working');
    expect(pill?.label).toBe('Implementer · working');
  });

  it('extracts verb from sub_state namespace prefix', () => {
    const pill = buildPerformerPill('reviewer', 'reviewer', 'reviewer.approved', true);
    expect(pill?.verb).toBe('approved');
    expect(pill?.label).toBe('Reviewer · approved');
  });

  it('null role falls back to agent', () => {
    const pill = buildPerformerPill(null, null, 'implementer.working', true);
    expect(pill?.performer).toBe('agent');
    expect(pill?.label).toBe('Agent · working');
  });

  it('manager run shows manager', () => {
    const pill = buildPerformerPill('manager', 'manager', null, true);
    expect(pill?.label).toBe('Manager · working');
  });
});
