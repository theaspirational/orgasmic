import { describe, expect, it } from 'vitest';

import {
  backEntityPeek,
  closeEntityPeek,
  getEntityPeek,
  popEntityPeekTo,
  pushEntityPeek,
  withEntityPeek,
} from '@/lib/entityPeek';

describe('entity peek search state', () => {
  it('opens an entity without discarding the current route or filters', () => {
    const search = withEntityPeek(
      { task: 'TASK-FILTER', range: '7d', types: ['comment'] },
      'term_ORSL',
    );

    expect(search).toEqual({
      task: 'TASK-FILTER',
      range: '7d',
      types: ['comment'],
      drawer_stack: ['term_ORSL'],
      peek_task: undefined,
    });
    expect(getEntityPeek('/projects/orgasmic/activity', search)).toEqual({
      activeId: 'term_ORSL',
      source: 'drawer_stack',
      stack: ['term_ORSL'],
    });
  });

  it('does not treat the Activity task filter as a peek', () => {
    expect(getEntityPeek('/projects/orgasmic/activity', { task: 'TASK-FILTER' })).toBeNull();
  });

  it('keeps supporting task-page and global task deep links', () => {
    expect(getEntityPeek('/projects/orgasmic/tasks', { task: 'TASK-DEEP-LINK' })).toEqual({
      activeId: 'TASK-DEEP-LINK',
      source: 'task',
      stack: ['TASK-DEEP-LINK'],
    });
    expect(getEntityPeek('/projects/orgasmic/project', { peek_task: 'TASK-GLOBAL' })).toEqual({
      activeId: 'TASK-GLOBAL',
      source: 'peek_task',
      stack: ['TASK-GLOBAL'],
    });
  });

  it('migrates a legacy task deep link into one cross-kind history stack', () => {
    const next = pushEntityPeek(
      '/projects/orgasmic/tasks',
      { task: 'TASK-FIRST', layout: 'list' },
      'term_SECOND',
    );

    expect(next).toEqual({
      task: undefined,
      layout: 'list',
      drawer_stack: ['TASK-FIRST', 'term_SECOND'],
      peek_task: undefined,
    });
  });

  it('walks back through mixed entity kinds to the beginning', () => {
    const search = {
      q: 'routing',
      drawer_stack: ['TASK-FIRST', 'term_SECOND', 'dec_THIRD'],
    };

    const once = backEntityPeek('/projects/orgasmic/project', search);
    const twice = backEntityPeek('/projects/orgasmic/project', once);
    const closed = backEntityPeek('/projects/orgasmic/project', twice);

    expect(once.drawer_stack).toEqual(['TASK-FIRST', 'term_SECOND']);
    expect(twice.drawer_stack).toEqual(['TASK-FIRST']);
    expect(closed).toEqual({ q: 'routing', drawer_stack: undefined });
  });

  it('supports breadcrumb jumps and closes without losing unrelated search', () => {
    const search = {
      task: 'TASK-FILTER',
      actors: ['manager'],
      drawer_stack: ['term_FIRST', 'dec_SECOND', 'term_THIRD'],
    };

    expect(popEntityPeekTo(search, 0)).toEqual({
      ...search,
      drawer_stack: ['term_FIRST'],
    });
    expect(closeEntityPeek('/projects/orgasmic/activity', search)).toEqual({
      task: 'TASK-FILTER',
      actors: ['manager'],
      drawer_stack: undefined,
      peek_task: undefined,
    });
  });
});
