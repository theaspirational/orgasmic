import type { TaskSummary } from '@/lib/types';

export type TaskTreeNode = {
  task: TaskSummary;
  children: TaskTreeNode[];
};

// orgasmic:TASK-J3JB0
export function buildTaskTree(tasks: TaskSummary[]): TaskTreeNode[] {
  const nodes = new Map<string, TaskTreeNode>(
    tasks.map((task) => [task.id, { task, children: [] }]),
  );
  const roots: TaskTreeNode[] = [];

  for (const task of tasks) {
    const node = nodes.get(task.id)!;
    const parent = task.parent_task ? nodes.get(task.parent_task) : undefined;
    if (parent && parent !== node) parent.children.push(node);
    else roots.push(node);
  }

  return roots;
}

export function countTaskTreeNodes(nodes: TaskTreeNode[]): number {
  return nodes.reduce(
    (total, node) => total + 1 + countTaskTreeNodes(node.children),
    0,
  );
}
