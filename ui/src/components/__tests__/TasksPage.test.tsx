// @vitest-environment jsdom

import { describe, expect, it } from 'vitest';

import type { TaskSummary } from '@/lib/types';

import { buildTaskTree, countTaskTreeNodes } from '../taskTree';

function task(
  id: string,
  parentTask: string | null = null,
  lifecycleStage = 'backlog',
): TaskSummary {
  return {
    id,
    title: id,
    lifecycle_stage: lifecycleStage,
    parent_task: parentTask,
    owner: 'human',
    tags: [],
    source_file: '.orgasmic/tasks/backlog.org',
  };
}

describe('buildTaskTree', () => {
  it('builds recursive task families even when children arrive before parents', () => {
    const roots = buildTaskTree([
      task('TASK-A.1.1', 'TASK-A.1', 'done'),
      task('TASK-A'),
      task('TASK-A.1', 'TASK-A', 'in_progress'),
      task('TASK-A.2', 'TASK-A'),
      task('TASK-B'),
    ]);

    expect(roots.map((node) => node.task.id)).toEqual(['TASK-A', 'TASK-B']);
    expect(roots[0].children.map((node) => node.task.id)).toEqual([
      'TASK-A.1',
      'TASK-A.2',
    ]);
    expect(roots[0].children[0].children[0].task.id).toBe('TASK-A.1.1');
    expect(countTaskTreeNodes(roots)).toBe(5);
  });

  it('keeps a task visible as a root when its parent is absent', () => {
    const roots = buildTaskTree([
      task('TASK-MISSING.1', 'TASK-MISSING'),
      task('TASK-ROOT'),
    ]);

    expect(roots.map((node) => node.task.id)).toEqual([
      'TASK-MISSING.1',
      'TASK-ROOT',
    ]);
  });
});
