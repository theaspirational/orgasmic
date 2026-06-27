import { useEffect, useMemo, useState, type ReactNode, type WheelEvent } from 'react';
import { X } from 'lucide-react';

import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Popover, PopoverAnchor, PopoverContent } from '@/components/ui/popover';
import { Skeleton } from '@/components/ui/skeleton';
import { Textarea } from '@/components/ui/textarea';
import { useRefreshBump } from '@/hooks/useRefreshBus';
import { fetchOrgNode, postOrgNodeEdit } from '@/lib/api';
import { OrgBody } from '@/lib/orgBody';
import type { NodeEditOp, OrgNodeDoc } from '@/lib/orgdoc/types';
import { HttpError } from '@/lib/transport';
import { useResource } from '@/lib/useResource';
import { cn } from '@/lib/utils';

import type { ChipSeparator, NodeDescriptor, NodeFieldDescriptor, SuggestSource } from './descriptor';

/** Resolves cross-node labels and autocomplete options for link-chips. Built by
 *  the host (NodeModal) from the loaded summaries, so the editor stays generic. */
export type NodeDirectory = {
  labelFor: (id: string) => string;
  suggestionsFor: (source: SuggestSource) => { value: string; label: string }[];
};

// Mutable working copy of a node's editable values. Edits land here; on save we
// diff against the baseline document to produce the minimal NodeEditOp list.
type Draft = {
  title: string;
  tags: string[];
  body: string;
  sections: Record<string, string>;
  properties: Record<string, string>;
};

function toDraft(doc: OrgNodeDoc): Draft {
  return {
    title: doc.title,
    tags: [...doc.tags],
    body: doc.body ?? '',
    sections: Object.fromEntries(doc.sections.map((s) => [s.title, s.body])),
    properties: Object.fromEntries(doc.properties.map((p) => [p.key, p.value])),
  };
}

function arraysEqual(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((value, index) => value === b[index]);
}

function splitTokens(value: string, separator: ChipSeparator): string[] {
  const parts = separator === 'comma' ? value.split(',') : value.split(/\s+/);
  return parts.map((part) => part.trim()).filter(Boolean);
}

function joinTokens(tokens: string[], separator: ChipSeparator): string {
  return tokens.join(separator === 'comma' ? ', ' : ' ');
}

/** The fields actually rendered/diffed: the descriptor's static fields, plus —
 *  when `dynamicSections` is set — a prose field for every document section not
 *  already bound by a static field, in document order. */
function effectiveFields(descriptor: NodeDescriptor, doc: OrgNodeDoc): NodeFieldDescriptor[] {
  const fields = [...descriptor.fields];
  if (descriptor.dynamicSections) {
    const bound = new Set(
      fields.flatMap((f) => (f.binding.kind === 'section' ? [f.binding.title] : [])),
    );
    for (const section of doc.sections) {
      if (!bound.has(section.title)) {
        fields.push({
          label: section.title,
          binding: { kind: 'section', title: section.title },
          editor: 'prose',
        });
      }
    }
  }
  return fields;
}

function computeOps(
  baseline: OrgNodeDoc,
  draft: Draft,
  descriptor: NodeDescriptor,
  fields: NodeFieldDescriptor[],
): NodeEditOp[] {
  const base = toDraft(baseline);
  const ops: NodeEditOp[] = [];
  if (descriptor.editableTitle && draft.title.trim() !== base.title.trim()) {
    ops.push({ op: 'set_title', title: draft.title.trim() });
  }
  for (const field of fields) {
    const binding = field.binding;
    if (binding.kind === 'tags') {
      if (!arraysEqual(draft.tags, base.tags)) ops.push({ op: 'set_tags', tags: draft.tags });
    } else if (binding.kind === 'body') {
      if (draft.body.trim() !== base.body.trim()) {
        ops.push({ op: 'set_body', body: draft.body.trim() });
      }
    } else if (binding.kind === 'section') {
      const next = (draft.sections[binding.title] ?? '').trim();
      const prev = (base.sections[binding.title] ?? '').trim();
      if (next !== prev) {
        ops.push(
          base.sections[binding.title] === undefined
            ? { op: 'add_section', title: binding.title, body: next }
            : { op: 'set_section_body', title: binding.title, body: next },
        );
      }
    } else if (binding.kind === 'property') {
      const next = (draft.properties[binding.key] ?? '').trim();
      const prev = (base.properties[binding.key] ?? '').trim();
      if (next !== prev) {
        ops.push(
          next === ''
            ? { op: 'remove_property', key: binding.key }
            : { op: 'set_property', key: binding.key, value: next },
        );
      }
    }
  }
  return ops;
}

function fieldValue(field: NodeFieldDescriptor, draft: Draft): string | string[] {
  const binding = field.binding;
  if (binding.kind === 'title') return draft.title;
  if (binding.kind === 'tags') return draft.tags;
  if (binding.kind === 'body') return draft.body;
  if (binding.kind === 'section') return draft.sections[binding.title] ?? '';
  return draft.properties[binding.key] ?? '';
}

function fieldIsEmpty(field: NodeFieldDescriptor, draft: Draft): boolean {
  const value = fieldValue(field, draft);
  if (Array.isArray(value)) return value.length === 0;
  if (field.editor === 'chips' && field.binding.kind === 'property') {
    return splitTokens(value, field.separator ?? 'space').length === 0;
  }
  return value.trim() === '';
}

export function NodeDocEditor({
  projectId,
  nodeId,
  descriptor,
  directory,
  onOpenNode,
  mode,
  apiKind,
}: {
  projectId: string;
  nodeId: string;
  descriptor: NodeDescriptor;
  directory: NodeDirectory;
  onOpenNode: (id: string) => void;
  mode: 'view' | 'edit';
  /** Explicit layer selector for the node API. Required for ids whose owning
   *  `.org` file the daemon can't infer from the id prefix (e.g. project). */
  apiKind?: string;
}) {
  const refreshBump = useRefreshBump();
  const resource = useResource(
    `org-node:${projectId}:${apiKind ?? 'auto'}:${nodeId}`,
    () => fetchOrgNode(nodeId, projectId, apiKind),
    { enabled: Boolean(nodeId) },
  );
  const doc = resource.data;

  const [baseline, setBaseline] = useState<OrgNodeDoc | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  // (Re)initialize the draft whenever the document's identity or version
  // changes — initial load, or an external/post-409 reload.
  useEffect(() => {
    if (!doc) return;
    setBaseline(doc);
    setDraft(toDraft(doc));
    setSaveError(null);
  }, [doc?.id, doc?.source.base_version]);

  const fields = useMemo(
    () => (baseline ? effectiveFields(descriptor, baseline) : descriptor.fields),
    [baseline, descriptor],
  );
  const ops = useMemo(
    () => (baseline && draft ? computeOps(baseline, draft, descriptor, fields) : []),
    [baseline, draft, descriptor, fields],
  );
  const editing = mode === 'edit';

  useEffect(() => {
    if (!editing && baseline) {
      setDraft(toDraft(baseline));
      setSaveError(null);
    }
  }, [baseline, editing]);

  if (resource.loading && !doc) {
    return (
      <div className="flex flex-col gap-3">
        <Skeleton className="h-6 w-40" />
        <Skeleton className="h-40" />
      </div>
    );
  }
  if (resource.error && !doc) {
    return (
      <p className="text-sm text-destructive">
        {resource.error instanceof Error ? resource.error.message : String(resource.error)}
      </p>
    );
  }
  if (!draft || !baseline) return null;

  const setBody = (body: string) => setDraft((prev) => (prev ? { ...prev, body } : prev));
  const setSection = (title: string, body: string) =>
    setDraft((prev) => (prev ? { ...prev, sections: { ...prev.sections, [title]: body } } : prev));
  const setProperty = (key: string, value: string) =>
    setDraft((prev) => (prev ? { ...prev, properties: { ...prev.properties, [key]: value } } : prev));
  const setTitle = (title: string) => setDraft((prev) => (prev ? { ...prev, title } : prev));
  const setTags = (tags: string[]) => setDraft((prev) => (prev ? { ...prev, tags } : prev));

  async function onSave() {
    if (!baseline || !draft || ops.length === 0) return;
    setSaving(true);
    setSaveError(null);
    setNotice(null);
    try {
      const updated = await postOrgNodeEdit(
        nodeId,
        { baseVersion: baseline.source.base_version, ops },
        projectId,
        apiKind,
      );
      setBaseline(updated);
      setDraft(toDraft(updated));
      refreshBump();
    } catch (err) {
      if (err instanceof HttpError && err.status === 409) {
        setNotice('This node changed on disk — reloaded the latest version.');
        await resource.refresh();
      } else {
        setSaveError(err instanceof Error ? err.message : String(err));
      }
    } finally {
      setSaving(false);
    }
  }

  function onCancel() {
    if (baseline) setDraft(toDraft(baseline));
    setSaveError(null);
  }

  return (
    <div className="flex flex-col gap-5">
      {notice ? <Banner tone="info">{notice}</Banner> : null}
      {saveError ? <Banner tone="error">{saveError}</Banner> : null}

      {descriptor.editableTitle && editing ? (
        <Input
          aria-label="Title"
          value={draft.title}
          onChange={(event) => setTitle(event.target.value)}
          className="h-9 text-base font-semibold"
          placeholder="Untitled"
        />
      ) : null}

      {fields.map((field) => {
        if (!editing && field.hideWhenEmpty && fieldIsEmpty(field, draft)) {
          return null;
        }
        const binding = field.binding;

        if (field.editor === 'prose') {
          const value =
            binding.kind === 'body'
              ? draft.body
              : binding.kind === 'section'
                ? draft.sections[binding.title] ?? ''
                : binding.kind === 'property'
                  ? draft.properties[binding.key] ?? ''
                  : '';
          const onChange = (next: string) => {
            if (binding.kind === 'body') setBody(next);
            else if (binding.kind === 'section') setSection(binding.title, next);
            else if (binding.kind === 'property') setProperty(binding.key, next);
          };
          return (
            <FieldShell key={field.label} label={field.label}>
              {editing ? (
                <Textarea value={value} onChange={(event) => onChange(event.target.value)} placeholder={field.placeholder} />
              ) : (
                <ProseValue value={value} placeholder={field.placeholder} />
              )}
            </FieldShell>
          );
        }

        if (field.editor === 'text' && binding.kind === 'property') {
          const value = draft.properties[binding.key] ?? '';
          return (
            <FieldShell key={field.label} label={field.label}>
              {editing ? (
                <Input
                  value={value}
                  onChange={(event) => setProperty(binding.key, event.target.value)}
                  placeholder={field.placeholder}
                />
              ) : (
                <TextValue value={value} placeholder={field.placeholder} />
              )}
            </FieldShell>
          );
        }

        if (field.editor === 'chips') {
          if (binding.kind === 'tags') {
            return (
              <FieldShell key={field.label} label={field.label}>
                {editing ? (
                  <ChipsField values={draft.tags} onChange={setTags} placeholder={field.placeholder} />
                ) : (
                  <ChipsValue values={draft.tags} placeholder={field.placeholder} />
                )}
              </FieldShell>
            );
          }
          if (binding.kind === 'property') {
            const separator = field.separator ?? 'space';
            const tokens = splitTokens(draft.properties[binding.key] ?? '', separator);
            return (
              <FieldShell key={field.label} label={field.label}>
                {editing ? (
                  <ChipsField
                    values={tokens}
                    onChange={(next) => setProperty(binding.key, joinTokens(next, separator))}
                    placeholder={field.placeholder}
                    labelFor={field.link ? directory.labelFor : undefined}
                    onOpen={field.link ? onOpenNode : undefined}
                    suggestions={field.suggest ? directory.suggestionsFor(field.suggest) : undefined}
                    maxItems={field.maxItems}
                  />
                ) : (
                  <ChipsValue
                    values={tokens}
                    placeholder={field.placeholder}
                    labelFor={field.link ? directory.labelFor : undefined}
                    onOpen={field.link ? onOpenNode : undefined}
                  />
                )}
              </FieldShell>
            );
          }
        }

        return null;
      })}

      {editing && ops.length > 0 ? (
        <div className="sticky bottom-0 z-10 flex items-center justify-end gap-2 border-t bg-background/95 py-3 backdrop-blur">
          <span className="mr-auto text-xs text-muted-foreground">
            {ops.length} unsaved change{ops.length === 1 ? '' : 's'}
          </span>
          <Button type="button" variant="ghost" size="sm" onClick={onCancel} disabled={saving}>
            Cancel
          </Button>
          <Button type="button" size="sm" onClick={onSave} disabled={saving}>
            {saving ? 'Saving…' : 'Save'}
          </Button>
        </div>
      ) : null}
    </div>
  );
}

function ProseValue({ value, placeholder }: { value: string; placeholder?: string }) {
  const trimmed = value.trim();
  if (!trimmed) return <EmptyValue placeholder={placeholder} />;
  return (
    <div className="rounded-md border border-transparent py-1 text-sm leading-6 text-foreground">
      <OrgBody source={trimmed} />
    </div>
  );
}

function TextValue({ value, placeholder }: { value: string; placeholder?: string }) {
  const trimmed = value.trim();
  if (!trimmed) return <EmptyValue placeholder={placeholder} />;
  return <p className="py-1 text-sm leading-6 text-foreground">{trimmed}</p>;
}

function EmptyValue({ placeholder }: { placeholder?: string }) {
  return <p className="py-1 text-sm italic leading-6 text-muted-foreground">{placeholder ?? 'Empty'}</p>;
}

function ChipsValue({
  values,
  placeholder,
  labelFor,
  onOpen,
}: {
  values: string[];
  placeholder?: string;
  labelFor?: (value: string) => string;
  onOpen?: (value: string) => void;
}) {
  if (values.length === 0) return <EmptyValue placeholder={placeholder} />;
  return (
    <div className="flex flex-wrap items-center gap-1.5 py-1">
      {values.map((value) => (
        <span
          key={value}
          className="inline-flex items-center gap-1 rounded-md border bg-muted/40 px-2 py-0.5 text-sm"
        >
          {onOpen ? (
            <button type="button" className="hover:underline" onClick={() => onOpen(value)}>
              {labelFor?.(value) ?? value}
            </button>
          ) : (
            <span>{labelFor?.(value) ?? value}</span>
          )}
        </span>
      ))}
    </div>
  );
}

function FieldShell({ label, children }: { label: string; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-2">
      <h3 className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">{label}</h3>
      {children}
    </section>
  );
}

function Banner({ tone, children }: { tone: 'info' | 'error'; children: ReactNode }) {
  return (
    <div
      className={cn(
        'rounded-md border px-3 py-2 text-sm',
        tone === 'error'
          ? 'border-destructive/40 text-destructive'
          : 'border-border bg-muted/30 text-muted-foreground',
      )}
    >
      {children}
    </div>
  );
}

function ChipsField({
  values,
  onChange,
  placeholder,
  labelFor,
  onOpen,
  suggestions,
  maxItems,
}: {
  values: string[];
  onChange: (next: string[]) => void;
  placeholder?: string;
  labelFor?: (value: string) => string;
  onOpen?: (value: string) => void;
  suggestions?: { value: string; label: string }[];
  maxItems?: number;
}) {
  const [text, setText] = useState('');
  const [focused, setFocused] = useState(false);
  const filteredSuggestions = useMemo(() => {
    if (!suggestions || !focused) return [];
    const query = text.trim().toLowerCase();
    return suggestions
      .filter((option) => !values.includes(option.value))
      .filter((option) => {
        if (!query) return true;
        return (
          option.value.toLowerCase().includes(query) ||
          option.label.toLowerCase().includes(query)
        );
      })
      .slice(0, 24);
  }, [focused, suggestions, text, values]);

  function commit(raw: string) {
    const token = raw.trim();
    if (token && !values.includes(token)) {
      onChange(maxItems === 1 ? [token] : [...values, token].slice(0, maxItems ?? undefined));
    }
    setText('');
  }
  function remove(token: string) {
    onChange(values.filter((value) => value !== token));
  }
  function onSuggestionsWheel(event: WheelEvent<HTMLDivElement>) {
    event.preventDefault();
    event.stopPropagation();
    event.currentTarget.scrollTop += event.deltaY;
  }

  const showSuggestions = filteredSuggestions.length > 0;

  return (
    <Popover open={showSuggestions}>
      <PopoverAnchor asChild>
        <div className="flex flex-wrap items-center gap-1.5 rounded-lg border border-input px-2 py-1.5">
          {values.map((value) => (
            <span
              key={value}
              className="inline-flex items-center gap-1 rounded-md border bg-muted/40 py-0.5 pl-2 pr-1 text-sm"
            >
              {onOpen ? (
                <button type="button" className="hover:underline" onClick={() => onOpen(value)}>
                  {labelFor?.(value) ?? value}
                </button>
              ) : (
                <span>{labelFor?.(value) ?? value}</span>
              )}
              <button
                type="button"
                aria-label={`Remove ${value}`}
                className="rounded p-0.5 text-muted-foreground hover:text-foreground"
                onClick={() => remove(value)}
              >
                <X className="size-3" />
              </button>
            </span>
          ))}
          <input
            value={text}
            onChange={(event) => {
              setText(event.target.value);
              setFocused(true);
            }}
            onClick={() => setFocused(true)}
            onFocus={() => setFocused(true)}
            onKeyDown={(event) => {
              if (event.key === 'Enter' || event.key === ',') {
                event.preventDefault();
                commit(text);
                setFocused(false);
              } else if (event.key === 'Backspace' && text === '' && values.length > 0) {
                remove(values[values.length - 1]);
              }
            }}
            onBlur={() => {
              commit(text);
              setFocused(false);
            }}
            placeholder={values.length === 0 ? placeholder : undefined}
            className="h-7 min-w-[8rem] flex-1 bg-transparent px-1 text-sm outline-none placeholder:text-muted-foreground"
          />
        </div>
      </PopoverAnchor>
      {showSuggestions ? (
        <PopoverContent
          align="start"
          side="bottom"
          sideOffset={4}
          collisionPadding={12}
          onOpenAutoFocus={(event) => event.preventDefault()}
          onCloseAutoFocus={(event) => event.preventDefault()}
          onWheelCapture={onSuggestionsWheel}
          className="max-h-[min(20rem,var(--radix-popover-content-available-height))] w-[var(--radix-popover-trigger-width)] overflow-y-auto overscroll-contain p-1"
        >
          {filteredSuggestions.map((option) => (
            <button
              key={option.value}
              type="button"
              className="flex w-full flex-col items-start gap-0.5 rounded-md px-2 py-1.5 text-left hover:bg-muted focus-visible:bg-muted focus-visible:outline-none"
              onMouseDown={(event) => {
                event.preventDefault();
                onChange(maxItems === 1 ? [option.value] : [...values, option.value].slice(0, maxItems ?? undefined));
                setText('');
                setFocused(false);
              }}
            >
              <span className="text-sm font-medium leading-tight">{option.label}</span>
              <span className="font-mono text-xs leading-tight text-muted-foreground">{option.value}</span>
            </button>
          ))}
        </PopoverContent>
      ) : null}
    </Popover>
  );
}
