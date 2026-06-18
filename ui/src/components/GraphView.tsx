// @arch arch_M8JQT
import { lazy, Suspense, useCallback, useEffect, useMemo, useState } from 'react';
import { useNavigate } from '@tanstack/react-router';
import { toast } from 'sonner';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import {
  fetchArchitecture,
  fetchDecisions,
  fetchGlossary,
  fetchGraphEdges,
  fetchGraphLayout,
  fetchGraphNodes,
  fetchProjectTasks,
  patchGraphLayout,
} from '@/lib/api';
import { appendDrawerStack, routeSearch } from '@/lib/searchState';
import type {
  ArchitectureSummary,
  DecisionSummary,
  GlossarySummary,
  GraphEdgeSummary,
  GraphLayoutEntry,
  GraphNodeSummary,
  LifecycleStage,
  TaskSummary,
} from '@/lib/types';
import { useResource } from '@/lib/useResource';
import { cn } from '@/lib/utils';

import { ErrorPanel, PageHeader } from './Primitives';
import { NodeModal } from './node-views/NodeModal';
import type { FlowEdge, FlowNode, GraphNodeData } from './node-views/NodeGraph';
import { edgeStyle } from './node-views/NodeGraph';

const NodeGraphCanvas = lazy(() =>
  import('./node-views/NodeGraph').then((module) => ({ default: module.NodeGraphCanvas })),
);

const ACTIVE_TASK_STAGES = new Set<LifecycleStage>([
  'backlog',
  'todo',
  'in_progress',
  'in_review',
]);

// Layout constants. All positions are deterministic functions of (sorted)
// inputs — no Math.random / Date.now anywhere on the rendering path.
const DECISION_CARD = { w: 220, h: 76 };
const TASK_CARD = { w: 220, h: 76 };
const GLOSSARY_CARD = { w: 200, h: 64 };
const LEAF_CARD = { w: 220, h: 94 };
const SUBSYSTEM_CARD = { w: 560, headerH: 66, gridGap: 18, bottomPad: 30 };
const BAND_GAP = 64;
const COL_GAP = 24;
const ROW_GAP = 16;
const DECISIONS_PER_ROW = 6;
const TASKS_PER_ROW = 6;
const SUBSYSTEMS_PER_ROW = 3;
const GLOSSARY_GUTTER = 60;

type Curation = {
  glossary: boolean;
  artifacts: boolean;
  backbone: boolean;
  dependsOn: boolean;
  dataflow: boolean;
};

type LocalOverrideStore = Record<string, GraphLayoutEntry>;
type BaselineStore = Record<string, GraphLayoutEntry | null>;

function localOverridesKey(projectId: string): string {
  return `graph-layout-local:${projectId}`;
}

function baselineKey(projectId: string): string {
  return `graph-layout-baseline:${projectId}`;
}

function isLayoutEntry(value: unknown): value is GraphLayoutEntry {
  if (!value || typeof value !== 'object') return false;
  const candidate = value as Record<string, unknown>;
  for (const k of ['x', 'y', 'w', 'h']) {
    if (k in candidate && candidate[k] !== undefined && typeof candidate[k] !== 'number') return false;
  }
  for (const k of ['hidden', 'pinned']) {
    if (k in candidate && candidate[k] !== undefined && typeof candidate[k] !== 'boolean') return false;
  }
  return true;
}

function readLocalOverrides(projectId: string): LocalOverrideStore {
  try {
    const raw = window.localStorage.getItem(localOverridesKey(projectId));
    if (!raw) return {};
    const parsed = JSON.parse(raw) as unknown;
    if (!parsed || typeof parsed !== 'object') return {};
    const out: LocalOverrideStore = {};
    for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
      if (isLayoutEntry(v)) out[k] = v;
    }
    return out;
  } catch {
    return {};
  }
}

function writeLocalOverrides(projectId: string, store: LocalOverrideStore): void {
  try {
    window.localStorage.setItem(localOverridesKey(projectId), JSON.stringify(store));
  } catch {
    /* quota or disabled — drop quietly */
  }
}

function readBaselines(projectId: string): BaselineStore {
  try {
    const raw = window.localStorage.getItem(baselineKey(projectId));
    if (!raw) return {};
    const parsed = JSON.parse(raw) as unknown;
    if (!parsed || typeof parsed !== 'object') return {};
    const out: BaselineStore = {};
    for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
      if (v === null) out[k] = null;
      else if (isLayoutEntry(v)) out[k] = v;
    }
    return out;
  } catch {
    return {};
  }
}

function writeBaselines(projectId: string, store: BaselineStore): void {
  try {
    window.localStorage.setItem(baselineKey(projectId), JSON.stringify(store));
  } catch {
    /* quota or disabled — drop quietly */
  }
}

function layoutEntryEqual(a: GraphLayoutEntry | null | undefined, b: GraphLayoutEntry | null | undefined): boolean {
  const an = a ?? null;
  const bn = b ?? null;
  if (an === null && bn === null) return true;
  if (an === null || bn === null) return false;
  return an.x === bn.x && an.y === bn.y && an.w === bn.w && an.h === bn.h
    && (an.hidden ?? undefined) === (bn.hidden ?? undefined)
    && (an.pinned ?? undefined) === (bn.pinned ?? undefined);
}

function subsystemHeight(leafCount: number): number {
  const rows = Math.max(1, Math.ceil(leafCount / 2));
  return (
    SUBSYSTEM_CARD.headerH +
    rows * LEAF_CARD.h +
    Math.max(0, rows - 1) * SUBSYSTEM_CARD.gridGap +
    SUBSYSTEM_CARD.bottomPad
  );
}

function chunkRows<T>(items: T[], perRow: number): T[][] {
  const rows: T[][] = [];
  for (let i = 0; i < items.length; i += perRow) rows.push(items.slice(i, i + perRow));
  return rows;
}

function isTaskId(id: string): boolean {
  return /^TASK-[0-9A-HJKMNP-TV-Z]{5}(?:\.\d+)*$/.test(id);
}

function isArchId(id: string): boolean {
  return id.startsWith('arch_');
}

function isDecisionId(id: string): boolean {
  return id.startsWith('dec_');
}

function isGlossaryId(id: string): boolean {
  return id.startsWith('term_') || id.startsWith('term:');
}

// Three-tier per-node precedence: local override > shared overlay > deterministic base.
// "Owned wholesale" — if local has the node, the shared overlay for that node is
// ignored entirely; missing fields fall through to the deterministic base.
function applyOverlay(
  base: { x: number; y: number },
  shared: GraphLayoutEntry | undefined,
  local: GraphLayoutEntry | undefined,
): { x: number; y: number } {
  const chosen = local ?? shared;
  if (!chosen) return base;
  return {
    x: typeof chosen.x === 'number' ? chosen.x : base.x,
    y: typeof chosen.y === 'number' ? chosen.y : base.y,
  };
}

export function buildFlowGraph({
  nodes,
  edges,
  layout,
  localOverrides,
  tasks,
  decisions,
  architecture,
  glossary,
  curation,
  selectedId,
}: {
  nodes: GraphNodeSummary[];
  edges: GraphEdgeSummary[];
  layout: Record<string, GraphLayoutEntry>;
  localOverrides: Record<string, GraphLayoutEntry>;
  tasks: TaskSummary[];
  decisions: DecisionSummary[];
  architecture: ArchitectureSummary[];
  glossary: GlossarySummary[];
  curation: Curation;
  selectedId: string | null;
}): { nodes: FlowNode[]; edges: FlowEdge[] } {
  const decisionTitleById = new Map(decisions.map((d) => [d.id, d.title]));
  const archLabelById = new Map(architecture.map((a) => [a.id, a.label]));
  const archParentById = new Map(architecture.map((a) => [a.id, a.parent_id ?? null]));
  const glossaryLabelById = new Map(glossary.map((g) => [g.id, g.canonical ?? g.id]));
  const taskById = new Map(tasks.map((t) => [t.id, t]));

  const supersededDecisions = nodes
    .filter((n) => n.layer === 'decision' && n.superseded)
    .sort((a, b) => a.id.localeCompare(b.id));
  const currentDecisions = nodes
    .filter((n) => n.layer === 'decision' && !n.superseded)
    .sort((a, b) => a.id.localeCompare(b.id));
  const archNodes = nodes
    .filter((n) => n.layer === 'architecture')
    .sort((a, b) => a.id.localeCompare(b.id));
  const activeTaskNodes = nodes
    .filter((n) => {
      if (n.layer !== 'task') return false;
      const task = taskById.get(n.id);
      const stage = task?.lifecycle_stage as LifecycleStage | undefined;
      return Boolean(stage) && ACTIVE_TASK_STAGES.has(stage as LifecycleStage);
    })
    .sort((a, b) => a.id.localeCompare(b.id));
  const glossaryNodes = nodes
    .filter((n) => n.layer === 'glossary')
    .sort((a, b) => a.id.localeCompare(b.id));
  const artifactNodes = nodes
    .filter((n) => n.layer === 'artifact')
    .sort((a, b) => a.id.localeCompare(b.id));

  const subsystems = archNodes.filter((n) => !archParentById.get(n.id));
  const leavesByParent = new Map<string, GraphNodeSummary[]>();
  for (const node of archNodes) {
    const parent = archParentById.get(node.id);
    if (parent) {
      const arr = leavesByParent.get(parent) ?? [];
      arr.push(node);
      leavesByParent.set(parent, arr);
    }
  }

  const supRows = chunkRows(supersededDecisions, DECISIONS_PER_ROW);
  const curRows = chunkRows(currentDecisions, DECISIONS_PER_ROW);
  const subsysRows = chunkRows(subsystems, SUBSYSTEMS_PER_ROW);
  const taskRows = chunkRows(activeTaskNodes, TASKS_PER_ROW);

  const supBandY = 0;
  const supBandH = supRows.length === 0
    ? 0
    : supRows.length * DECISION_CARD.h + Math.max(0, supRows.length - 1) * ROW_GAP;

  const curBandY = supBandY + (supBandH > 0 ? supBandH + BAND_GAP : 0);
  const curBandH = curRows.length === 0
    ? 0
    : curRows.length * DECISION_CARD.h + Math.max(0, curRows.length - 1) * ROW_GAP;

  const subsysBandY = curBandY + (curBandH > 0 ? curBandH + BAND_GAP : 0);
  const subsysRowHeights = subsysRows.map((row) =>
    row.reduce((max, sub) => Math.max(max, subsystemHeight(leavesByParent.get(sub.id)?.length ?? 0)), 0),
  );
  const subsysBandH = subsysRowHeights.reduce((sum, h, i) => sum + h + (i > 0 ? BAND_GAP / 2 : 0), 0);

  const taskBandY = subsysBandY + (subsysBandH > 0 ? subsysBandH + BAND_GAP : 0);
  const taskBandH = taskRows.length === 0
    ? 0
    : taskRows.length * TASK_CARD.h + Math.max(0, taskRows.length - 1) * ROW_GAP;

  const flowNodes: FlowNode[] = [];
  const visibleIds = new Set<string>();

  const decisionData = (
    node: GraphNodeSummary,
    superseded: boolean,
  ): GraphNodeData => ({
    id: node.id,
    label: decisionTitleById.get(node.id) ?? node.id,
    role: 'decision',
    lod: 'mid',
    sourcePaths: [],
    selected: selectedId === node.id,
    superseded,
  });

  supRows.forEach((row, rowIdx) => {
    const rowY = supBandY + rowIdx * (DECISION_CARD.h + ROW_GAP);
    row.forEach((node, col) => {
      const base = { x: col * (DECISION_CARD.w + COL_GAP), y: rowY };
      const pos = applyOverlay(base, layout[node.id], localOverrides[node.id]);
      flowNodes.push({
        id: node.id,
        type: 'architecture',
        position: pos,
        style: { width: DECISION_CARD.w, height: DECISION_CARD.h },
        data: decisionData(node, true),
      });
      visibleIds.add(node.id);
    });
  });

  curRows.forEach((row, rowIdx) => {
    const rowY = curBandY + rowIdx * (DECISION_CARD.h + ROW_GAP);
    row.forEach((node, col) => {
      const base = { x: col * (DECISION_CARD.w + COL_GAP), y: rowY };
      const pos = applyOverlay(base, layout[node.id], localOverrides[node.id]);
      flowNodes.push({
        id: node.id,
        type: 'architecture',
        position: pos,
        style: { width: DECISION_CARD.w, height: DECISION_CARD.h },
        data: decisionData(node, false),
      });
      visibleIds.add(node.id);
    });
  });

  let subsysCursorY = subsysBandY;
  subsysRows.forEach((row, rowIdx) => {
    const rowH = subsysRowHeights[rowIdx] ?? 0;
    row.forEach((sub, col) => {
      const leaves = leavesByParent.get(sub.id) ?? [];
      const h = subsystemHeight(leaves.length);
      const base = {
        x: col * (SUBSYSTEM_CARD.w + COL_GAP * 2),
        y: subsysCursorY,
      };
      const pos = applyOverlay(base, layout[sub.id], localOverrides[sub.id]);
      flowNodes.push({
        id: sub.id,
        type: 'architecture',
        position: pos,
        style: { width: SUBSYSTEM_CARD.w, height: h },
        data: {
          id: sub.id,
          label: archLabelById.get(sub.id) ?? sub.id,
          role: 'subsystem',
          lod: 'mid',
          childCount: leaves.length,
          sourcePaths: [],
          selected: selectedId === sub.id,
        },
      });
      visibleIds.add(sub.id);

      leaves.forEach((leaf, leafIdx) => {
        const lcol = leafIdx % 2;
        const lrow = Math.floor(leafIdx / 2);
        flowNodes.push({
          id: leaf.id,
          type: 'architecture',
          parentNode: sub.id,
          extent: 'parent',
          position: {
            x: 22 + lcol * ((SUBSYSTEM_CARD.w - 44) / 2),
            y: SUBSYSTEM_CARD.headerH + lrow * (LEAF_CARD.h + SUBSYSTEM_CARD.gridGap),
          },
          style: { width: LEAF_CARD.w, height: LEAF_CARD.h },
          data: {
            id: leaf.id,
            label: archLabelById.get(leaf.id) ?? leaf.id,
            role: 'leaf',
            lod: 'mid',
            sourcePaths: [],
            selected: selectedId === leaf.id,
            status: 'shipped',
          },
        });
        visibleIds.add(leaf.id);
      });
    });
    subsysCursorY += rowH + BAND_GAP / 2;
  });

  taskRows.forEach((row, rowIdx) => {
    const rowY = taskBandY + rowIdx * (TASK_CARD.h + ROW_GAP);
    row.forEach((node, col) => {
      const task = taskById.get(node.id);
      const base = { x: col * (TASK_CARD.w + COL_GAP), y: rowY };
      const pos = applyOverlay(base, layout[node.id], localOverrides[node.id]);
      flowNodes.push({
        id: node.id,
        type: 'architecture',
        position: pos,
        style: { width: TASK_CARD.w, height: TASK_CARD.h },
        data: {
          id: node.id,
          label: task?.title ?? node.id,
          role: 'task',
          lod: 'mid',
          sourcePaths: [],
          selected: selectedId === node.id,
          taskStage: task?.lifecycle_stage ?? null,
        },
      });
      visibleIds.add(node.id);
    });
  });

  if (curation.glossary) {
    glossaryNodes.forEach((node, idx) => {
      const base = {
        x: -(GLOSSARY_GUTTER + GLOSSARY_CARD.w),
        y: idx * (GLOSSARY_CARD.h + ROW_GAP),
      };
      const pos = applyOverlay(base, layout[node.id], localOverrides[node.id]);
      flowNodes.push({
        id: node.id,
        type: 'architecture',
        position: pos,
        style: { width: GLOSSARY_CARD.w, height: GLOSSARY_CARD.h },
        data: {
          id: node.id,
          label: glossaryLabelById.get(node.id) ?? node.id,
          role: 'glossary',
          lod: 'mid',
          sourcePaths: [],
          selected: selectedId === node.id,
        },
      });
      visibleIds.add(node.id);
    });
  }

  if (curation.artifacts) {
    const artifactBandY = taskBandY + (taskBandH > 0 ? taskBandH + BAND_GAP : 0);
    const artifactRows = chunkRows(artifactNodes, TASKS_PER_ROW);
    artifactRows.forEach((row, rowIdx) => {
      const rowY = artifactBandY + rowIdx * (TASK_CARD.h + ROW_GAP);
      row.forEach((node, col) => {
        const base = { x: col * (TASK_CARD.w + COL_GAP), y: rowY };
        const pos = applyOverlay(base, layout[node.id], localOverrides[node.id]);
        flowNodes.push({
          id: node.id,
          type: 'architecture',
          position: pos,
          style: { width: TASK_CARD.w, height: TASK_CARD.h },
          data: {
            id: node.id,
            label: node.id,
            role: 'artifact',
            lod: 'mid',
            sourcePaths: [],
            selected: selectedId === node.id,
            artifactKind: 'artifact',
          },
        });
        visibleIds.add(node.id);
      });
    });
  }

  const backboneKinds = new Set(['motivated_by', 'implements']);
  const dependsKinds = new Set(['depends_on']);
  const dataflowKinds = new Set(['dataflow']);

  const flowEdges: FlowEdge[] = [];
  for (const edge of edges) {
    if (!visibleIds.has(edge.from) || !visibleIds.has(edge.to)) continue;
    if (backboneKinds.has(edge.kind)) {
      if (!curation.backbone) continue;
    } else if (dependsKinds.has(edge.kind)) {
      if (!curation.dependsOn) continue;
    } else if (dataflowKinds.has(edge.kind)) {
      if (!curation.dataflow) continue;
    } else {
      // Remaining kinds (artifact produces/reads/writes, plus arch-to-arch
      // calls/spawns/exposes_*/subscribes_to) ride the Artifacts toggle; not all
      // of these terminate on an artifact node.
      if (!curation.artifacts) continue;
    }
    flowEdges.push({
      id: `${edge.kind}:${edge.from}->${edge.to}`,
      source: edge.from,
      target: edge.to,
      type: 'smoothstep',
      data: { kind: edge.kind },
      ...edgeStyle(edge.kind),
    });
  }

  return { nodes: flowNodes, edges: flowEdges };
}

function ToggleButton({
  active,
  onToggle,
  label,
  hint,
}: {
  active: boolean;
  onToggle: () => void;
  label: string;
  hint?: string;
}) {
  return (
    <Button
      type="button"
      variant={active ? 'secondary' : 'outline'}
      size="sm"
      aria-pressed={active}
      onClick={onToggle}
      title={hint}
      className={cn('h-7 px-2 text-xs', !active && 'text-muted-foreground')}
    >
      {label}
    </Button>
  );
}

export function GraphView({ projectId }: { projectId: string }) {
  const navigate = useNavigate();
  const refresh = useRefreshToken();
  const graphNodes = useResource(`graph-nodes:${projectId}:${refresh}`, () => fetchGraphNodes(projectId));
  const graphEdges = useResource(`graph-edges:${projectId}:${refresh}`, () => fetchGraphEdges(projectId));
  const graphLayout = useResource(`graph-layout:${projectId}:${refresh}`, () => fetchGraphLayout(projectId));
  const tasks = useResource(`graph-tasks:${projectId}:${refresh}`, () => fetchProjectTasks(projectId));
  const decisions = useResource(`graph-decisions:${projectId}:${refresh}`, () => fetchDecisions(projectId));
  const architecture = useResource(`graph-architecture:${projectId}:${refresh}`, () => fetchArchitecture(projectId));
  const glossary = useResource(`graph-glossary:${projectId}:${refresh}`, () => fetchGlossary(projectId));

  const [curation, setCuration] = useState<Curation>({
    glossary: false,
    artifacts: false,
    backbone: true,
    dependsOn: false,
    dataflow: false,
  });
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [localOverrides, setLocalOverrides] = useState<LocalOverrideStore>(() => readLocalOverrides(projectId));
  const [baselines, setBaselines] = useState<BaselineStore>(() => readBaselines(projectId));
  const [publishing, setPublishing] = useState(false);

  // Re-read machine-local stores when project switches.
  useEffect(() => {
    setLocalOverrides(readLocalOverrides(projectId));
    setBaselines(readBaselines(projectId));
  }, [projectId]);

  const sharedLayout = graphLayout.data ?? {};

  const built = useMemo(() => {
    if (!graphNodes.data || !graphEdges.data || !tasks.data) {
      return null;
    }
    return buildFlowGraph({
      nodes: graphNodes.data,
      edges: graphEdges.data,
      layout: sharedLayout,
      localOverrides,
      tasks: tasks.data,
      decisions: decisions.data ?? [],
      architecture: architecture.data ?? [],
      glossary: glossary.data ?? [],
      curation,
      selectedId,
    });
  }, [architecture.data, curation, decisions.data, glossary.data, graphEdges.data, sharedLayout, graphNodes.data, localOverrides, selectedId, tasks.data]);

  // Quiet "shared map updated" nudge: a node has a local override AND the shared
  // overlay for that node differs from the baseline snapshotted when the override
  // was made. Empty when no drift.
  const driftedNodeIds = useMemo(() => {
    const ids: string[] = [];
    for (const id of Object.keys(localOverrides)) {
      const current = sharedLayout[id];
      const baseline = baselines[id] ?? null;
      if (!layoutEntryEqual(current ?? null, baseline)) ids.push(id);
    }
    return ids;
  }, [localOverrides, sharedLayout, baselines]);

  const overrideCount = Object.keys(localOverrides).length;

  const handleNodeDragStop = useCallback((id: string, position: { x: number; y: number }) => {
    // Round to integers: GraphLayoutEntry x/y are i64 on the daemon, so a
    // fractional React Flow drag coord would fail to publish (CKRYB review HIGH).
    const nextX = Math.round(position.x);
    const nextY = Math.round(position.y);
    // A click (or a drag that lands back where it started) fires onNodeDragStop
    // with the node's unchanged position. Recording that manufactures a phantom
    // overlay entry — e.g. the lone superseded decision sits at its deterministic
    // base (0,0), so a stray click pinned it to (0,0): a no-op override the user
    // never intended, publishable as drift. Only record an actual move.
    const current = built?.nodes.find((n) => n.id === id)?.position;
    if (current && Math.round(current.x) === nextX && Math.round(current.y) === nextY) {
      return;
    }
    setLocalOverrides((prev) => {
      const next: LocalOverrideStore = {
        ...prev,
        [id]: { ...(prev[id] ?? {}), x: nextX, y: nextY },
      };
      writeLocalOverrides(projectId, next);
      return next;
    });
    setBaselines((prev) => {
      if (id in prev) return prev;
      const next: BaselineStore = { ...prev, [id]: sharedLayout[id] ?? null };
      writeBaselines(projectId, next);
      return next;
    });
  }, [projectId, sharedLayout, built]);

  const handleNodeContextMenu = useCallback((id: string) => {
    setLocalOverrides((prev) => {
      if (!(id in prev)) return prev;
      const next: LocalOverrideStore = { ...prev };
      delete next[id];
      writeLocalOverrides(projectId, next);
      toast.success(`Snapped ${id} to shared map`);
      return next;
    });
    setBaselines((prev) => {
      if (!(id in prev)) return prev;
      const next: BaselineStore = { ...prev };
      delete next[id];
      writeBaselines(projectId, next);
      return next;
    });
  }, [projectId]);

  const handleResetAll = useCallback(() => {
    if (overrideCount === 0) return;
    setLocalOverrides({});
    writeLocalOverrides(projectId, {});
    setBaselines({});
    writeBaselines(projectId, {});
    toast.success(`Reset ${overrideCount} local override${overrideCount === 1 ? '' : 's'} to shared map`);
  }, [overrideCount, projectId]);

  const handlePublish = useCallback(async () => {
    if (overrideCount === 0 || publishing) return;
    setPublishing(true);
    const ids = Object.keys(localOverrides);
    let ok = 0;
    let failed = 0;
    const published: BaselineStore = {};
    for (const id of ids) {
      try {
        const entry = localOverrides[id];
        await patchGraphLayout(projectId, { node_id: id, ...entry });
        // The published entry is now the shared value for this node; advance the
        // baseline so our own publish does not later read as external drift
        // (CKRYB review MEDIUM). The local override stays — local-first.
        published[id] = entry;
        ok += 1;
      } catch {
        failed += 1;
      }
    }
    if (ok > 0) {
      setBaselines((prev) => {
        const next: BaselineStore = { ...prev, ...published };
        writeBaselines(projectId, next);
        return next;
      });
    }
    setPublishing(false);
    if (failed === 0) {
      toast.success(`Published ${ok} layout entr${ok === 1 ? 'y' : 'ies'} to shared map`);
    } else {
      toast.error(`Published ${ok}/${ids.length} — ${failed} failed`);
    }
  }, [localOverrides, overrideCount, projectId, publishing]);

  // /graph/layout is a pure positional overlay; a fetch error (e.g. a daemon that
  // predates the endpoint) must NOT blank the page — it falls back to {} above so
  // the deterministic band layout still renders. Only the topology/task sources are fatal.
  const firstError = graphNodes.error ?? graphEdges.error ?? tasks.error;
  if (firstError) return <ErrorPanel error={firstError} />;

  const visibleNodeCount = built?.nodes.length ?? 0;

  function handleSelect(id: string) {
    setSelectedId(id);
    if (isDecisionId(id) || isArchId(id) || isGlossaryId(id)) {
      void navigate({ search: routeSearch((prev) => appendDrawerStack(prev, id)) });
      return;
    }
    if (isTaskId(id)) {
      void navigate({
        to: '/projects/$projectId/tasks',
        params: { projectId },
        search: routeSearch((prev) => ({ ...prev, task: id })),
      });
      return;
    }
    // Artifacts have no detail surface yet — selection-only.
  }

  const legend = (
    <div className="flex flex-wrap items-center gap-3 border-b px-3 py-2 text-xs">
      <div className="flex items-center gap-1.5">
        <span className="font-medium text-muted-foreground">Layers</span>
        <ToggleButton
          active={curation.glossary}
          onToggle={() => setCuration((prev) => ({ ...prev, glossary: !prev.glossary }))}
          label="Glossary"
          hint="Show the glossary column on the left."
        />
        <ToggleButton
          active={curation.artifacts}
          onToggle={() => setCuration((prev) => ({ ...prev, artifacts: !prev.artifacts }))}
          label="Artifacts & extra edges"
          hint="Show artifact nodes plus the non-backbone, non-dataflow typed edges (produces, calls, spawns, etc.)."
        />
      </div>
      <div className="flex items-center gap-1.5">
        <span className="font-medium text-muted-foreground">Edges</span>
        <ToggleButton
          active={curation.backbone}
          onToggle={() => setCuration((prev) => ({ ...prev, backbone: !prev.backbone }))}
          label="Backbone"
          hint="motivated_by + implements (top-down)."
        />
        <ToggleButton
          active={curation.dependsOn}
          onToggle={() => setCuration((prev) => ({ ...prev, dependsOn: !prev.dependsOn }))}
          label="depends_on"
          hint="Dense layer; off by default."
        />
        <ToggleButton
          active={curation.dataflow}
          onToggle={() => setCuration((prev) => ({ ...prev, dataflow: !prev.dataflow }))}
          label="dataflow"
          hint="Architecture↔architecture dataflow."
        />
      </div>
      <div className="ml-auto flex items-center gap-1.5">
        {driftedNodeIds.length > 0 ? (
          <span
            className="text-[11px] text-muted-foreground"
            title={`Shared map changed under ${driftedNodeIds.length} locally-overridden node${driftedNodeIds.length === 1 ? '' : 's'}: ${driftedNodeIds.join(', ')}`}
          >
            shared map updated · {driftedNodeIds.length}
          </span>
        ) : null}
        {overrideCount > 0 ? (
          <>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="h-7 px-2 text-xs"
              onClick={handleResetAll}
              title="Discard all local layout overrides for this project and snap every node to the shared map."
            >
              Reset to shared
            </Button>
            <Button
              type="button"
              variant="secondary"
              size="sm"
              className="h-7 px-2 text-xs"
              onClick={() => void handlePublish()}
              disabled={publishing}
              title="PATCH the shared graph_layout.org overlay with your local positions for the curator/team."
            >
              {publishing ? 'Publishing…' : `Publish (${overrideCount})`}
            </Button>
          </>
        ) : null}
        <Badge variant="outline" className="font-mono">{visibleNodeCount} nodes</Badge>
      </div>
    </div>
  );

  function graphContent() {
    if (!built) {
      return <p className="rounded-lg border p-4 text-sm text-muted-foreground">Loading cross-layer graph…</p>;
    }
    return (
      <Suspense fallback={<p className="text-sm text-muted-foreground">Loading graph renderer…</p>}>
        <NodeGraphCanvas
          scope={`graph:${projectId}`}
          nodes={built.nodes}
          edges={built.edges}
          onSelect={handleSelect}
          onNodeDragStop={handleNodeDragStop}
          onNodeContextMenu={handleNodeContextMenu}
          legend={legend}
        />
      </Suspense>
    );
  }

  return (
    <div className="flex min-h-0 flex-col gap-4">
      <PageHeader
        title="Graph"
        count={visibleNodeCount}
        description={`Cross-layer graph for ${projectId} — decisions, architecture, active tasks; glossary and artifacts via toggles.`}
      />
      <div className="grid min-h-[calc(100vh-12rem)]">{graphContent()}</div>
      <NodeModal
        projectId={projectId}
        nodeKind="decision"
        seed={{
          decisions: decisions.data ?? null,
          architecture: architecture.data ?? null,
          glossary: glossary.data ?? null,
        }}
      />
    </div>
  );
}
