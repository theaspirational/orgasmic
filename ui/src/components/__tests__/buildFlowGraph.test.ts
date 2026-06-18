import { beforeAll, describe, expect, it } from 'vitest';

import type {
  ArchitectureSummary,
  DecisionSummary,
  GlossarySummary,
  GraphEdgeSummary,
  GraphLayoutEntry,
  GraphNodeSummary,
  TaskSummary,
} from '@/lib/types';

// GraphView.tsx pulls in src/lib/transport.ts whose module-load reads `window.location`.
// vitest runs in 'node' env; stub a minimal `window` BEFORE the dynamic import so the
// rest of the runtime stays intact.
(globalThis as unknown as { window: unknown }).window = {
  location: { origin: 'http://localhost' },
  localStorage: {
    getItem: () => null,
    setItem: () => undefined,
    removeItem: () => undefined,
  },
};

type BuildFlowGraphFn = typeof import('../GraphView')['buildFlowGraph'];
let buildFlowGraph: BuildFlowGraphFn;

beforeAll(async () => {
  ({ buildFlowGraph } = await import('../GraphView'));
});

const DEFAULT_CURATION = {
  glossary: false,
  artifacts: false,
  backbone: true,
  dependsOn: false,
  dataflow: false,
};

function taskSummary(id: string, stage: string, title = id): TaskSummary {
  return {
    id,
    title,
    lifecycle_stage: stage,
    owner: 'tester',
    tags: [],
    source_file: 'tasks.org',
  };
}

function decisionSummary(id: string, title = id): DecisionSummary {
  return {
    id,
    title,
    tags: [],
    glossary_refs: [],
    source_file: 'decisions.org',
  };
}

function archSummary(id: string, label = id, parent_id: string | null = null): ArchitectureSummary {
  return {
    id,
    label,
    motivated_by: [],
    glossary_refs: [],
    interface: [],
    constraints: [],
    depends_on: [],
    parent_id,
    source_file: 'architecture.org',
  };
}

function graphNode(id: string, layer: string, opts: { superseded?: boolean } = {}): GraphNodeSummary {
  return {
    id,
    layer,
    outgoing: [],
    source_file: `${layer}.org`,
    ...(opts.superseded !== undefined ? { superseded: opts.superseded } : {}),
  };
}

function callBuild(opts: {
  nodes: GraphNodeSummary[];
  edges?: GraphEdgeSummary[];
  layout?: Record<string, GraphLayoutEntry>;
  localOverrides?: Record<string, GraphLayoutEntry>;
  tasks?: TaskSummary[];
  decisions?: DecisionSummary[];
  architecture?: ArchitectureSummary[];
  glossary?: GlossarySummary[];
  curation?: typeof DEFAULT_CURATION;
  selectedId?: string | null;
}) {
  return buildFlowGraph({
    nodes: opts.nodes,
    edges: opts.edges ?? [],
    layout: opts.layout ?? {},
    localOverrides: opts.localOverrides ?? {},
    tasks: opts.tasks ?? [],
    decisions: opts.decisions ?? [],
    architecture: opts.architecture ?? [],
    glossary: opts.glossary ?? [],
    curation: opts.curation ?? DEFAULT_CURATION,
    selectedId: opts.selectedId ?? null,
  });
}

describe('buildFlowGraph', () => {
  it('is deterministic: identical inputs produce identical node positions and order', () => {
    const nodes = [
      graphNode('dec_AAAAA', 'decision'),
      graphNode('dec_BBBBB', 'decision'),
      graphNode('arch_X', 'architecture'),
      graphNode('TASK-AAAAA', 'task'),
    ];
    const decisions = [decisionSummary('dec_AAAAA'), decisionSummary('dec_BBBBB')];
    const architecture = [archSummary('arch_X')];
    const tasks = [taskSummary('TASK-AAAAA', 'todo')];

    const r1 = callBuild({ nodes, decisions, architecture, tasks });
    const r2 = callBuild({ nodes, decisions, architecture, tasks });
    expect(r1.nodes.map((n) => [n.id, n.position.x, n.position.y]))
      .toEqual(r2.nodes.map((n) => [n.id, n.position.x, n.position.y]));
    expect(r1.edges).toEqual(r2.edges);
  });

  it('filters out done tasks but keeps backlog/todo/in_progress/in_review', () => {
    const nodes = [
      graphNode('TASK-AAAAA', 'task'),
      graphNode('TASK-BBBBB', 'task'),
      graphNode('TASK-CCCCC', 'task'),
      graphNode('TASK-DDDDD', 'task'),
      graphNode('TASK-EEEEE', 'task'),
    ];
    const tasks = [
      taskSummary('TASK-AAAAA', 'backlog'),
      taskSummary('TASK-BBBBB', 'todo'),
      taskSummary('TASK-CCCCC', 'in_progress'),
      taskSummary('TASK-DDDDD', 'in_review'),
      taskSummary('TASK-EEEEE', 'done'),
    ];
    const built = callBuild({ nodes, tasks });
    const renderedIds = built.nodes.map((n) => n.id);
    expect(renderedIds).toContain('TASK-AAAAA');
    expect(renderedIds).toContain('TASK-BBBBB');
    expect(renderedIds).toContain('TASK-CCCCC');
    expect(renderedIds).toContain('TASK-DDDDD');
    expect(renderedIds).not.toContain('TASK-EEEEE');
  });

  it('default curation includes backbone edges and excludes depends_on/dataflow', () => {
    const nodes = [
      graphNode('arch_A', 'architecture'),
      graphNode('arch_B', 'architecture'),
      graphNode('dec_A', 'decision'),
      graphNode('TASK-AAAAA', 'task'),
    ];
    const architecture = [archSummary('arch_A'), archSummary('arch_B')];
    const decisions = [decisionSummary('dec_A')];
    const tasks = [taskSummary('TASK-AAAAA', 'todo')];
    const edges: GraphEdgeSummary[] = [
      { kind: 'motivated_by', from: 'arch_A', to: 'dec_A' },
      { kind: 'implements', from: 'TASK-AAAAA', to: 'arch_A' },
      { kind: 'depends_on', from: 'arch_A', to: 'arch_B' },
      { kind: 'dataflow', from: 'arch_A', to: 'arch_B' },
    ];
    const built = callBuild({ nodes, edges, architecture, decisions, tasks });
    const kinds = built.edges.map((e) => e.data?.kind);
    expect(kinds).toContain('motivated_by');
    expect(kinds).toContain('implements');
    expect(kinds).not.toContain('depends_on');
    expect(kinds).not.toContain('dataflow');
  });

  it('precedence: local override beats shared overlay beats default', () => {
    const node = graphNode('dec_X', 'decision');
    const decision = decisionSummary('dec_X');

    const defaultBuilt = callBuild({ nodes: [node], decisions: [decision] });
    const defaultPos = defaultBuilt.nodes.find((n) => n.id === 'dec_X')!.position;

    const sharedBuilt = callBuild({
      nodes: [node],
      decisions: [decision],
      layout: { dec_X: { x: 555, y: 777 } },
    });
    const sharedPos = sharedBuilt.nodes.find((n) => n.id === 'dec_X')!.position;
    expect(sharedPos).toEqual({ x: 555, y: 777 });
    expect(sharedPos).not.toEqual(defaultPos);

    const localBuilt = callBuild({
      nodes: [node],
      decisions: [decision],
      layout: { dec_X: { x: 555, y: 777 } },
      localOverrides: { dec_X: { x: 111, y: 222 } },
    });
    const localPos = localBuilt.nodes.find((n) => n.id === 'dec_X')!.position;
    expect(localPos).toEqual({ x: 111, y: 222 });
  });

  it('owned-wholesale: a partial local override falls through to default for missing fields, not shared', () => {
    const node = graphNode('dec_X', 'decision');
    const decision = decisionSummary('dec_X');
    const defaultPos = callBuild({ nodes: [node], decisions: [decision] })
      .nodes.find((n) => n.id === 'dec_X')!.position;

    // shared supplies both x and y; local supplies only x. "Owned wholesale" =>
    // shared is ignored entirely for this node, so y comes from the default base,
    // NOT from shared's 777.
    const built = callBuild({
      nodes: [node],
      decisions: [decision],
      layout: { dec_X: { x: 555, y: 777 } },
      localOverrides: { dec_X: { x: 111 } },
    });
    const pos = built.nodes.find((n) => n.id === 'dec_X')!.position;
    expect(pos.x).toBe(111);
    expect(pos.y).toBe(defaultPos.y);
    expect(pos.y).not.toBe(777);
  });

  it('is order-stable: reordering the input arrays yields identical per-node positions', () => {
    const ids = ['dec_AAAAA', 'dec_BBBBB', 'dec_CCCCC'];
    const build = (order: string[]) =>
      callBuild({
        nodes: order.map((id) => graphNode(id, 'decision')),
        decisions: order.map((id) => decisionSummary(id)),
      });
    const norm = (r: ReturnType<typeof build>) =>
      r.nodes
        .map((n) => [n.id, n.position.x, n.position.y] as const)
        .sort((a, b) => String(a[0]).localeCompare(String(b[0])));

    expect(norm(build([...ids]))).toEqual(norm(build([...ids].reverse())));
  });
});
