// @arch arch_MK2Q2.7
import { useEffect, useMemo, useState, type KeyboardEvent } from 'react';
import { useNavigate, useSearch } from '@tanstack/react-router';
import { ChevronDown, ChevronRight, Plus, Sparkles } from 'lucide-react';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { useRefreshBump, useRefreshToken } from '@/hooks/useRefreshBus';
import { createDecision, fetchDecisions } from '@/lib/api';
import { appendDrawerStack, routeSearch, searchList, type AppSearch } from '@/lib/searchState';
import type { DecisionSummary } from '@/lib/types';
import { useResource } from '@/lib/useResource';
import { cn } from '@/lib/utils';

import { CopyIdBadge } from './CopyIdBadge';
import { GenerateArtifactDialog } from './GenerateArtifactDialog';
import { ErrorPanel, PageHeader } from './Primitives';
import { NodeModal } from './node-views/NodeModal';
import { TagFilterInput } from './node-views/TagFilterInput';
import { firstSentence } from './node-views/orgNodes';

const DECISIONS_LIST_ID = 'decisions-list-region';

type DecisionsSearch = AppSearch & {
  tag?: string[];
};

function tagOptions(items: DecisionSummary[]): string[] {
  return Array.from(new Set(items.flatMap((item) => item.tags ?? []))).sort();
}

type DecisionTreeRow = {
  decision: DecisionSummary;
  depth: number;
  context: boolean;
  ghost: boolean;
};

function pathKey(decision: DecisionSummary): number[] {
  return (decision.path ?? '')
    .split('.')
    .map((part) => Number.parseInt(part, 10))
    .filter((part) => Number.isFinite(part));
}

function compareDecisionPath(a: DecisionSummary, b: DecisionSummary): number {
  const aa = pathKey(a);
  const bb = pathKey(b);
  const len = Math.max(aa.length, bb.length);
  for (let i = 0; i < len; i += 1) {
    const av = aa[i] ?? 0;
    const bv = bb[i] ?? 0;
    if (av !== bv) return av - bv;
  }
  return a.id.localeCompare(b.id);
}

function buildDecisionTreeRows(
  decisions: DecisionSummary[],
  selectedTags: string[],
  showSuperseded: boolean,
  collapsed: Set<string>,
): DecisionTreeRow[] {
  const byId = new Map(decisions.map((decision) => [decision.id, decision]));
  const children = new Map<string, DecisionSummary[]>();
  const roots: DecisionSummary[] = [];
  for (const decision of decisions) {
    const parent = decision.parent ?? null;
    if (parent && byId.has(parent)) {
      const bucket = children.get(parent) ?? [];
      bucket.push(decision);
      children.set(parent, bucket);
    } else {
      roots.push(decision);
    }
  }
  for (const bucket of children.values()) bucket.sort(compareDecisionPath);
  roots.sort(compareDecisionPath);
  const tagFilterActive = selectedTags.length > 0;
  const tagMatches = (decision: DecisionSummary) =>
    !tagFilterActive || (decision.tags ?? []).some((tag) => selectedTags.includes(tag));
  const visibleRows: DecisionTreeRow[] = [];

  function visit(decision: DecisionSummary, depth: number): boolean {
    const kids = children.get(decision.id) ?? [];
    const childMatches = kids.map((child) => visit(child, depth + 1));
    const descendantVisible = childMatches.some(Boolean);
    const ownVisible = tagMatches(decision) && (showSuperseded || !decision.superseded);
    const visible = ownVisible || descendantVisible;
    if (!visible) return false;

    const row: DecisionTreeRow = {
      decision,
      depth,
      context: !ownVisible && descendantVisible,
      ghost: Boolean(decision.superseded && descendantVisible && !showSuperseded),
    };
    const insertAt = visibleRows.findIndex((candidate) => {
      const candidatePath = candidate.decision.path ?? '';
      const path = decision.path ?? '';
      return candidatePath.startsWith(`${path}.`);
    });
    if (insertAt >= 0) visibleRows.splice(insertAt, 0, row);
    else visibleRows.push(row);
    if (collapsed.has(decision.id)) {
      let i = visibleRows.length - 1;
      while (i >= 0) {
        const candidate = visibleRows[i];
        if (candidate.decision.id === decision.id) break;
        if ((candidate.decision.path ?? '').startsWith(`${decision.path ?? ''}.`)) {
          visibleRows.splice(i, 1);
        }
        i -= 1;
      }
    }
    return true;
  }

  for (const root of roots) visit(root, 0);
  visibleRows.sort((a, b) => compareDecisionPath(a.decision, b.decision));
  return visibleRows.filter((row) => {
    const path = row.decision.path ?? '';
    return ![...collapsed].some((id) => {
      const parent = byId.get(id);
      const parentPath = parent?.path;
      return parentPath && path.startsWith(`${parentPath}.`);
    });
  });
}

export function DecisionsView({ projectId }: { projectId: string }) {
  const navigate = useNavigate();
  const search = useSearch({ strict: false }) as DecisionsSearch;
  const refresh = useRefreshToken();
  const refreshBump = useRefreshBump();
  const decisions = useResource(`decisions:${projectId}:${refresh}`, () => fetchDecisions(projectId));
  const tags = useMemo(() => tagOptions(decisions.data ?? []), [decisions.data]);
  const selectedTags = useMemo(() => searchList(search.tag), [search.tag]);
  const [showSuperseded, setShowSuperseded] = useState(false);
  const [collapsed, setCollapsed] = useState<Set<string>>(() => new Set());
  const [creatingUnder, setCreatingUnder] = useState<string | null>(null);
  const [selectMode, setSelectMode] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(() => new Set());
  const [generateOpen, setGenerateOpen] = useState(false);
  const [generateSelectionOpen, setGenerateSelectionOpen] = useState(false);

  // Drop selections that no longer exist once a live refresh lands, so a
  // stale id never rides along in a "Generate from N selected" request.
  useEffect(() => {
    if (!decisions.data) return;
    const known = new Set(decisions.data.map((item) => item.id));
    setSelected((current) => {
      const next = new Set([...current].filter((id) => known.has(id)));
      return next.size === current.size ? current : next;
    });
  }, [decisions.data]);

  const supersededCount = useMemo(
    () => (decisions.data ?? []).filter((d) => d.superseded).length,
    [decisions.data],
  );

  const rows = useMemo(
    () => buildDecisionTreeRows(decisions.data ?? [], selectedTags, showSuperseded, collapsed),
    [collapsed, decisions.data, selectedTags, showSuperseded],
  );

  function setSelectedTags(next: string[]) {
    void navigate({
      search: routeSearch((prev) => ({
        ...prev,
        tag: next.length > 0 ? next : undefined,
      })),
    });
  }

  function openNode(id: string) {
    void navigate({
      search: routeSearch((prev) => appendDrawerStack(prev, id)),
    });
  }

  function activateRow(id: string) {
    if (selectMode) toggleSelected(id);
    else openNode(id);
  }

  function openRowFromKeyboard(event: KeyboardEvent<HTMLDivElement>, id: string) {
    if (event.target !== event.currentTarget) return;
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      activateRow(id);
    }
  }

  function stopRowOpen(event: { stopPropagation: () => void }) {
    event.stopPropagation();
  }

  function toggleCollapsed(id: string) {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  function toggleSelected(id: string) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  async function addSubDecision(parentId: string) {
    setCreatingUnder(parentId);
    try {
      const result = await createDecision({
        project: projectId,
        title: 'New sub-decision',
        properties: { PARENT: parentId },
        body: '** Context\n\n** Decision\n\n** Consequences\n',
      });
      refreshBump();
      openNode(result.id);
    } catch (error) {
      // Keep the tree surface usable; the daemon returns validation details in
      // the console for now rather than adding a broad create-dialog state.
      console.error('Failed to create sub-decision', error);
    } finally {
      setCreatingUnder(null);
    }
  }

  if (decisions.error) return <ErrorPanel error={decisions.error} />;

  return (
    <div className="flex flex-col gap-4">
      <PageHeader
        title="Decisions"
        count={rows.length}
        description={`Decision records for ${projectId}.`}
        actions={
          <>
            <Button
              type="button"
              variant={selectMode ? 'default' : 'outline'}
              size="sm"
              aria-pressed={selectMode}
              onClick={() => {
                setSelectMode((v) => !v);
                setSelected(new Set());
              }}
            >
              {selectMode ? `${selected.size} selected` : 'Select'}
            </Button>
            {selectMode ? (
              <Button type="button" size="sm" disabled={selected.size === 0} onClick={() => setGenerateSelectionOpen(true)}>
                <Sparkles />
                Generate from {selected.size} selected
              </Button>
            ) : (
              <Button type="button" size="sm" onClick={() => setGenerateOpen(true)}>
                <Sparkles />
                Generate artifact
              </Button>
            )}
          </>
        }
      />
      <section className="rounded-xl border bg-card" aria-label="Decisions">
        <div className="border-b p-3">
          <div className="flex flex-wrap items-center gap-2">
            <TagFilterInput
              options={tags}
              selected={selectedTags}
              onChange={setSelectedTags}
              ariaControls={DECISIONS_LIST_ID}
              className="md:w-96"
            />
            {supersededCount > 0 && (
              <Button
                variant="outline"
                size="sm"
                aria-pressed={showSuperseded}
                onClick={() => setShowSuperseded((v) => !v)}
                className="shrink-0 text-muted-foreground"
              >
                {showSuperseded ? `Hide superseded (${supersededCount})` : `Show superseded (${supersededCount})`}
              </Button>
            )}
          </div>
        </div>
        <div id={DECISIONS_LIST_ID} className="divide-y" aria-busy={decisions.loading}>
          {decisions.loading && rows.length === 0 ? (
            <div className="p-4 text-sm text-muted-foreground">Loading decisions…</div>
          ) : rows.length === 0 ? (
            <div className="p-4 text-sm text-muted-foreground">No decisions match the current filters.</div>
          ) : (
            rows.map((row) => {
              const decision = row.decision;
              const decisionTags = decision.tags ?? [];
              const decisionText = firstSentence(decision.preview) || decision.title || decision.id;
              const hasChildren = (decision.children ?? []).length > 0;
              const isCollapsed = collapsed.has(decision.id);
              return (
                <div
                  key={decision.id}
                  onClick={() => activateRow(decision.id)}
                  className={cn(
                    'grid w-full cursor-pointer gap-2 px-3 py-3 transition-colors hover:bg-muted/30 md:grid-cols-[1fr_auto] md:items-center',
                    row.ghost && 'bg-muted/20 opacity-70',
                    row.context && 'bg-muted/10',
                  )}
                  style={{ paddingLeft: `${0.75 + row.depth * 1.25}rem` }}
                >
                  <div className="flex min-w-0 items-start gap-2">
                    <Button
                      type="button"
                      variant="ghost"
                      size="icon-sm"
                      className={cn('mt-0.5 shrink-0', !hasChildren && 'invisible')}
                      aria-label={isCollapsed ? `Expand ${decision.id}` : `Collapse ${decision.id}`}
                      onPointerDown={stopRowOpen}
                      onClick={(event) => {
                        event.stopPropagation();
                        toggleCollapsed(decision.id);
                      }}
                    >
                      {isCollapsed ? <ChevronRight /> : <ChevronDown />}
                    </Button>
                    <div className="min-w-0 flex-1">
                      <div className="mb-1 flex flex-wrap items-center gap-1.5">
                        <Badge variant="outline" className="font-mono">{decision.path ?? '—'}</Badge>
                        <CopyIdBadge
                          value={decision.id}
                          className="h-4 w-fit origin-top-left rounded-sm px-1 text-[10px] leading-none"
                        />
                        {row.context ? <Badge variant="secondary">ancestor context</Badge> : null}
                        {decision.superseded ? <Badge variant="outline">superseded</Badge> : null}
                      </div>
                      <div
                        role="button"
                        tabIndex={0}
                        aria-label={`Open ${decision.id}`}
                        className="min-w-0 rounded-sm text-left focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40"
                        onClick={(event) => {
                          event.stopPropagation();
                          activateRow(decision.id);
                        }}
                        onKeyDown={(event) => openRowFromKeyboard(event, decision.id)}
                      >
                        <p className="text-sm font-medium leading-5 text-pretty break-words">{decisionText}</p>
                        <p className="text-xs text-muted-foreground">{decision.title || decision.id}</p>
                      </div>
                    </div>
                  </div>
                  <div
                    className="flex flex-wrap items-center gap-1.5 md:justify-end"
                    onPointerDown={stopRowOpen}
                    onClick={stopRowOpen}
                  >
                    {decisionTags.map((tag) => (
                      <Badge key={tag} variant="secondary" className="hidden sm:inline-flex">{tag}</Badge>
                    ))}
                    {selectMode ? (
                      <Checkbox
                        checked={selected.has(decision.id)}
                        onCheckedChange={() => toggleSelected(decision.id)}
                        aria-label={`Select ${decision.title || decision.id}`}
                      />
                    ) : (
                      <Button
                        type="button"
                        variant="outline"
                        size="sm"
                        disabled={creatingUnder === decision.id}
                        onClick={() => void addSubDecision(decision.id)}
                      >
                        <Plus />
                        Add sub-decision
                      </Button>
                    )}
                  </div>
                </div>
              );
            })
          )}
        </div>
      </section>
      <GenerateArtifactDialog
        projectId={projectId}
        open={generateOpen}
        onOpenChange={setGenerateOpen}
        nodes={(decisions.data ?? []).map((item) => item.id)}
        nodeLabels={(decisions.data ?? []).map((item) => item.title || item.id)}
      />
      <GenerateArtifactDialog
        projectId={projectId}
        open={generateSelectionOpen}
        onOpenChange={(next) => {
          setGenerateSelectionOpen(next);
          if (!next) setSelectMode(false);
        }}
        nodes={[...selected]}
        nodeLabels={[...selected].map((id) => (decisions.data ?? []).find((item) => item.id === id)?.title ?? id)}
      />
      <NodeModal
        projectId={projectId}
        nodeKind="decision"
        seed={{ decisions: decisions.data ?? null }}
      />
    </div>
  );
}
