import { useEffect, useMemo, useRef, useState } from 'react';
import {
  CheckCircle2,
  FileCode2,
  GitFork,
  Pencil,
  Puzzle,
  RotateCcw,
  Save,
  Trash2,
} from 'lucide-react';
import { toast } from 'sonner';

import { ErrorPanel, Loading, PageHeader } from '@/components/Primitives';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Textarea } from '@/components/ui/textarea';
import {
  fetchPromptParts,
  fetchPromptSpec,
  fetchPromptSpecs,
  forkPromptSpec,
  savePromptPart,
  savePromptSpec,
} from '@/lib/api';
import { useResource } from '@/lib/useResource';
import type { PromptPartSummary, PromptSpecSummary } from '@/lib/types';
import { cn } from '@/lib/utils';

import {
  parseOrgSourceNodes,
  updateOrgNodeBody,
  updateOrgNodeProperty,
  updateOrgRootBody,
  type OrgSourceNode,
} from './node-views/orgNodes';

type PromptLayerBlock =
  | {
      kind: 'spec';
      origin: 'own' | 'inherited';
      specId: string;
      sourcePath: string;
      title: string;
      body: string;
      merge: 'replace' | 'append';
    }
  | {
      kind: 'part';
      partId: string;
      sourcePath: string;
      title: string;
      body: string;
      dirty: boolean;
      missing: boolean;
    };

type PromptLayerSection = {
  title: string;
  blocks: PromptLayerBlock[];
};

type LayeredPrompt = {
  chain: string[];
  sections: PromptLayerSection[];
  selectedPartIds: string[];
  missingPartIds: string[];
};

function firstSpecId(specs: PromptSpecSummary[] | null): string {
  return specs?.[0]?.id ?? '';
}

function splitWords(value: string | null | undefined): string[] {
  return (value ?? '')
    .split(/[\s,]+/)
    .map((part) => part.trim())
    .filter(Boolean);
}

function isNotesSection(title: string): boolean {
  return title.trim().toLowerCase() === 'notes';
}

function propertyValue(node: OrgSourceNode | null | undefined, key: string): string {
  return node?.properties.find((property) => property.key === key)?.value ?? '';
}

function hasDirtyPart(part: PromptPartSummary, draft: string | undefined): boolean {
  return draft !== undefined && draft !== part.source;
}

function partSource(part: PromptPartSummary, drafts: Record<string, string>): string {
  return drafts[part.id] ?? part.source;
}

function partBody(part: PromptPartSummary, drafts: Record<string, string>): string {
  const source = partSource(part, drafts);
  return parseOrgSourceNodes(source).root?.body ?? part.body;
}

function partTargetSection(part: PromptPartSummary, drafts: Record<string, string>): string {
  const source = partSource(part, drafts);
  return propertyValue(parseOrgSourceNodes(source).root, 'TARGET_SECTION') || part.target_section;
}

function buildLayeredPrompt({
  selectedId,
  selectedSource,
  specs,
  parts,
  partDrafts,
}: {
  selectedId: string;
  selectedSource: string;
  specs: PromptSpecSummary[];
  parts: PromptPartSummary[];
  partDrafts: Record<string, string>;
}): LayeredPrompt {
  const specById = new Map(specs.map((spec) => [spec.id, spec]));
  const partById = new Map(parts.map((part) => [part.id, part]));
  const order: string[] = [];
  const byTitle = new Map<string, PromptLayerBlock[]>();
  const chain: string[] = [];

  function setSection(title: string, blocks: PromptLayerBlock[]) {
    if (!byTitle.has(title)) order.push(title);
    byTitle.set(title, blocks);
  }

  function appendSection(title: string, block: PromptLayerBlock) {
    const blocks = byTitle.get(title);
    if (blocks) blocks.push(block);
    else setSection(title, [block]);
  }

  function resolve(specId: string, stack: string[]) {
    if (stack.includes(specId)) return;
    const spec = specById.get(specId);
    if (!spec) return;
    const source = specId === selectedId ? selectedSource : spec.source;
    const doc = parseOrgSourceNodes(source);
    const parentId = propertyValue(doc.root, 'EXTENDS') || spec.extends || '';
    if (parentId) resolve(parentId, [...stack, specId]);
    chain.push(specId);

    for (const section of doc.sections) {
      if (isNotesSection(section.title)) continue;
      const merge = propertyValue(section, 'MERGE') === 'append' ? 'append' : 'replace';
      const block: PromptLayerBlock = {
        kind: 'spec',
        origin: specId === selectedId ? 'own' : 'inherited',
        specId,
        sourcePath: spec.source_path,
        title: section.title,
        body: section.body,
        merge,
      };
      if (merge === 'append' && byTitle.has(section.title)) {
        appendSection(section.title, block);
      } else {
        setSection(section.title, [block]);
      }
    }
  }

  resolve(selectedId, []);

  const selectedRoot = parseOrgSourceNodes(selectedSource).root;
  const selectedPartIds = splitWords(propertyValue(selectedRoot, 'USES_PARTS'));
  const missingPartIds: string[] = [];
  for (const partId of selectedPartIds) {
    const part = partById.get(partId);
    if (!part) {
      missingPartIds.push(partId);
      continue;
    }
    const title = partTargetSection(part, partDrafts);
    appendSection(title, {
      kind: 'part',
      partId,
      sourcePath: part.source_path,
      title,
      body: partBody(part, partDrafts),
      dirty: hasDirtyPart(part, partDrafts[part.id]),
      missing: false,
    });
  }

  return {
    chain,
    sections: order.map((title) => ({ title, blocks: byTitle.get(title) ?? [] })),
    selectedPartIds,
    missingPartIds,
  };
}

export function PromptStudioView({ projectId: _projectId }: { projectId: string | null }) {
  const specs = useResource('prompt-specs', fetchPromptSpecs);
  const parts = useResource('prompt-parts', fetchPromptParts);
  const [selectedId, setSelectedId] = useState('');
  const [source, setSource] = useState('');
  const [partDrafts, setPartDrafts] = useState<Record<string, string>>({});
  const [saving, setSaving] = useState(false);
  const [editingPartId, setEditingPartId] = useState<string | null>(null);
  const loadedSpecId = useRef<string | null>(null);

  const selected = useResource(
    `prompt-spec:${selectedId}`,
    () => fetchPromptSpec(selectedId),
    { enabled: Boolean(selectedId) },
  );

  const selectedSummary = useMemo(
    () => specs.data?.find((spec) => spec.id === selectedId) ?? selected.data,
    [selected.data, selectedId, specs.data],
  );
  const selectedDoc = useMemo(() => parseOrgSourceNodes(source), [source]);
  const dirtySpec = selected.data ? source !== selected.data.source : false;
  const dirtyPartIds = useMemo(
    () =>
      (parts.data ?? [])
        .filter((part) => hasDirtyPart(part, partDrafts[part.id]))
        .map((part) => part.id),
    [partDrafts, parts.data],
  );
  const dirty = dirtySpec || dirtyPartIds.length > 0;
  const layered = useMemo(
    () =>
      buildLayeredPrompt({
        selectedId,
        selectedSource: source,
        specs: specs.data ?? [],
        parts: parts.data ?? [],
        partDrafts,
      }),
    [partDrafts, parts.data, selectedId, source, specs.data],
  );

  useEffect(() => {
    if (!selectedId && specs.data?.length) setSelectedId(firstSpecId(specs.data));
  }, [selectedId, specs.data]);

  useEffect(() => {
    if (!selected.data) return;
    const specChanged = loadedSpecId.current !== selected.data.id;
    setSource(selected.data.source);
    if (specChanged) {
      setPartDrafts({});
      setEditingPartId(null);
    }
    loadedSpecId.current = selected.data.id;
  }, [selected.data]);

  function openSpec(id: string) {
    if (id === selectedId) return;
    if (dirty && !window.confirm('Discard unsaved prompt edits and open another spec?')) return;
    setPartDrafts({});
    setEditingPartId(null);
    setSelectedId(id);
  }

  async function refreshSelected(nextId = selectedId) {
    await specs.refresh();
    await parts.refresh();
    if (nextId) await selected.refresh();
  }

  async function save() {
    if (!selectedId) return;
    setSaving(true);
    try {
      let savedCount = 0;
      if (dirtySpec) {
        const next = await savePromptSpec(selectedId, source);
        setSource(next.source);
        savedCount += 1;
      }
      const nextDrafts = { ...partDrafts };
      for (const partId of dirtyPartIds) {
        const draft = nextDrafts[partId];
        if (!draft) continue;
        await savePromptPart(partId, draft);
        delete nextDrafts[partId];
        savedCount += 1;
      }
      setPartDrafts(nextDrafts);
      toast.success('Prompt sources saved', {
        description: `${savedCount} file${savedCount === 1 ? '' : 's'} updated`,
      });
      await refreshSelected(selectedId);
    } catch (err) {
      toast.error('Prompt save failed', {
        description: err instanceof Error ? err.message : String(err),
      });
    } finally {
      setSaving(false);
    }
  }

  async function fork() {
    if (!selectedId) return;
    try {
      const next = await forkPromptSpec(selectedId);
      toast.success('Prompt spec forked');
      setSource(next.source);
      await refreshSelected(selectedId);
    } catch (err) {
      toast.error('Prompt spec fork failed', {
        description: err instanceof Error ? err.message : String(err),
      });
    }
  }

  function reset() {
    if (!selected.data) return;
    setSource(selected.data.source);
    setPartDrafts({});
    setEditingPartId(null);
  }

  function togglePart(partId: string) {
    const current = splitWords(propertyValue(selectedDoc.root, 'USES_PARTS'));
    const next = current.includes(partId)
      ? current.filter((id) => id !== partId)
      : [...current, partId];
    setSource(updateOrgNodeProperty(source, null, 'USES_PARTS', next.join(' ')));
  }

  function updatePartBody(part: PromptPartSummary, body: string) {
    setPartDrafts((current) => {
      const baseSource = current[part.id] ?? part.source;
      return { ...current, [part.id]: updateOrgRootBody(baseSource, body) };
    });
  }

  if (specs.error) return <ErrorPanel error={specs.error} />;
  if (parts.error) return <ErrorPanel error={parts.error} />;

  return (
    <div className="flex min-h-[calc(100vh-9rem)] flex-col gap-4">
      <PageHeader
        title="Prompt Studio"
        count={specs.data?.length ?? undefined}
        description="Layered prompt editor. Own blocks are editable; inherited blocks and prompt parts stay visible in final order."
        actions={
          <div className="flex flex-wrap items-center gap-2">
            <Select value={selectedId} onValueChange={openSpec}>
              <SelectTrigger className="h-9 w-[17rem]" aria-label="Prompt spec">
                <SelectValue placeholder="Select spec" />
              </SelectTrigger>
              <SelectContent>
                {(specs.data ?? []).map((spec) => (
                  <SelectItem key={spec.id} value={spec.id}>
                    <span className="flex min-w-0 items-center gap-2">
                      <FileCode2 className="size-4 shrink-0" />
                      <span className="truncate">{spec.id}</span>
                      <span className="text-xs text-muted-foreground">{spec.kind}</span>
                    </span>
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Button type="button" variant="outline" size="sm" onClick={fork} disabled={!selectedId}>
              <GitFork />
              Fork
            </Button>
            <Button type="button" variant="outline" size="sm" onClick={reset} disabled={!dirty}>
              <RotateCcw />
              Reset
            </Button>
            <Button type="button" size="sm" onClick={save} disabled={!dirty || saving}>
              <Save />
              {saving ? 'Saving...' : 'Save'}
            </Button>
          </div>
        }
      />

      <section className="grid min-h-0 flex-1 grid-rows-[auto_minmax(0,1fr)] rounded-md border bg-card">
        <div className="flex flex-wrap items-center justify-between gap-2 border-b px-3 py-2">
          <div className="min-w-0">
            <div className="truncate text-sm font-medium">{selectedSummary?.id ?? 'No spec'}</div>
            <div className="truncate font-mono text-xs text-muted-foreground">
              {selectedSummary?.source_path ?? ''}
            </div>
          </div>
          <div className="flex flex-wrap items-center gap-1.5">
            {layered.chain.length > 1 ? (
              <Badge variant="secondary">{layered.chain.join(' -> ')}</Badge>
            ) : null}
            {dirtySpec ? <Badge variant="outline">spec dirty</Badge> : null}
            {dirtyPartIds.map((partId) => (
              <Badge key={partId} variant="outline">part dirty: {partId}</Badge>
            ))}
          </div>
        </div>

        {selected.error ? (
          <ErrorPanel error={selected.error} />
        ) : selected.loading || specs.loading || parts.loading ? (
          <div className="p-3">
            <Loading label="Loading prompt sources..." />
          </div>
        ) : (
          <div className="min-h-0 overflow-auto p-3">
            <div className="mx-auto flex max-w-5xl flex-col gap-4">
              <PromptRootEditor source={source} root={selectedDoc.root} onChange={setSource} />
              {layered.missingPartIds.length > 0 ? (
                <div className="rounded-md border border-destructive/30 bg-destructive/5 px-3 py-2 text-sm text-destructive">
                  Missing prompt parts: {layered.missingPartIds.join(', ')}
                </div>
              ) : null}
              {layered.sections.map((section, index) => (
                <LayerSection
                  key={section.title}
                  index={index + 1}
                  section={section}
                  source={source}
                  onSourceChange={setSource}
                  allParts={parts.data ?? []}
                  selectedPartIds={layered.selectedPartIds}
                  onTogglePart={togglePart}
                  editingPartId={editingPartId}
                  onEditingPartIdChange={setEditingPartId}
                  onOpenSpec={openSpec}
                  onPartBodyChange={updatePartBody}
                />
              ))}
            </div>
          </div>
        )}
      </section>
    </div>
  );
}

function PromptRootEditor({
  source,
  root,
  onChange,
}: {
  source: string;
  root: OrgSourceNode | null;
  onChange: (next: string) => void;
}) {
  if (!root) {
    return (
      <Textarea
        value={source}
        onChange={(event) => onChange(event.target.value)}
        spellCheck={false}
        className="min-h-[32rem] resize-y font-mono text-[13px] leading-relaxed"
      />
    );
  }
  const editableProperties = root.properties.filter((property) => property.key !== 'USES_PARTS');
  return (
    <section className="rounded-md border bg-background">
      <div className="flex flex-wrap items-center justify-between gap-2 border-b px-3 py-2">
        <div className="min-w-0">
          <div className="truncate font-mono text-sm font-medium">{root.title}</div>
          <div className="text-xs text-muted-foreground">metadata</div>
        </div>
        <Badge variant="outline">prompt spec</Badge>
      </div>
      <div className="grid grid-cols-1 gap-2 px-3 py-3 md:grid-cols-2">
        {editableProperties.map((property) => (
          <label key={property.key} className="grid min-w-0 gap-1.5">
            <span className="font-mono text-[11px] text-muted-foreground">{property.key}</span>
            <Input
              value={property.value}
              onChange={(event) =>
                onChange(updateOrgNodeProperty(source, null, property.key, event.target.value))
              }
              spellCheck={false}
              className="font-mono text-[13px]"
            />
          </label>
        ))}
      </div>
    </section>
  );
}

function LayerSection({
  index,
  section,
  source,
  onSourceChange,
  allParts,
  selectedPartIds,
  onTogglePart,
  editingPartId,
  onEditingPartIdChange,
  onOpenSpec,
  onPartBodyChange,
}: {
  index: number;
  section: PromptLayerSection;
  source: string;
  onSourceChange: (next: string) => void;
  allParts: PromptPartSummary[];
  selectedPartIds: string[];
  onTogglePart: (partId: string) => void;
  editingPartId: string | null;
  onEditingPartIdChange: (partId: string | null) => void;
  onOpenSpec: (id: string) => void;
  onPartBodyChange: (part: PromptPartSummary, body: string) => void;
}) {
  return (
    <section className="rounded-md border bg-background">
      <div className="flex flex-wrap items-center justify-between gap-2 border-b px-3 py-2">
        <div className="flex min-w-0 items-center gap-2">
          <span className="font-mono text-xs tabular-nums text-muted-foreground">{index.toString().padStart(2, '0')}</span>
          <h3 className="truncate text-sm font-semibold">{section.title}</h3>
        </div>
        <PromptPartPicker
          sectionTitle={section.title}
          allParts={allParts}
          selectedPartIds={selectedPartIds}
          onTogglePart={onTogglePart}
        />
      </div>
      <div className="flex flex-col gap-3 p-3">
        {section.blocks.map((block, blockIndex) =>
          block.kind === 'spec' ? (
            <SpecLayerBlock
              key={`${block.kind}-${block.specId}-${block.title}-${blockIndex}`}
              block={block}
              source={source}
              onSourceChange={onSourceChange}
              onOpenSpec={onOpenSpec}
            />
          ) : (
            <PartLayerBlock
              key={`${block.kind}-${block.partId}-${block.title}-${blockIndex}`}
              block={block}
              part={allParts.find((part) => part.id === block.partId) ?? null}
              editing={editingPartId === block.partId}
              onEditingChange={(editing) => onEditingPartIdChange(editing ? block.partId : null)}
              onRemove={() => onTogglePart(block.partId)}
              onPartBodyChange={onPartBodyChange}
            />
          ),
        )}
      </div>
    </section>
  );
}

function SpecLayerBlock({
  block,
  source,
  onSourceChange,
  onOpenSpec,
}: {
  block: Extract<PromptLayerBlock, { kind: 'spec' }>;
  source: string;
  onSourceChange: (next: string) => void;
  onOpenSpec: (id: string) => void;
}) {
  const inherited = block.origin === 'inherited';
  return (
    <div
      className={cn(
        'rounded-md border',
        inherited
          ? 'border-dashed bg-muted/35'
          : 'border-border bg-card',
      )}
    >
      <div className="flex flex-wrap items-center justify-between gap-2 border-b px-3 py-2">
        <div className="flex min-w-0 items-center gap-2">
          <Badge variant={inherited ? 'outline' : 'default'}>
            {inherited ? 'inherited' : 'own'}
          </Badge>
          <span className="truncate font-mono text-xs text-muted-foreground">{block.specId}</span>
          {block.merge === 'append' ? <Badge variant="secondary">append</Badge> : null}
        </div>
        {inherited ? (
          <Button type="button" variant="outline" size="sm" onClick={() => onOpenSpec(block.specId)}>
            <Pencil />
            Edit parent
          </Button>
        ) : null}
      </div>
      <div className="p-3">
        {inherited ? (
          <pre className="whitespace-pre-wrap rounded-md border bg-background/60 p-3 font-mono text-[13px] leading-relaxed text-muted-foreground">
            {block.body || 'Empty'}
          </pre>
        ) : (
          <Textarea
            value={block.body}
            onChange={(event) => onSourceChange(updateOrgNodeBody(source, block.title, event.target.value))}
            spellCheck={false}
            className="min-h-36 resize-y font-mono text-[13px] leading-relaxed"
          />
        )}
      </div>
    </div>
  );
}

function PartLayerBlock({
  block,
  part,
  editing,
  onEditingChange,
  onRemove,
  onPartBodyChange,
}: {
  block: Extract<PromptLayerBlock, { kind: 'part' }>;
  part: PromptPartSummary | null;
  editing: boolean;
  onEditingChange: (editing: boolean) => void;
  onRemove: () => void;
  onPartBodyChange: (part: PromptPartSummary, body: string) => void;
}) {
  return (
    <div className="rounded-md border border-primary/35 bg-primary/5">
      <div className="flex flex-wrap items-center justify-between gap-2 border-b border-primary/20 px-3 py-2">
        <div className="flex min-w-0 items-center gap-2">
          <Badge variant="secondary">
            <Puzzle />
            part
          </Badge>
          <span className="truncate font-mono text-xs text-muted-foreground">{block.partId}</span>
          {block.dirty ? <Badge variant="outline">dirty</Badge> : null}
        </div>
        <div className="flex items-center gap-1.5">
          {part ? (
            <Button type="button" variant="outline" size="sm" onClick={() => onEditingChange(!editing)}>
              <Pencil />
              {editing ? 'Done' : 'Edit part'}
            </Button>
          ) : null}
          <Button type="button" variant="destructive" size="sm" onClick={onRemove}>
            <Trash2 />
            Remove
          </Button>
        </div>
      </div>
      <div className="p-3">
        {part && editing ? (
          <Textarea
            value={block.body}
            onChange={(event) => onPartBodyChange(part, event.target.value)}
            spellCheck={false}
            className="min-h-36 resize-y font-mono text-[13px] leading-relaxed"
          />
        ) : (
          <pre className="whitespace-pre-wrap rounded-md border border-primary/20 bg-background/70 p-3 font-mono text-[13px] leading-relaxed">
            {block.body || 'Empty prompt part'}
          </pre>
        )}
      </div>
    </div>
  );
}

function PromptPartPicker({
  sectionTitle,
  allParts,
  selectedPartIds,
  onTogglePart,
}: {
  sectionTitle: string;
  allParts: PromptPartSummary[];
  selectedPartIds: string[];
  onTogglePart: (partId: string) => void;
}) {
  const candidates = allParts.filter((part) => part.target_section === sectionTitle);
  const selectedForSection = candidates.filter((part) => selectedPartIds.includes(part.id));
  return (
    <Popover>
      <PopoverTrigger asChild>
        <Button type="button" variant="outline" size="sm">
          <Puzzle />
          Parts
          {selectedForSection.length > 0 ? <Badge variant="secondary">{selectedForSection.length}</Badge> : null}
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-96 p-2">
        <div className="mb-2 px-2 text-xs font-medium text-muted-foreground">
          Prompt parts targeting {sectionTitle}
        </div>
        {candidates.length === 0 ? (
          <div className="px-2 py-3 text-sm text-muted-foreground">No parts target this section.</div>
        ) : (
          <div className="flex max-h-72 flex-col gap-1 overflow-auto">
            {candidates.map((part) => {
              const checked = selectedPartIds.includes(part.id);
              return (
                <button
                  key={part.id}
                  type="button"
                  className="flex w-full items-start gap-2 rounded-md px-2 py-2 text-left text-sm hover:bg-muted"
                  onClick={() => onTogglePart(part.id)}
                >
                  <span
                    className={cn(
                      'mt-0.5 flex size-4 items-center justify-center rounded border',
                      checked ? 'border-primary bg-primary text-primary-foreground' : 'border-border',
                    )}
                  >
                    {checked ? <CheckCircle2 className="size-3" /> : null}
                  </span>
                  <span className="min-w-0">
                    <span className="block truncate font-mono text-xs">{part.id}</span>
                    <span className="line-clamp-2 text-xs text-muted-foreground">{part.preview}</span>
                  </span>
                </button>
              );
            })}
          </div>
        )}
      </PopoverContent>
    </Popover>
  );
}
