// @arch arch_MK2Q2.7
import { useEffect, useMemo, useState } from 'react';
import { useNavigate, useSearch } from '@tanstack/react-router';
import { ChevronDown, ChevronRight, Plus, Sparkles } from 'lucide-react';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { useMe } from '@/hooks/useMe';
import { useRefreshBump, useRefreshToken } from '@/hooks/useRefreshBus';
import { createDecision, fetchDecisions } from '@/lib/api';
import { appendDrawerStack, routeSearch, searchList, type AppSearch } from '@/lib/searchState';
import type { DecisionSummary } from '@/lib/types';
import { useResource } from '@/lib/useResource';
import { cn } from '@/lib/utils';

import { CopyIdBadge } from './CopyIdBadge';
import { GenerateArtifactDialog } from './GenerateArtifactDialog';
import { ErrorPanel, PageHeader } from './Primitives';
import { NodeListView } from './node-views/NodeListView';
import { NodeModal } from './node-views/NodeModal';
import { TagFilterInput } from './node-views/TagFilterInput';
import { firstSentence } from './node-views/orgNodes';

const DECISIONS_LIST_ID = 'decisions-list-region';

type DecisionsSearch = AppSearch & {
  q?: string;
  tag?: string[];
};

type DecisionTreeRow = {
  decision: DecisionSummary;
  depth: number;
  childCount: number;
  collapsible: boolean;
  context: boolean;
  ghost: boolean;
};

function tagOptions(items: DecisionSummary[]): string[] {
  return Array.from(new Set(items.flatMap((item) => item.tags ?? []))).sort();
}

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

function matchesQuery(decision: DecisionSummary, q: string): boolean {
  if (!q) return true;
  const haystack = `${decision.id} ${decision.title} ${decision.preview ?? ''} ${(decision.tags ?? []).join(' ')} ${decision.path ?? ''}`.toLowerCase();
  return haystack.includes(q);
}

function buildDecisionTreeRows(
  decisions: DecisionSummary[],
  selectedTags: string[],
  showSuperseded: boolean,
  collapsed: Set<string>,
  query: string,
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

  const q = query.trim().toLowerCase();
  const tagFilterActive = selectedTags.length > 0;
  const tagMatches = (decision: DecisionSummary) =>
    !tagFilterActive || (decision.tags ?? []).some((tag) => selectedTags.includes(tag));
  const visibleRows: DecisionTreeRow[] = [];

  function visit(decision: DecisionSummary, depth: number): boolean {
    const kids = children.get(decision.id) ?? [];
    const childMatches = kids.map((child) => visit(child, depth + 1));
    const descendantVisible = childMatches.some(Boolean);
    const ownVisible =
      tagMatches(decision) &&
      matchesQuery(decision, q) &&
      (showSuperseded || !decision.superseded);
    // When searching, keep ancestors so matching descendants stay nested.
    const visible = ownVisible || descendantVisible;
    if (!visible) return false;

    const row: DecisionTreeRow = {
      decision,
      depth,
      childCount: kids.length,
      collapsible: kids.length > 0,
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

    // Collapse only when not searching — search forces the matching subtree open.
    if (!q && collapsed.has(decision.id)) {
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
  if (q) return visibleRows;
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
  const { can } = useMe();
  const canGenerate = can(projectId, 'artifacts.generate');
  const decisions = useResource(`decisions:${projectId}:${refresh}`, () => fetchDecisions(projectId));
  const tags = useMemo(() => tagOptions(decisions.data ?? []), [decisions.data]);
  const selectedTags = useMemo(() => searchList(search.tag), [search.tag]);
  const query = search.q ?? '';
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
    () => buildDecisionTreeRows(decisions.data ?? [], selectedTags, showSuperseded, collapsed, query),
    [collapsed, decisions.data, query, selectedTags, showSuperseded],
  );

  function setQuery(value: string) {
    void navigate({
      search: routeSearch((prev) => ({ ...prev, q: value || undefined })),
      replace: true,
    });
  }

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

  const filtersActive = selectedTags.length > 0 || (!showSuperseded && supersededCount > 0);

  return (
    <div className="flex min-h-0 flex-col gap-4">
      <PageHeader
        title="Decisions"
        count={rows.length}
        description={`Decision records for ${projectId}.`}
        actions={
          canGenerate ? (
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
          ) : null
        }
      />
      <NodeListView
        ariaLabel="Decisions"
        items={rows}
        getId={(row) => row.decision.id}
        search={query}
        onSearchChange={setQuery}
        onSelect={selectMode ? toggleSelected : openNode}
        loading={decisions.loading}
        listId={DECISIONS_LIST_ID}
        filters={
          <>
            <TagFilterInput
              options={tags}
              selected={selectedTags}
              onChange={setSelectedTags}
              ariaControls={DECISIONS_LIST_ID}
              className="md:w-64"
            />
            {supersededCount > 0 ? (
              <Button
                variant="outline"
                size="sm"
                aria-pressed={showSuperseded}
                onClick={() => setShowSuperseded((v) => !v)}
                className="shrink-0 text-muted-foreground"
              >
                {showSuperseded ? `Hide superseded (${supersededCount})` : `Show superseded (${supersededCount})`}
              </Button>
            ) : null}
          </>
        }
        emptyLabel={
          query.trim() || filtersActive ? (
            <div className="flex flex-col items-center gap-3">
              <span>
                {query.trim()
                  ? `No matches for "${query.trim()}".`
                  : 'No decisions match the current filters.'}
              </span>
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={() => {
                  setQuery('');
                  setSelectedTags([]);
                  setShowSuperseded(true);
                }}
              >
                Show all
              </Button>
            </div>
          ) : (
            <>
              No decisions yet. Record the first with{' '}
              <code className="font-mono text-foreground">orgasmic decision create</code>, or let
              your agent infer them from the repo with{' '}
              <code className="font-mono text-foreground">/orgasmic resume</code>.
            </>
          )
        }
        renderRow={(row) => {
          const decision = row.decision;
          const collapsedRow = collapsed.has(decision.id);
          const decisionText = firstSentence(decision.preview) || decision.title || decision.id;
          const decisionTags = decision.tags ?? [];
          return (
            <div
              className={cn(
                'grid w-full gap-2 md:grid-cols-[1fr_auto] md:items-center',
                row.ghost && 'opacity-70',
                row.context && 'opacity-80',
              )}
            >
              <div className="min-w-0">
                <div className="flex min-w-0 items-center gap-2" style={{ paddingLeft: row.depth ? row.depth * 28 : 0 }}>
                  {row.collapsible ? (
                    <Button
                      type="button"
                      variant="ghost"
                      size="icon-sm"
                      className="-ml-2 size-7 shrink-0"
                      aria-label={`${collapsedRow ? 'Expand' : 'Collapse'} ${decision.title || decision.id}`}
                      onPointerDown={(event) => event.stopPropagation()}
                      onClick={(event) => {
                        event.stopPropagation();
                        toggleCollapsed(decision.id);
                      }}
                    >
                      {collapsedRow ? <ChevronRight /> : <ChevronDown />}
                    </Button>
                  ) : (
                    <span className="size-7 shrink-0" aria-hidden="true" />
                  )}
                  <div className="min-w-0">
                    <div className="flex flex-wrap items-center gap-1.5">
                      <CopyIdBadge
                        value={decision.id}
                        className="h-4 w-fit origin-top-left scale-[0.65] rounded-sm px-1 text-[10px] leading-none"
                      />
                      {row.context ? <Badge variant="secondary" className="h-4 scale-[0.85] origin-left text-[10px]">context</Badge> : null}
                      {decision.superseded ? <Badge variant="outline" className="h-4 scale-[0.85] origin-left text-[10px]">superseded</Badge> : null}
                    </div>
                    <div className="flex min-w-0 items-center gap-2">
                      <p className="truncate text-sm font-medium">{decisionText}</p>
                    </div>
                    <p className="truncate text-xs text-muted-foreground">{decision.title || decision.id}</p>
                  </div>
                </div>
              </div>
              <div className="flex flex-wrap gap-1.5 md:justify-end">
                {decisionTags.map((tag) => (
                  <Badge key={tag} variant="secondary" className="hidden sm:inline-flex">
                    {tag}
                  </Badge>
                ))}
                {row.childCount > 0 ? (
                  <Badge variant="secondary" className="hidden sm:inline-flex">
                    {row.childCount} child{row.childCount === 1 ? '' : 'ren'}
                  </Badge>
                ) : null}
              </div>
            </div>
          );
        }}
        renderActionZone={(row) => (
          <div className="flex items-center gap-1.5">
            {selectMode ? (
              <Checkbox
                checked={selected.has(row.decision.id)}
                onCheckedChange={() => toggleSelected(row.decision.id)}
                aria-label={`Select ${row.decision.title || row.decision.id}`}
              />
            ) : (
              <Button
                type="button"
                variant="outline"
                size="sm"
                className="hidden sm:inline-flex"
                disabled={creatingUnder === row.decision.id}
                onClick={() => void addSubDecision(row.decision.id)}
              >
                <Plus />
                Add sub-decision
              </Button>
            )}
          </div>
        )}
      />
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
