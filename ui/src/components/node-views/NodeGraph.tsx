// @arch arch_MK2Q2.7
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import {
  Background,
  Controls,
  Handle,
  MiniMap,
  Position,
  ReactFlow,
  ReactFlowProvider,
  useEdgesState,
  useNodesState,
  type Edge,
  type Node,
  type NodeProps,
} from 'reactflow';
import 'reactflow/dist/style.css';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import type { ArchitectureGraphNode, ArchitectureNodesResponse, ArchitectureSummary } from '@/lib/types';
import { cn } from '@/lib/utils';

import { firstSentence, shortPath } from './orgNodes';

type Lod = 'low' | 'mid' | 'high';
export type NodeRole =
  | 'subsystem'
  | 'leaf'
  | 'artifact'
  | 'decision'
  | 'task'
  | 'glossary';
export type GraphNodeData = {
  id: string;
  label: string;
  role: NodeRole;
  lod: Lod;
  childCount?: number;
  sourcePaths: string[];
  artifactKind?: string;
  edgeKinds?: string[];
  selected?: boolean;
  status?: 'shipped' | 'planned' | 'drifted' | null;
  superseded?: boolean;
  taskStage?: string | null;
  onOpen?: (id: string) => void;
};
export type FlowNode = Node<GraphNodeData>;
export type FlowEdge = Edge<{ kind: string }>;
type Viewport = { x: number; y: number; zoom: number };
type FlowInstance = {
  setViewport: (viewport: Viewport) => void | Promise<boolean>;
  getViewport: () => Viewport;
  fitView: (options?: { padding?: number }) => boolean | void | Promise<boolean>;
};

const NODE_TYPES = { architecture: ArchitectureGraphNodeCard };
const LEAF_WIDTH = 220;
const LEAF_HEIGHT_MID = 94;
const LEAF_HEIGHT_HIGH = 132;
const ARTIFACT_WIDTH = 190;
const ARTIFACT_HEIGHT = 72;
const CLUSTER_WIDTH = 560;
const CLUSTER_HEADER = 66;
const CLUSTER_GAP = 28;
const GRID_GAP = 18;

const EDGE_STYLES: Record<string, { className: string; style: FlowEdge['style']; animated?: boolean }> = {
  reads: { className: 'text-muted-foreground', style: { strokeDasharray: '6 4', strokeWidth: 1.4 } },
  writes: { className: 'text-foreground', style: { strokeWidth: 2 } },
  exposes_rest: { className: 'text-sky-600', style: { stroke: '#0284c7', strokeWidth: 2.2 } },
  exposes_ws: { className: 'text-emerald-600', style: { stroke: '#059669', strokeWidth: 2.2 } },
  subscribes_to: { className: 'text-emerald-600', style: { stroke: '#059669', strokeDasharray: '3 3', strokeWidth: 1.8 } },
  spawns: { className: 'text-orange-600', style: { stroke: '#ea580c', strokeWidth: 2.2 } },
  calls: { className: 'text-slate-500', style: { stroke: '#64748b', strokeWidth: 1.2 } },
  depends_on: { className: 'text-muted-foreground', style: { strokeDasharray: '2 5', strokeWidth: 1.3 } },
  motivated_by: { className: 'text-indigo-600', style: { stroke: '#4f46e5', strokeWidth: 1.6 } },
  implements: { className: 'text-emerald-700', style: { stroke: '#047857', strokeWidth: 1.6 } },
  produces: { className: 'text-amber-600', style: { stroke: '#d97706', strokeWidth: 1.3, strokeDasharray: '4 3' } },
  dataflow: { className: 'text-fuchsia-600', style: { stroke: '#c026d3', strokeWidth: 1.6 } },
};

function viewportKeyFor(scope: string) {
  return `node-graph-viewport:${scope}`;
}

function isViewport(value: unknown): value is Viewport {
  if (!value || typeof value !== 'object') return false;
  const candidate = value as Partial<Viewport>;
  return (
    typeof candidate.x === 'number' && Number.isFinite(candidate.x) &&
    typeof candidate.y === 'number' && Number.isFinite(candidate.y) &&
    typeof candidate.zoom === 'number' && Number.isFinite(candidate.zoom)
  );
}

function readViewport(scope: string): Viewport | null {
  try {
    const parsed = JSON.parse(window.localStorage.getItem(viewportKeyFor(scope)) ?? 'null') as unknown;
    return isViewport(parsed) ? parsed : null;
  } catch {
    return null;
  }
}

function writeViewport(scope: string, viewport: Viewport) {
  window.localStorage.setItem(viewportKeyFor(scope), JSON.stringify(viewport));
}

function lodForZoom(zoom: number): Lod {
  if (zoom < 0.48) return 'low';
  if (zoom < 0.92) return 'mid';
  return 'high';
}

function nodeLabel(node: ArchitectureGraphNode, summaries: Map<string, ArchitectureSummary>): string {
  return node.label ?? node.name ?? summaries.get(node.id)?.label ?? node.id;
}

function graphRole(node: ArchitectureGraphNode, summaries: Map<string, ArchitectureSummary>): NodeRole {
  const summary = summaries.get(node.id);
  if (node.kind === 'arch' && !summary?.parent_id && !node.parent_id) return 'subsystem';
  if (node.kind === 'arch') return 'leaf';
  return 'artifact';
}

export function edgeStyle(kind: string): Pick<FlowEdge, 'style' | 'animated' | 'className'> {
  const style = EDGE_STYLES[kind] ?? EDGE_STYLES.depends_on;
  return { style: style.style, animated: style.animated, className: style.className };
}

function parentFor(id: string, summaries: Map<string, ArchitectureSummary>, nodes: Map<string, ArchitectureGraphNode>): string | null {
  const summary = summaries.get(id);
  if (summary?.parent_id) return summary.parent_id;
  const node = nodes.get(id);
  if (node?.parent_id) return node.parent_id;
  if (node && node.kind !== 'arch') return 'architecture-artifacts';
  if (id.startsWith('artifact:')) return 'architecture-artifacts';
  return null;
}

function clusterHeight(childCount: number, lod: Lod): number {
  if (lod === 'low') return 120;
  const perRow = 2;
  const rows = Math.max(1, Math.ceil(childCount / perRow));
  const childHeight = lod === 'high' ? LEAF_HEIGHT_HIGH : LEAF_HEIGHT_MID;
  return CLUSTER_HEADER + rows * childHeight + Math.max(0, rows - 1) * GRID_GAP + 30;
}

function layoutClusters(clusters: { id: string; childCount: number }[], lod: Lod): Map<string, { x: number; y: number; h: number }> {
  const out = new Map<string, { x: number; y: number; h: number }>();
  const columns = 2;
  const colHeights = Array.from({ length: columns }, () => 0);
  clusters.forEach((cluster, index) => {
    const col = index % columns;
    const h = clusterHeight(cluster.childCount, lod);
    out.set(cluster.id, { x: col * (CLUSTER_WIDTH + 90), y: colHeights[col], h });
    colHeights[col] += h + CLUSTER_GAP;
  });
  return out;
}

function artifactId(node: ArchitectureGraphNode): string {
  return node.id || `${node.kind}:${node.scheme ?? 'unknown'}:${node.name ?? 'unknown'}`;
}

function lowClusterEdges(edges: ArchitectureNodesResponse['edges'], summaries: Map<string, ArchitectureSummary>, nodeMap: Map<string, ArchitectureGraphNode>): FlowEdge[] {
  const seen = new Set<string>();
  const out: FlowEdge[] = [];
  for (const edge of edges) {
    const sourceParent = parentFor(edge.from, summaries, nodeMap) ?? edge.from;
    const targetParent = parentFor(edge.to, summaries, nodeMap) ?? edge.to;
    if (sourceParent === targetParent) continue;
    const id = `${edge.kind}:${sourceParent}->${targetParent}`;
    if (seen.has(id)) continue;
    seen.add(id);
    out.push({
      id,
      source: sourceParent,
      target: targetParent,
      type: 'smoothstep',
      data: { kind: edge.kind },
      ...edgeStyle(edge.kind),
    });
  }
  return out;
}

function buildGraph({
  model,
  summaries,
  lod,
  selectedId,
}: {
  model: ArchitectureNodesResponse;
  summaries: ArchitectureSummary[];
  lod: Lod;
  selectedId: string | null;
}): { nodes: FlowNode[]; edges: FlowEdge[] } {
  const summaryMap = new Map(summaries.map((item) => [item.id, item]));
  const normalizedNodes = model.nodes.map((node) => ({ ...node, id: artifactId(node) }));
  const nodeMap = new Map(normalizedNodes.map((node) => [node.id, node]));
  const childrenByParent = new Map<string, ArchitectureGraphNode[]>();
  const roots: ArchitectureGraphNode[] = [];
  const artifacts: ArchitectureGraphNode[] = [];

  for (const node of normalizedNodes) {
    const role = graphRole(node, summaryMap);
    if (role === 'subsystem') {
      roots.push(node);
    } else if (role === 'leaf') {
      const parent = parentFor(node.id, summaryMap, nodeMap);
      if (parent) childrenByParent.set(parent, [...(childrenByParent.get(parent) ?? []), node]);
      else roots.push(node);
    } else {
      artifacts.push(node);
    }
  }

  if (artifacts.length > 0) {
    roots.push({ id: 'architecture-artifacts', kind: 'artifact-cluster', label: 'Artifacts', parent_id: null, source_paths: [], tests: [], scheme: null, name: null });
    childrenByParent.set('architecture-artifacts', artifacts);
  }

  const clusters = roots.map((root) => ({ id: root.id, childCount: childrenByParent.get(root.id)?.length ?? 0 }));
  const clusterLayout = layoutClusters(clusters, lod);
  const nodes: FlowNode[] = [];

  for (const root of roots) {
    const children = childrenByParent.get(root.id) ?? [];
    const rect = clusterLayout.get(root.id) ?? { x: 0, y: 0, h: clusterHeight(children.length, lod) };
    nodes.push({
      id: root.id,
      type: 'architecture',
      position: { x: rect.x, y: rect.y },
      style: { width: CLUSTER_WIDTH, height: rect.h },
      data: {
        id: root.id,
        label: nodeLabel(root, summaryMap),
        role: 'subsystem',
        lod,
        childCount: children.length,
        sourcePaths: [],
        selected: selectedId === root.id,
      },
    });

    children.forEach((child, index) => {
      const role = graphRole(child, summaryMap);
      const col = index % 2;
      const row = Math.floor(index / 2);
      const width = role === 'artifact' ? ARTIFACT_WIDTH : LEAF_WIDTH;
      const height = lod === 'high' ? LEAF_HEIGHT_HIGH : role === 'artifact' ? ARTIFACT_HEIGHT : LEAF_HEIGHT_MID;
      const summary = summaryMap.get(child.id);
      nodes.push({
        id: child.id,
        type: 'architecture',
        parentNode: root.id,
        extent: 'parent',
        hidden: lod === 'low',
        position: {
          x: 22 + col * ((CLUSTER_WIDTH - 44) / 2),
          y: CLUSTER_HEADER + row * (height + GRID_GAP),
        },
        style: { width, height },
        data: {
          id: child.id,
          label: nodeLabel(child, summaryMap),
          role,
          lod,
          sourcePaths: child.source_paths?.length ? child.source_paths : summary?.source_paths ?? [],
          artifactKind: role === 'artifact' ? child.kind : undefined,
          selected: selectedId === child.id,
          status: 'shipped',
        },
      });
    });
  }

  const fullEdges = model.edges.map((item) => ({
    id: `${item.kind}:${item.from}->${item.to}`,
    source: item.from,
    target: item.to,
    // Anchor architecture leaf edges Right->Left. The leaf card also exposes
    // unnamed Top/Bottom handles for the cross-layer band layout, so without an
    // explicit handle these edges would default to Top (TASK-JH9DD review F1).
    sourceHandle: 'right',
    targetHandle: 'left',
    label: lod === 'high' ? item.kind : undefined,
    type: 'smoothstep',
    data: { kind: item.kind },
    hidden: lod === 'low',
    labelBgPadding: [6, 3] as [number, number],
    labelBgBorderRadius: 4,
    labelStyle: { fontSize: 10, fontFamily: 'var(--mono)' },
    ...edgeStyle(item.kind),
  }));

  return {
    nodes,
    edges: lod === 'low' ? lowClusterEdges(model.edges, summaryMap, nodeMap) : fullEdges,
  };
}

export function NodeGraphCanvas({
  scope,
  nodes: incomingNodes,
  edges: incomingEdges,
  onSelect,
  legend,
  onZoomChange,
  onNodeDragStop,
  onNodeContextMenu,
  minZoom = 0.22,
  maxZoom = 1.7,
  showMiniMap = true,
}: {
  scope: string;
  nodes: FlowNode[];
  edges: FlowEdge[];
  onSelect: (id: string) => void;
  legend?: ReactNode;
  onZoomChange?: (zoom: number) => void;
  onNodeDragStop?: (id: string, position: { x: number; y: number }) => void;
  onNodeContextMenu?: (id: string) => void;
  minZoom?: number;
  maxZoom?: number;
  showMiniMap?: boolean;
}) {
  const [nodes, setNodes, onNodesChange] = useNodesState(incomingNodes);
  const [edges, setEdges, onEdgesChange] = useEdgesState(incomingEdges);
  const instanceRef = useRef<FlowInstance | null>(null);
  const fittedRef = useRef(false);

  useEffect(() => {
    setNodes(incomingNodes);
    setEdges(incomingEdges);
    fittedRef.current = false;
  }, [incomingEdges, incomingNodes, setEdges, setNodes]);

  const onInit = useCallback((instance: FlowInstance) => {
    instanceRef.current = instance;
    const saved = readViewport(scope);
    if (saved) {
      onZoomChange?.(saved.zoom);
      void instance.setViewport(saved);
    }
  }, [onZoomChange, scope]);

  useEffect(() => {
    const instance = instanceRef.current;
    if (!instance || fittedRef.current || nodes.length === 0) return;
    if (readViewport(scope)) {
      fittedRef.current = true;
      return;
    }
    fittedRef.current = true;
    const timer = window.setTimeout(() => {
      void instance.fitView({ padding: 0.16 });
    }, 0);
    return () => window.clearTimeout(timer);
  }, [nodes.length, scope]);

  const persistViewport = useCallback(() => {
    const instance = instanceRef.current;
    if (!instance) return;
    const viewport = instance.getViewport();
    onZoomChange?.(viewport.zoom);
    writeViewport(scope, viewport);
  }, [onZoomChange, scope]);

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden rounded-lg border bg-background">
      {legend}
      <div className="min-h-[34rem] flex-1 touch-none">
        <ReactFlowProvider>
          <ReactFlow
            nodes={nodes.map((node) => ({ ...node, data: { ...node.data, onOpen: onSelect } }))}
            edges={edges}
            nodeTypes={NODE_TYPES}
            onNodesChange={onNodesChange}
            onEdgesChange={onEdgesChange}
            onInit={onInit}
            onMove={(_, viewport) => onZoomChange?.(viewport.zoom)}
            onMoveEnd={persistViewport}
            onNodeClick={(_, node) => onSelect(node.id)}
            onNodeDragStop={(_, node) => onNodeDragStop?.(node.id, { x: node.position.x, y: node.position.y })}
            onNodeContextMenu={(event, node) => {
              if (!onNodeContextMenu) return;
              event.preventDefault();
              onNodeContextMenu(node.id);
            }}
            fitView
            panOnDrag
            zoomOnPinch
            zoomOnScroll
            zoomOnDoubleClick
            minZoom={minZoom}
            maxZoom={maxZoom}
          >
            <Background gap={18} />
            {showMiniMap ? <MiniMap pannable zoomable /> : null}
            <Controls showInteractive={false}>
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={() => void instanceRef.current?.fitView({ padding: 0.16 })}
              >
                Reset
              </Button>
            </Controls>
          </ReactFlow>
        </ReactFlowProvider>
      </div>
    </div>
  );
}

export function NodeGraph({
  projectId,
  model,
  summaries,
  selectedId,
  onSelect,
}: {
  projectId: string;
  model: ArchitectureNodesResponse;
  summaries: ArchitectureSummary[];
  selectedId: string | null;
  onSelect: (id: string) => void;
}) {
  const [zoom, setZoom] = useState(1);
  const lod = lodForZoom(zoom);
  const graph = useMemo(
    () => buildGraph({ model, summaries, lod, selectedId }),
    [lod, model, selectedId, summaries],
  );
  const legend = (
    <div className="flex flex-wrap items-center justify-between gap-2 border-b px-3 py-2 text-xs text-muted-foreground">
      <div className="flex items-center gap-2">
        <Badge variant="secondary" className="font-mono uppercase">{lod}</Badge>
        <span>{lod === 'low' ? 'Subsystem clusters' : lod === 'mid' ? 'Leaves and typed edges' : 'Source paths and edge labels'}</span>
      </div>
      <div className="hidden flex-wrap gap-1 md:flex">
        {Object.keys(EDGE_STYLES).filter((kind) => !['motivated_by', 'implements', 'produces', 'dataflow'].includes(kind)).map((kind) => (
          <span key={kind} className="rounded border px-1.5 py-0.5 font-mono text-[10px]">{kind}</span>
        ))}
      </div>
    </div>
  );
  return (
    <NodeGraphCanvas
      scope={`architecture:${projectId}`}
      nodes={graph.nodes}
      edges={graph.edges}
      onSelect={onSelect}
      legend={legend}
      onZoomChange={setZoom}
    />
  );
}

function ArchitectureGraphNodeCard({ data }: NodeProps<GraphNodeData>) {
  if (data.role === 'subsystem') {
    return (
      <div
        className={cn(
          'h-full w-full rounded-xl border bg-card/70 p-4 shadow-sm transition-colors',
          data.lod !== 'low' && 'bg-muted/20',
          data.selected && 'ring-2 ring-ring',
        )}
      >
        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0">
            <code className="font-mono text-[11px] text-muted-foreground">{data.id}</code>
            <p className="truncate text-sm font-semibold">{firstSentence(data.label)}</p>
          </div>
          <Badge variant="outline">{data.childCount ?? 0} leaves</Badge>
        </div>
        {data.lod === 'low' ? (
          <p className="mt-3 max-w-[28rem] text-xs text-muted-foreground">Zoom in to inspect mechanism leaves and typed edges.</p>
        ) : null}
        <Handle type="target" position={Position.Left} className="opacity-0" />
        <Handle type="source" position={Position.Right} className="opacity-0" />
      </div>
    );
  }

  if (data.role === 'decision') {
    return (
      <DecisionCard data={data} />
    );
  }
  if (data.role === 'task') {
    return <TaskCard data={data} />;
  }
  if (data.role === 'glossary') {
    return <GlossaryCard data={data} />;
  }

  const planned = data.status === 'planned';
  const drifted = data.status === 'drifted';
  return (
    <div
      role="button"
      tabIndex={0}
      aria-label={`Open details for ${data.id} ${data.label}`}
      onKeyDown={(event) => {
        if (event.key === 'Enter' || event.key === ' ') {
          event.preventDefault();
          data.onOpen?.(data.id);
        }
      }}
      className={cn(
        'h-full w-full rounded-lg border bg-background px-3 py-2 shadow-sm transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring',
        data.role === 'artifact' && 'border-dashed bg-muted/30',
        data.selected && 'ring-2 ring-ring',
        planned && 'border-dashed border-muted-foreground/70 bg-muted/40 text-muted-foreground',
        drifted && 'border-destructive',
      )}
    >
      <Handle type="target" position={Position.Top} className="opacity-0" />
      <Handle type="target" position={Position.Left} className="opacity-0" id="left" />
      <div className="flex items-center justify-between gap-2">
        <code className="truncate font-mono text-[10px] text-muted-foreground">{data.id}</code>
        {data.role === 'artifact' ? <Badge variant="outline" className="h-5 px-1.5 text-[10px]">{data.artifactKind}</Badge> : null}
      </div>
      <p className="mt-1 line-clamp-2 text-xs font-medium leading-snug">{firstSentence(data.label)}</p>
      {data.lod === 'high' && data.sourcePaths.length > 0 ? (
        <div className="mt-2 flex flex-col gap-0.5 text-[10px] text-muted-foreground">
          {data.sourcePaths.slice(0, 3).map((path) => <span key={path} className="truncate font-mono">{shortPath(path)}</span>)}
          {data.sourcePaths.length > 3 ? <span>+{data.sourcePaths.length - 3} more</span> : null}
        </div>
      ) : null}
      <Handle type="source" position={Position.Bottom} className="opacity-0" />
      <Handle type="source" position={Position.Right} className="opacity-0" id="right" />
    </div>
  );
}

function DecisionCard({ data }: { data: GraphNodeData }) {
  return (
    <div
      role="button"
      tabIndex={0}
      aria-label={`Open details for ${data.id} ${data.label}`}
      onKeyDown={(event) => {
        if (event.key === 'Enter' || event.key === ' ') {
          event.preventDefault();
          data.onOpen?.(data.id);
        }
      }}
      className={cn(
        'h-full w-full rounded-lg border bg-background px-3 py-2 shadow-sm transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring',
        data.superseded && 'border-muted bg-muted/40 text-muted-foreground',
        data.selected && 'ring-2 ring-ring',
      )}
    >
      <Handle type="target" position={Position.Top} className="opacity-0" />
      <Handle type="target" position={Position.Left} className="opacity-0" id="left" />
      <div className="flex items-center justify-between gap-2">
        <code className={cn('truncate font-mono text-[10px]', data.superseded ? 'text-muted-foreground/80' : 'text-muted-foreground')}>{data.id}</code>
        <Badge variant={data.superseded ? 'outline' : 'secondary'} className="h-5 px-1.5 text-[10px]">{data.superseded ? 'superseded' : 'decision'}</Badge>
      </div>
      <p className={cn('mt-1 line-clamp-2 text-xs font-medium leading-snug', data.superseded && 'line-through opacity-80')}>{firstSentence(data.label)}</p>
      <Handle type="source" position={Position.Bottom} className="opacity-0" />
      <Handle type="source" position={Position.Right} className="opacity-0" id="right" />
    </div>
  );
}

function TaskCard({ data }: { data: GraphNodeData }) {
  return (
    <div
      role="button"
      tabIndex={0}
      aria-label={`Open task ${data.id} ${data.label}`}
      onKeyDown={(event) => {
        if (event.key === 'Enter' || event.key === ' ') {
          event.preventDefault();
          data.onOpen?.(data.id);
        }
      }}
      className={cn(
        'h-full w-full rounded-lg border border-sky-200 bg-sky-50 px-3 py-2 text-sky-950 shadow-sm transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring dark:border-sky-900/60 dark:bg-sky-950/40 dark:text-sky-100',
        data.selected && 'ring-2 ring-ring',
      )}
    >
      <Handle type="target" position={Position.Top} className="opacity-0" />
      <Handle type="target" position={Position.Left} className="opacity-0" id="left" />
      <div className="flex items-center justify-between gap-2">
        <code className="truncate font-mono text-[10px] text-sky-900/70 dark:text-sky-200/70">{data.id}</code>
        {data.taskStage ? <Badge variant="outline" className="h-5 px-1.5 text-[10px]">{data.taskStage}</Badge> : null}
      </div>
      <p className="mt-1 line-clamp-2 text-xs font-medium leading-snug">{firstSentence(data.label)}</p>
      <Handle type="source" position={Position.Bottom} className="opacity-0" />
      <Handle type="source" position={Position.Right} className="opacity-0" id="right" />
    </div>
  );
}

function GlossaryCard({ data }: { data: GraphNodeData }) {
  return (
    <div
      role="button"
      tabIndex={0}
      aria-label={`Open glossary term ${data.id} ${data.label}`}
      onKeyDown={(event) => {
        if (event.key === 'Enter' || event.key === ' ') {
          event.preventDefault();
          data.onOpen?.(data.id);
        }
      }}
      className={cn(
        'h-full w-full rounded-md border border-purple-200 bg-purple-50/60 px-2 py-1.5 text-purple-950 shadow-sm transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring dark:border-purple-900/60 dark:bg-purple-950/30 dark:text-purple-100',
        data.selected && 'ring-2 ring-ring',
      )}
    >
      <Handle type="target" position={Position.Top} className="opacity-0" />
      <Handle type="target" position={Position.Left} className="opacity-0" id="left" />
      <code className="block truncate font-mono text-[10px] text-purple-900/70 dark:text-purple-200/70">{data.id}</code>
      <p className="mt-0.5 line-clamp-2 text-[11px] font-medium leading-snug">{firstSentence(data.label)}</p>
      <Handle type="source" position={Position.Bottom} className="opacity-0" />
      <Handle type="source" position={Position.Right} className="opacity-0" id="right" />
    </div>
  );
}
