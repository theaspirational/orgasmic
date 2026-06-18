import { describe, it, expect } from 'vitest';
import { kanbanStage, KANBAN_COLUMNS } from '../kanbanUtils';

describe('KANBAN_COLUMNS', () => {
  it('contains all active v3 lifecycle stages', () => {
    expect(KANBAN_COLUMNS).toContain('backlog');
    expect(KANBAN_COLUMNS).toContain('todo');
    expect(KANBAN_COLUMNS).toContain('in_progress');
    expect(KANBAN_COLUMNS).toContain('in_review');
    expect(KANBAN_COLUMNS).toContain('done');
  });

  it('does not include cancelled', () => {
    expect(KANBAN_COLUMNS).not.toContain('cancelled');
  });
});

describe('kanbanStage', () => {
  it('maps backlog to backlog', () => {
    expect(kanbanStage('backlog')).toBe('backlog');
  });

  it('maps todo to todo', () => {
    expect(kanbanStage('todo')).toBe('todo');
  });

  it('maps in_progress to in_progress', () => {
    expect(kanbanStage('in_progress')).toBe('in_progress');
  });

  it('maps in_review to in_review', () => {
    expect(kanbanStage('in_review')).toBe('in_review');
  });

  it('maps done to done', () => {
    expect(kanbanStage('done')).toBe('done');
  });

  it('returns null for cancelled (excluded from kanban)', () => {
    expect(kanbanStage('cancelled')).toBeNull();
  });

  it('maps unknown stage to backlog fallback', () => {
    expect(kanbanStage('WEIRD_STAGE')).toBe('backlog');
  });

  it('maps null to backlog fallback', () => {
    expect(kanbanStage(null)).toBe('backlog');
  });

  it('maps undefined to backlog fallback', () => {
    expect(kanbanStage(undefined)).toBe('backlog');
  });

  it('maps empty string to backlog fallback', () => {
    expect(kanbanStage('')).toBe('backlog');
  });

  it('maps arbitrary uppercase stage to backlog fallback', () => {
    expect(kanbanStage('IMPLEMENT')).toBe('backlog');
  });
});
