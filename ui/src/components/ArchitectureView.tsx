// @arch arch_MK2Q2.7
import { useMemo, useState } from 'react';
import { useNavigate, useSearch } from '@tanstack/react-router';
import { ChevronDown, ChevronRight } from 'lucide-react';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import { fetchArchitecture } from '@/lib/api';
import { appendDrawerStack, routeSearch, type AppSearch } from '@/lib/searchState';
import type { ArchitectureSummary } from '@/lib/types';
import { useResource } from '@/lib/useResource';

import { CopyIdBadge } from './CopyIdBadge';
import { ErrorPanel, PageHeader } from './Primitives';
import { NodeListView } from './node-views/NodeListView';
import { NodeModal } from './node-views/NodeModal';
import { firstSentence } from './node-views/orgNodes';

const ARCHITECTURE_LIST_ID = 'architecture-list-region';

type ArchitectureSearch = AppSearch & {
  q?: string;
};

type ArchitectureTreeRow = {
  item: ArchitectureSummary;
  depth: 0 | 1;
  childCount: number;
  collapsible: boolean;
};

export function ArchitectureView({ projectId }: { projectId: string }) {
  const navigate = useNavigate();
  const search = useSearch({ strict: false }) as ArchitectureSearch;
  const refresh = useRefreshToken();
  const [collapsedRoots, setCollapsedRoots] = useState<Set<string>>(() => new Set());
  const architecture = useResource(`architecture:${projectId}:${refresh}`, () => fetchArchitecture(projectId));
  const query = search.q ?? '';
  const filteredTree = useMemo(() => {
    const q = query.trim().toLowerCase();
    const roots: ArchitectureSummary[] = [];
    const childrenByParent = new Map<string, ArchitectureSummary[]>();
    const matches = (item: ArchitectureSummary) => {
      if (!q) return true;
      const haystack = `${item.id} ${item.label} ${item.interface.join(' ')} ${item.description ?? ''}`.toLowerCase();
      return haystack.includes(q);
    };

    for (const item of architecture.data ?? []) {
      if (item.parent_id) {
        const children = childrenByParent.get(item.parent_id) ?? [];
        children.push(item);
        childrenByParent.set(item.parent_id, children);
      } else {
        roots.push(item);
      }
    }

    const rows: ArchitectureTreeRow[] = [];
    for (const root of roots) {
      const children = childrenByParent.get(root.id) ?? [];
      const visibleChildren = q ? children.filter(matches) : children;
      if (q && !matches(root) && visibleChildren.length === 0) continue;

      rows.push({
        item: root,
        depth: 0,
        childCount: children.length,
        collapsible: children.length > 0,
      });

      const showChildren = q || !collapsedRoots.has(root.id);
      if (showChildren) {
        for (const child of visibleChildren) {
          rows.push({ item: child, depth: 1, childCount: 0, collapsible: false });
        }
      }
    }
    return rows;
  }, [architecture.data, collapsedRoots, query]);

  function setQuery(value: string) {
    void navigate({
      search: routeSearch((prev) => ({ ...prev, q: value || undefined })),
      replace: true,
    });
  }

  function openNode(id: string) {
    void navigate({ search: routeSearch((prev) => appendDrawerStack(prev, id)) });
  }

  function toggleRoot(id: string) {
    setCollapsedRoots((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  if (architecture.error) return <ErrorPanel error={architecture.error} />;

  return (
    <div className="flex min-h-0 flex-col gap-4">
      <PageHeader
        title="Architecture"
        count={filteredTree.length}
        description={`Org-sourced mechanism model for ${projectId}.`}
      />
      <NodeListView
        ariaLabel="Architecture"
        items={filteredTree}
        getId={(row) => row.item.id}
        search={query}
        onSearchChange={setQuery}
        onSelect={openNode}
        loading={architecture.loading}
        listId={ARCHITECTURE_LIST_ID}
        renderRow={(row) => {
          const item = row.item;
          const collapsed = collapsedRoots.has(item.id);
          return (
            <div className="grid w-full gap-2 md:grid-cols-[1fr_auto] md:items-center">
              <div className="min-w-0">
                <div className="flex min-w-0 items-center gap-2" style={{ paddingLeft: row.depth ? 28 : 0 }}>
                  {row.collapsible ? (
                    <Button
                      type="button"
                      variant="ghost"
                      size="icon-sm"
                      className="-ml-2 size-7 shrink-0"
                      aria-label={`${collapsed ? 'Expand' : 'Collapse'} ${item.label || item.id}`}
                      onPointerDown={(event) => event.stopPropagation()}
                      onClick={(event) => {
                        event.stopPropagation();
                        toggleRoot(item.id);
                      }}
                    >
                      {collapsed ? <ChevronRight /> : <ChevronDown />}
                    </Button>
                  ) : (
                    <span className="size-7 shrink-0" aria-hidden="true" />
                  )}
                  <div className="min-w-0">
                    <CopyIdBadge value={item.id} className="h-4 w-fit origin-top-left scale-[0.65] rounded-sm px-1 text-[10px] leading-none" />
                    <div className="flex min-w-0 items-center gap-2">
                      <p className="truncate text-sm font-medium">{item.label || item.id}</p>
                    </div>
                    <p className="truncate text-xs text-muted-foreground">{firstSentence(item.description ?? item.interface[0])}</p>
                  </div>
                </div>
              </div>
              <div className="flex flex-wrap gap-1.5 md:justify-end">
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
          row.item.motivated_by[0] ? (
            <CopyIdBadge value={row.item.motivated_by[0]} className="hidden h-4 origin-center scale-[0.85] rounded-sm px-1 text-[10px] leading-none sm:inline-flex" />
          ) : null
        )}
      />
      <NodeModal projectId={projectId} nodeKind="architecture" seed={{ architecture: architecture.data ?? null }} />
    </div>
  );
}
