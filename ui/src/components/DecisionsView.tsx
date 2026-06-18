// @arch arch_MK2Q2.7
import { useMemo, useState } from 'react';
import { useNavigate, useSearch } from '@tanstack/react-router';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import { fetchDecisions } from '@/lib/api';
import { appendDrawerStack, routeSearch, searchList, type AppSearch } from '@/lib/searchState';
import type { DecisionSummary } from '@/lib/types';
import { useResource } from '@/lib/useResource';

import { CopyIdBadge } from './CopyIdBadge';
import { ErrorPanel, PageHeader } from './Primitives';
import { NodeListView } from './node-views/NodeListView';
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

export function DecisionsView({ projectId }: { projectId: string }) {
  const navigate = useNavigate();
  const search = useSearch({ strict: false }) as DecisionsSearch;
  const refresh = useRefreshToken();
  const decisions = useResource(`decisions:${projectId}:${refresh}`, () => fetchDecisions(projectId));
  const tags = useMemo(() => tagOptions(decisions.data ?? []), [decisions.data]);
  const selectedTags = useMemo(() => searchList(search.tag), [search.tag]);
  const [showSuperseded, setShowSuperseded] = useState(false);

  const supersededCount = useMemo(
    () => (decisions.data ?? []).filter((d) => d.superseded).length,
    [decisions.data],
  );

  const filtered = useMemo(() => {
    return [...(decisions.data ?? [])]
      .sort((a, b) => parseInt(a.id.slice(4), 10) - parseInt(b.id.slice(4), 10))
      .filter((decision) => {
        if (!showSuperseded && decision.superseded) return false;
        if (selectedTags.length === 0) return true;
        return (decision.tags ?? []).some((tag) => selectedTags.includes(tag));
      });
  }, [decisions.data, selectedTags, showSuperseded]);

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

  if (decisions.error) return <ErrorPanel error={decisions.error} />;

  return (
    <div className="flex flex-col gap-4">
      <PageHeader
        title="Decisions"
        count={filtered.length}
        description={`Decision records for ${projectId}.`}
      />
      <NodeListView
        ariaLabel="Decisions"
        items={filtered}
        getId={(item) => item.id}
        onSelect={openNode}
        loading={decisions.loading}
        listId={DECISIONS_LIST_ID}
        filters={
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
        }
        renderRow={(decision) => {
          const decisionTags = decision.tags ?? [];
          const decisionText = firstSentence(decision.preview) || decision.title || decision.id;
          return (
            <div className="grid w-full gap-2 md:grid-cols-[1fr_auto] md:items-center">
              <div className="min-w-0">
                <CopyIdBadge
                  value={decision.id}
                  className="h-4 w-fit origin-top-left scale-[0.65] rounded-sm px-1 text-[10px] leading-none"
                />
                <div className="flex min-w-0 items-start gap-2">
                  <p className="text-sm font-medium leading-5 text-pretty break-words">{decisionText}</p>
                </div>
                <p className="text-xs text-muted-foreground">{decision.title || decision.id}</p>
              </div>
              <div className="flex flex-wrap gap-1.5 md:justify-end">
                {decisionTags.map((tag) => (
                  <Badge key={tag} variant="secondary" className="hidden sm:inline-flex">{tag}</Badge>
                ))}
              </div>
            </div>
          );
        }}
      />
      <NodeModal
        projectId={projectId}
        nodeKind="decision"
        seed={{ decisions: decisions.data ?? null }}
      />
    </div>
  );
}
