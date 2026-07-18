import { useCallback, useEffect, useMemo, useRef, useState, type FormEvent } from 'react';
import { useNavigate, useParams, useSearch } from '@tanstack/react-router';
import { ArrowLeft, Check, Loader2, MessageSquarePlus, RefreshCw, Reply, RotateCcw, X } from 'lucide-react';
import { toast } from 'sonner';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Popover, PopoverAnchor, PopoverContent } from '@/components/ui/popover';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Textarea } from '@/components/ui/textarea';
import { useEventStream } from '@/hooks/useEventStream';
import { useMe } from '@/hooks/useMe';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import {
  fetchArtifact,
  isArtifactMissingError,
  postArtifactComment,
  regenerateArtifact,
  resolveArtifactComment,
} from '@/lib/api';
import { ArtifactRenderer } from '@/lib/artifacts/ArtifactRenderer';
import { findQuoteRange } from '@/lib/artifacts/anchorRanges';
import { ArtifactInteractionContext, type ArtifactInteraction } from '@/lib/artifacts/interaction';
import { parseQuestionAnchor } from '@/lib/artifacts/questionKey';
import { routeSearch } from '@/lib/searchState';
import type { ArtifactDetail, CommentRecord } from '@/lib/types';
import { useResource } from '@/lib/useResource';
import { cn } from '@/lib/utils';

import { ErrorPanel, PageHeader } from './Primitives';

const STATE_VARIANT: Record<string, 'outline' | 'default' | 'destructive' | 'secondary'> = {
  submitted: 'secondary',
  regenerating: 'default',
  failed: 'destructive',
};

const MAX_ANCHOR_LEN = 280;

type InlineSelection = {
  anchor: string;
  virtualElement: {
    contextElement?: Element;
    getBoundingClientRect: () => DOMRect;
  };
  focusOnOpen: boolean;
};

export function ArtifactView({ projectId }: { projectId: string }) {
  const navigate = useNavigate();
  const { artifactId } = useParams({ strict: false }) as { artifactId: string };
  const search = useSearch({ strict: false }) as { version?: number };
  const refresh = useRefreshToken();
  const { can } = useMe();
  const [regenerating, setRegenerating] = useState(false);
  const [regenerateDialogOpen, setRegenerateDialogOpen] = useState(false);
  // Last known live version (set from any successful fetch — `data.version`
  // always reflects the current/live version, never the archived version's
  // own number). Used to decide whether a version in the URL is archived
  // *before* this fetch resolves, without ever guessing "archived" when we
  // don't yet know better (see fetchArtifact call below).
  const currentVersionRef = useRef<number | undefined>(undefined);

  const canComment = can(projectId, 'artifacts.comment');
  const canGenerate = can(projectId, 'artifacts.generate');

  const artifact = useResource(
    `artifact:${projectId}:${artifactId}:${search.version ?? 'latest'}:${refresh}`,
    () => {
      const requestedVersion = search.version;
      const includeConsumed =
        typeof requestedVersion === 'number' &&
        typeof currentVersionRef.current === 'number' &&
        requestedVersion !== currentVersionRef.current;
      return fetchArtifact(artifactId, projectId, requestedVersion, includeConsumed);
    },
  );

  useEffect(() => {
    if (artifact.data) currentVersionRef.current = artifact.data.version;
  }, [artifact.data]);

  useEventStream(
    useCallback(
      (event) => {
        // Live: artifact.* events (state changes, new/resolved comments) refetch
        // this artifact's detail so threads and the regenerating banner update.
        if (event.topic === 'artifact') artifact.refresh();
      },
      // eslint-disable-next-line react-hooks/exhaustive-deps
      [],
    ),
  );

  function setVersion(version: number | undefined) {
    void navigate({
      search: routeSearch((prev) => ({ ...prev, version })),
    });
  }

  async function regenerate(extraPrompt: string): Promise<boolean> {
    setRegenerating(true);
    try {
      const summary = artifact.data;
      const payload: Parameters<typeof regenerateArtifact>[1] = {};
      if (extraPrompt) payload.extraPrompt = extraPrompt;
      if (summary?.launch_mode && summary.launch_harness) {
        payload.mode = summary.launch_mode;
        payload.harness = summary.launch_harness;
        if (summary.launch_harness_args?.length) payload.harness_args = summary.launch_harness_args;
        if (summary.launch_model) payload.model = summary.launch_model;
        if (summary.launch_effort) payload.effort = summary.launch_effort;
      }
      await regenerateArtifact(artifactId, payload, projectId);
      toast.success('Regenerate started');
      setRegenerateDialogOpen(false);
      artifact.refresh();
      return true;
    } catch (err) {
      toast.error(err instanceof Error ? err.message : String(err));
      return false;
    } finally {
      setRegenerating(false);
    }
  }

  if (artifact.error) {
    if (isArtifactMissingError(artifact.error)) {
      return (
        <div className="flex flex-col items-start gap-4">
          <PageHeader title="Artifact not found" />
          <p className="text-sm text-muted-foreground">
            No artifact with id{' '}
            <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-xs">{artifactId}</code>{' '}
            exists in this project. It may have been renamed or removed.
          </p>
          <Button
            variant="outline"
            onClick={() =>
              void navigate({ to: '/projects/$projectId/artifacts', params: { projectId } })
            }
          >
            <ArrowLeft className="size-4" /> Back to Artifacts
          </Button>
        </div>
      );
    }
    return <ErrorPanel error={artifact.error} />;
  }
  if (!artifact.data) {
    return (
      <div className="flex items-center gap-2 text-sm text-muted-foreground">
        <Loader2 className="size-4 animate-spin" /> Loading artifact…
      </div>
    );
  }

  const data = artifact.data;
  const isArchivedVersion = typeof search.version === 'number' && search.version !== data.version;

  return (
    <div className="flex flex-col gap-4">
      <PageHeader
        title={data.title || data.id}
        description={data.prompt}
        actions={
          <div className="flex items-center gap-2">
            <Select
              value={String(search.version ?? data.version)}
              onValueChange={(value) => setVersion(value === String(data.version) ? undefined : Number(value))}
            >
              <SelectTrigger className="w-28" size="sm">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {Array.from({ length: data.version }, (_, index) => data.version - index).map((version) => (
                  <SelectItem key={version} value={String(version)}>
                    v{version}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            {canGenerate ? (
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={regenerating || isArchivedVersion}
                onClick={() => setRegenerateDialogOpen(true)}
              >
                {regenerating ? <Loader2 className="size-3.5 animate-spin" /> : <RefreshCw className="size-3.5" />}
                Regenerate
              </Button>
            ) : null}
          </div>
        }
      />
      {canGenerate ? (
        <RegenerateArtifactDialog
          open={regenerateDialogOpen}
          onOpenChange={setRegenerateDialogOpen}
          submitting={regenerating}
          onSubmit={regenerate}
        />
      ) : null}
      <div className="flex flex-wrap items-center gap-1.5">
        <Badge variant={STATE_VARIANT[data.state] ?? 'outline'} className={data.state === 'regenerating' ? 'animate-pulse' : undefined}>
          {data.state}
        </Badge>
        {isArchivedVersion ? <Badge variant="outline">archived version {search.version}</Badge> : null}
        {data.subject_nodes.length === 0 ? <Badge variant="secondary">prompt-only</Badge> : null}
      </div>
      <ArtifactComments
        data={data}
        projectId={projectId}
        artifactId={artifactId}
        canComment={canComment}
        isArchivedVersion={isArchivedVersion}
        onChanged={() => artifact.refresh()}
      />
    </div>
  );
}

function RegenerateArtifactDialog({
  open,
  onOpenChange,
  submitting,
  onSubmit,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  submitting: boolean;
  /** Trimmed extra-prompt text; empty string means "none". Resolves true when
   *  the regeneration was accepted, so the dialog knows to clear its draft. */
  onSubmit: (extraPrompt: string) => Promise<boolean>;
}) {
  // The draft survives close/reopen — an escaped dialog must not discard typed
  // steering text. It clears only after a successful submit.
  const [extraPrompt, setExtraPrompt] = useState('');

  function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    void onSubmit(extraPrompt.trim()).then((ok) => {
      if (ok) setExtraPrompt('');
    });
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent showCloseButton className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Regenerate artifact</DialogTitle>
          <DialogDescription>
            Archives the current version and launches a new run with the prior artifact and
            current-version comments as context.
          </DialogDescription>
        </DialogHeader>
        <form className="flex flex-col gap-4" onSubmit={handleSubmit}>
          <label className="flex flex-col gap-1.5 text-sm">
            <span className="font-medium">Extra prompt (optional)</span>
            <Textarea
              autoFocus
              rows={4}
              value={extraPrompt}
              disabled={submitting}
              onChange={(event) => setExtraPrompt(event.target.value)}
              placeholder="Anything extra to steer this regeneration…"
            />
          </label>
          <DialogFooter className="mx-0 mb-0 mt-2 rounded-md">
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={submitting}>
              Cancel
            </Button>
            <Button type="submit" disabled={submitting}>
              {submitting ? <Loader2 className="size-3.5 animate-spin" /> : null}
              {submitting ? 'Regenerating…' : 'Regenerate'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** A comment's `anchor` is plain selection text (as opposed to a `{}` /
 * structured JSON anchor) when it is non-empty and does not look like JSON.
 * Only these get a persistent text highlight and text-scroll navigation. */
function isPlainTextAnchor(anchor: string | undefined | null): anchor is string {
  if (!anchor) return false;
  const trimmed = anchor.trim();
  return trimmed.length > 0 && trimmed !== '{}' && trimmed[0] !== '{';
}

type CommentThreads = {
  roots: CommentRecord[];
  repliesByRoot: Map<string, CommentRecord[]>;
};

/** Group comments into root threads. A root is a top-level comment (empty or
 * dangling `reply_to`); every reply is flattened under its top-most root
 * ancestor, preserving file (chronological) order. */
function buildThreads(comments: CommentRecord[]): CommentThreads {
  const byCid = new Map(comments.map((c) => [c.cid, c]));
  const rootCidOf = (comment: CommentRecord): string => {
    let current = comment;
    const guard = new Set<string>();
    while (current.reply_to && byCid.has(current.reply_to) && !guard.has(current.cid)) {
      guard.add(current.cid);
      current = byCid.get(current.reply_to) as CommentRecord;
    }
    return current.cid;
  };
  const roots: CommentRecord[] = [];
  const repliesByRoot = new Map<string, CommentRecord[]>();
  for (const comment of comments) {
    const isRoot = !comment.reply_to || !byCid.has(comment.reply_to);
    if (isRoot) {
      roots.push(comment);
    } else {
      const rootCid = rootCidOf(comment);
      const list = repliesByRoot.get(rootCid) ?? [];
      list.push(comment);
      repliesByRoot.set(rootCid, list);
    }
  }
  return { roots, repliesByRoot };
}

export function ArtifactComments({
  data,
  projectId,
  artifactId,
  canComment,
  isArchivedVersion = false,
  onChanged,
}: {
  data: ArtifactDetail;
  projectId: string;
  artifactId: string;
  canComment: boolean;
  isArchivedVersion?: boolean;
  onChanged: () => void;
}) {
  const bodyRef = useRef<HTMLDivElement | null>(null);
  const composerRef = useRef<HTMLTextAreaElement | null>(null);
  const inlineComposerRef = useRef<HTMLTextAreaElement | null>(null);
  const selectionTimerRef = useRef<number | null>(null);
  // Latest inlineSelection/inlinePosting mirrored into refs so captureSelection
  // can read them without becoming a dependency (keeps the selectionchange
  // listener from re-subscribing on every selection update).
  const inlineSelectionRef = useRef<InlineSelection | null>(null);
  const inlinePostingRef = useRef(false);
  // Stored anchor ranges for highlight->comment hit-testing (cid + live Range).
  const anchorHitsRef = useRef<Array<{ cid: string; range: Range }>>([]);
  const [message, setMessage] = useState('');
  const [inlineMessage, setInlineMessage] = useState('');
  const [inlineSelection, setInlineSelection] = useState<InlineSelection | null>(null);
  const [posting, setPosting] = useState(false);
  const [inlinePosting, setInlinePosting] = useState(false);
  const [resolvingCid, setResolvingCid] = useState<string | null>(null);
  const [flashedCid, setFlashedCid] = useState<string | null>(null);

  const isRegenerating = data.state === 'regenerating';
  const comments = useMemo(() => data.comments ?? [], [data.comments]);
  const threads = useMemo(() => buildThreads(comments), [comments]);

  const interaction = useMemo<ArtifactInteraction>(
    () => ({
      canAnswer: canComment && !isRegenerating && !isArchivedVersion,
      comments,
      async submitAnswer({ questionKey, prompt, message: answerMessage }) {
        try {
          await postArtifactComment(
            artifactId,
            {
              message: answerMessage,
              anchor: JSON.stringify({ kind: 'question', key: questionKey, prompt: prompt.slice(0, 200) }),
            },
            projectId,
          );
          onChanged();
        } catch (err) {
          toast.error(err instanceof Error ? err.message : String(err));
          throw err;
        }
      },
      async agree({ cid }) {
        try {
          await postArtifactComment(artifactId, { message: 'Agree', reply_to: cid }, projectId);
          onChanged();
        } catch (err) {
          toast.error(err instanceof Error ? err.message : String(err));
          throw err;
        }
      },
    }),
    [canComment, isRegenerating, isArchivedVersion, comments, artifactId, projectId, onChanged],
  );

  // Persistent anchor highlights via the CSS Custom Highlight API — no DOM
  // mutation, so React re-renders (interactive QuestionForm, Tabs) are never
  // corrupted. Recompute on comment/content change and on a debounced
  // MutationObserver (tab switches re-mount text nodes). Silently skipped where
  // the API is absent (e.g. jsdom).
  useEffect(() => {
    const body = bodyRef.current;
    if (!body || !('highlights' in CSS)) return;

    let frame = 0;
    let debounce = 0;
    const recompute = () => {
      const hits: Array<{ cid: string; range: Range }> = [];
      const ranges: Range[] = [];
      for (const comment of comments) {
        if (!isPlainTextAnchor(comment.anchor)) continue;
        const range = findQuoteRange(body, comment.anchor);
        if (range) {
          hits.push({ cid: comment.cid, range });
          ranges.push(range);
        }
      }
      anchorHitsRef.current = hits;
      if (ranges.length === 0) {
        CSS.highlights.delete('orgasmic-comment-anchors');
      } else {
        CSS.highlights.set('orgasmic-comment-anchors', new Highlight(...ranges));
      }
    };

    frame = requestAnimationFrame(recompute);
    const observer = new MutationObserver(() => {
      window.clearTimeout(debounce);
      debounce = window.setTimeout(recompute, 120);
    });
    observer.observe(body, { childList: true, subtree: true, characterData: true });

    return () => {
      cancelAnimationFrame(frame);
      window.clearTimeout(debounce);
      observer.disconnect();
      anchorHitsRef.current = [];
      CSS.highlights.delete('orgasmic-comment-anchors');
    };
  }, [comments, data.content]);

  const flashComment = useCallback((cid: string) => {
    const el = document.getElementById(`comment-${cid}`);
    el?.scrollIntoView({ behavior: 'smooth', block: 'center' });
    setFlashedCid(cid);
    window.setTimeout(() => setFlashedCid((prev) => (prev === cid ? null : prev)), 1500);
  }, []);

  const flashElement = useCallback((el: Element) => {
    el.classList.add('orgasmic-flash');
    window.setTimeout(() => el.classList.remove('orgasmic-flash'), 1500);
  }, []);

  // Highlight -> comment: a plain click (collapsed selection) that lands inside
  // a stored anchor range scrolls that comment's card into view and flashes it.
  const handleBodyClick = useCallback(
    (event: React.MouseEvent) => {
      if (!window.getSelection()?.isCollapsed) return;
      const hits = anchorHitsRef.current;
      if (hits.length === 0) return;
      const { clientX: x, clientY: y } = event;
      for (const { cid, range } of hits) {
        for (const rect of Array.from(range.getClientRects())) {
          if (x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom) {
            flashComment(cid);
            return;
          }
        }
      }
    },
    [flashComment],
  );

  // Quote -> text: question answers scroll to their question element; plain
  // anchors scroll to the matched text and flash it via a short-lived Highlight.
  const navigateToAnchor = useCallback(
    (comment: CommentRecord) => {
      const body = bodyRef.current;
      if (!body) return;
      const question = parseQuestionAnchor(comment.anchor);
      if (question) {
        const target = body.querySelector(`[data-question-key="${question.key}"]`);
        if (target) {
          target.scrollIntoView({ behavior: 'smooth', block: 'center' });
          flashElement(target);
        }
        return;
      }
      if (!isPlainTextAnchor(comment.anchor)) return;
      const range = findQuoteRange(body, comment.anchor);
      if (!range) return;
      const startEl =
        range.startContainer.nodeType === Node.ELEMENT_NODE
          ? (range.startContainer as Element)
          : range.startContainer.parentElement;
      startEl?.scrollIntoView({ behavior: 'smooth', block: 'center' });
      if ('highlights' in CSS) {
        try {
          CSS.highlights.set('orgasmic-anchor-active', new Highlight(range));
          window.setTimeout(() => CSS.highlights.delete('orgasmic-anchor-active'), 1500);
        } catch {
          // Range detached mid-navigation — non-fatal, skip the flash.
        }
      }
    },
    [flashElement],
  );

  const captureSelection = useCallback((focusOnOpen: boolean) => {
    if (!canComment || isRegenerating || inlinePostingRef.current) return;
    const selection = window.getSelection();
    if (!selection || selection.isCollapsed || !bodyRef.current) {
      // A collapsed or out-of-body selection (e.g. tapping into the composer
      // textarea) must preserve the currently captured quote, not clear it.
      return;
    }
    // Collapse internal whitespace/newlines to single spaces before storing:
    // an embedded newline would corrupt the single-line `:ANCHOR:` org
    // property, and normalization makes the anchor-range matcher reliable.
    const text = selection.toString().replace(/\s+/g, ' ').trim();
    const range = selection.rangeCount > 0 ? selection.getRangeAt(0).cloneRange() : null;
    if (!text || !range) return;
    const withinBody = (node: Node | null) => Boolean(node && bodyRef.current?.contains(node));
    if (
      !withinBody(selection.anchorNode) ||
      !withinBody(selection.focusNode) ||
      !withinBody(range.commonAncestorContainer)
    ) {
      return;
    }
    const anchor = text.slice(0, MAX_ANCHOR_LEN);
    const prev = inlineSelectionRef.current;
    // selectionchange fires very frequently; skip when the quote is unchanged so
    // we avoid pointless re-renders and popover reposition churn.
    if (prev && prev.anchor === anchor) return;
    setInlineSelection({
      anchor,
      virtualElement: {
        contextElement: bodyRef.current,
        getBoundingClientRect: () => range.getBoundingClientRect(),
      },
      focusOnOpen,
    });
    // Only clear the draft when the composer is freshly opening; re-capturing an
    // already-open composer (e.g. extending the selection) must keep the draft.
    if (!prev) setInlineMessage('');
  }, [canComment, isRegenerating]);

  const scheduleSelectionCapture = useCallback(
    (focusOnOpen: boolean) => {
      if (selectionTimerRef.current !== null) window.clearTimeout(selectionTimerRef.current);
      selectionTimerRef.current = window.setTimeout(() => {
        selectionTimerRef.current = null;
        captureSelection(focusOnOpen);
      }, 0);
    },
    [captureSelection],
  );

  useEffect(() => {
    inlineSelectionRef.current = inlineSelection;
  }, [inlineSelection]);

  useEffect(() => {
    inlinePostingRef.current = inlinePosting;
  }, [inlinePosting]);

  useEffect(() => {
    if (!inlineSelection?.focusOnOpen) return;
    inlineComposerRef.current?.focus();
  }, [inlineSelection]);

  // Native selection-handle drags (mobile) are browser chrome: they never
  // dispatch pointerup/touchend to the page, only document-level
  // `selectionchange`. Keep the captured anchor in sync via a debounced
  // selectionchange listener so extending the selection updates the quote
  // without a pointer event. focusOnOpen is always false here — never steal
  // focus mid-drag (would pop the mobile keyboard). captureSelection's existing
  // guards preserve the quote on a collapsed/out-of-body selection.
  useEffect(() => {
    if (!canComment || isRegenerating) return;
    const handler = () => {
      if (selectionTimerRef.current !== null) window.clearTimeout(selectionTimerRef.current);
      selectionTimerRef.current = window.setTimeout(() => {
        selectionTimerRef.current = null;
        captureSelection(false);
      }, 150);
    };
    document.addEventListener('selectionchange', handler);
    return () => {
      document.removeEventListener('selectionchange', handler);
      if (selectionTimerRef.current !== null) {
        window.clearTimeout(selectionTimerRef.current);
        selectionTimerRef.current = null;
      }
    };
  }, [canComment, isRegenerating, captureSelection]);

  useEffect(() => {
    if (isRegenerating || !canComment) {
      setInlineSelection(null);
      setInlineMessage('');
    }
  }, [canComment, isRegenerating]);

  useEffect(
    () => () => {
      if (selectionTimerRef.current !== null) window.clearTimeout(selectionTimerRef.current);
    },
    [],
  );

  async function submitComment() {
    const trimmed = message.trim();
    if (!trimmed || posting || isRegenerating || !canComment) return;
    setPosting(true);
    try {
      await postArtifactComment(
        artifactId,
        { message: trimmed },
        projectId,
      );
      setMessage('');
      onChanged();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : String(err));
    } finally {
      setPosting(false);
    }
  }

  async function submitReply(parentCid: string, replyMessage: string): Promise<boolean> {
    const trimmed = replyMessage.trim();
    if (!trimmed || isRegenerating || !canComment) return false;
    try {
      await postArtifactComment(artifactId, { message: trimmed, reply_to: parentCid }, projectId);
      onChanged();
      return true;
    } catch (err) {
      toast.error(err instanceof Error ? err.message : String(err));
      return false;
    }
  }

  async function submitInlineComment() {
    const trimmed = inlineMessage.trim();
    if (!trimmed || inlinePosting || isRegenerating || !canComment || !inlineSelection) return;
    setInlinePosting(true);
    try {
      await postArtifactComment(
        artifactId,
        { message: trimmed, anchor: inlineSelection.anchor },
        projectId,
      );
      setInlineMessage('');
      setInlineSelection(null);
      window.getSelection()?.removeAllRanges();
      onChanged();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : String(err));
    } finally {
      setInlinePosting(false);
    }
  }

  async function toggleResolved(comment: CommentRecord) {
    setResolvingCid(comment.cid);
    try {
      await resolveArtifactComment(artifactId, comment.cid, !comment.resolved, projectId);
      onChanged();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : String(err));
    } finally {
      setResolvingCid(null);
    }
  }

  return (
    <div className="grid gap-4 lg:grid-cols-[minmax(0,1fr)_22rem]">
      <div
        ref={bodyRef}
        className="rounded-xl border bg-card/40 p-4"
        onPointerUp={(event) => scheduleSelectionCapture(event.pointerType !== 'touch')}
        onKeyUp={() => scheduleSelectionCapture(true)}
        onTouchEnd={() => scheduleSelectionCapture(false)}
        onClick={handleBodyClick}
      >
        <ArtifactInteractionContext.Provider value={interaction}>
          <ArtifactRenderer content={data.content} />
        </ArtifactInteractionContext.Provider>
      </div>
      <Popover open={Boolean(inlineSelection)} onOpenChange={(open) => {
        if (!open) {
          setInlineSelection(null);
          setInlineMessage('');
        }
      }}>
        {inlineSelection ? <PopoverAnchor virtualRef={{ current: inlineSelection.virtualElement }} /> : null}
        <PopoverContent
          align="center"
          side="bottom"
          sideOffset={8}
          collisionPadding={12}
          className="w-[min(22rem,calc(100vw-2rem))] p-3"
          onOpenAutoFocus={(event) => event.preventDefault()}
          onCloseAutoFocus={(event) => event.preventDefault()}
        >
          {inlineSelection ? (
            <form
              className="flex flex-col gap-2"
              aria-label="Comment on selected text"
              onSubmit={(event) => {
                event.preventDefault();
                void submitInlineComment();
              }}
            >
              <div className="flex items-start gap-2 rounded-md border bg-muted/40 px-2.5 py-1.5">
                <blockquote className="min-w-0 flex-1 border-l-2 border-primary/40 pl-2 text-xs italic text-muted-foreground">
                  {inlineSelection.anchor}
                </blockquote>
                <button
                  type="button"
                  className="text-muted-foreground hover:text-foreground"
                  aria-label="Close selection comment composer"
                  onClick={() => {
                    setInlineSelection(null);
                    setInlineMessage('');
                  }}
                >
                  <X className="size-3.5" />
                </button>
              </div>
              <Textarea
                ref={inlineComposerRef}
                rows={3}
                value={inlineMessage}
                disabled={inlinePosting || isRegenerating}
                aria-label="Selection comment"
                placeholder="Comment on the selected text…"
                onChange={(event) => setInlineMessage(event.target.value)}
              />
              <div className="flex justify-end gap-2">
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  disabled={inlinePosting}
                  onClick={() => {
                    setInlineSelection(null);
                    setInlineMessage('');
                  }}
                >
                  Cancel
                </Button>
                <Button type="submit" size="sm" disabled={!inlineMessage.trim() || inlinePosting || isRegenerating}>
                  {inlinePosting ? <Loader2 className="size-3.5 animate-spin" /> : null}
                  {inlinePosting ? 'Posting…' : 'Comment'}
                </Button>
              </div>
            </form>
          ) : null}
        </PopoverContent>
      </Popover>

      <aside className="flex flex-col gap-3">
        <div className="flex items-center justify-between">
          <h2 className="text-sm font-semibold">Comments</h2>
          <span className="text-xs text-muted-foreground">
            {comments.length === 0
              ? 'None yet'
              : `${comments.length} comment${comments.length === 1 ? '' : 's'}`}
          </span>
        </div>

        <div className="flex flex-col gap-2">
          {comments.length === 0 ? (
            <p className="text-xs text-muted-foreground">
              No comments yet.{canComment ? ' Select text in the artifact to pin one.' : ''}
            </p>
          ) : (
            threads.roots.map((root) => (
              <CommentCard
                key={root.cid}
                comment={root}
                replies={threads.repliesByRoot.get(root.cid) ?? []}
                canComment={canComment}
                resolving={resolvingCid === root.cid}
                onToggleResolved={() => void toggleResolved(root)}
                onReply={canComment && !isRegenerating ? submitReply : undefined}
                onQuoteNavigate={navigateToAnchor}
                flashedCid={flashedCid}
              />
            ))
          )}
        </div>

        {canComment ? (
          <form
            className="flex flex-col gap-2 rounded-lg border bg-card/40 p-3"
            onSubmit={(event) => {
              event.preventDefault();
              void submitComment();
            }}
          >
            {isRegenerating ? (
              <p
                className="rounded-md border border-amber-500/30 bg-amber-500/10 px-2.5 py-1.5 text-xs text-amber-700 dark:text-amber-400"
                role="status"
              >
                This artifact is regenerating — commenting is paused until it settles.
              </p>
            ) : null}
            <div className="flex items-center justify-between gap-2">
              <span className="text-xs font-medium">Add a comment</span>
              <MessageSquarePlus className="size-3.5 text-muted-foreground" aria-hidden="true" />
            </div>
            <Textarea
              ref={composerRef}
              rows={3}
              value={message}
              disabled={isRegenerating || posting}
              placeholder="Leave a comment…"
              onChange={(event) => setMessage(event.target.value)}
            />
            <div className="flex justify-end">
              <Button type="submit" size="sm" disabled={!message.trim() || posting || isRegenerating}>
                {posting ? <Loader2 className="size-3.5 animate-spin" /> : null}
                {posting ? 'Posting…' : 'Comment'}
              </Button>
            </div>
          </form>
        ) : null}
      </aside>
    </div>
  );
}

export function CommentCard({
  comment,
  replies = [],
  canComment,
  resolving,
  onToggleResolved,
  onReply,
  onQuoteNavigate,
  flashedCid,
  isReply = false,
}: {
  comment: CommentRecord;
  replies?: CommentRecord[];
  canComment: boolean;
  resolving: boolean;
  onToggleResolved?: () => void;
  /** Post a reply to `parentCid`; resolves true on success. Absent = no reply
   * affordance. */
  onReply?: (parentCid: string, message: string) => Promise<boolean>;
  /** Navigate from this comment's quote to the anchored text or question. */
  onQuoteNavigate?: (comment: CommentRecord) => void;
  /** CID currently flashing (highlight->comment navigation target). */
  flashedCid?: string | null;
  isReply?: boolean;
}) {
  const [replyOpen, setReplyOpen] = useState(false);
  const [replyText, setReplyText] = useState('');
  const [replyPosting, setReplyPosting] = useState(false);

  const question = parseQuestionAnchor(comment.anchor);
  const quoteText = question ? question.prompt : isPlainTextAnchor(comment.anchor) ? comment.anchor : null;

  async function sendReply() {
    if (!onReply || !replyText.trim() || replyPosting) return;
    setReplyPosting(true);
    try {
      const ok = await onReply(comment.cid, replyText);
      if (ok) {
        setReplyText('');
        setReplyOpen(false);
      }
    } finally {
      setReplyPosting(false);
    }
  }

  return (
    <div
      id={`comment-${comment.cid}`}
      className={cn(
        'flex flex-col gap-1.5 rounded-lg border bg-card/40 p-3',
        comment.resolved && 'opacity-70',
        flashedCid === comment.cid && 'orgasmic-flash',
      )}
    >
      <div className="flex items-center gap-2">
        <span className="truncate text-xs font-semibold">{comment.author}</span>
        <Badge variant="outline" className="h-4 px-1 font-mono text-[10px]">
          v{comment.version}
        </Badge>
        {question ? (
          <Badge variant="secondary" className="h-4 px-1.5 text-[10px]">
            answer
          </Badge>
        ) : null}
        {comment.resolved ? (
          <Badge variant="secondary" className="h-4 px-1.5 text-[10px]">
            resolved
          </Badge>
        ) : null}
        {comment.consumed ? (
          <Badge variant="outline" className="h-4 px-1.5 text-[10px]">
            consumed
          </Badge>
        ) : null}
      </div>
      {quoteText ? (
        onQuoteNavigate ? (
          <button
            type="button"
            className="truncate border-l-2 border-primary/40 pl-2 text-left text-xs italic text-muted-foreground hover:text-foreground"
            title={question ? 'Jump to the question' : 'Jump to the highlighted text'}
            onClick={() => onQuoteNavigate(comment)}
          >
            {quoteText}
          </button>
        ) : (
          <blockquote className="truncate border-l-2 border-primary/40 pl-2 text-xs italic text-muted-foreground">
            {quoteText}
          </blockquote>
        )
      ) : null}
      <p className="whitespace-pre-wrap text-sm leading-snug">{comment.message}</p>
      {canComment ? (
        <div className="flex justify-end gap-1">
          {onReply ? (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="h-7 gap-1 px-2 text-xs"
              onClick={() => setReplyOpen((open) => !open)}
            >
              <Reply className="size-3.5" />
              Reply
            </Button>
          ) : null}
          {!isReply && onToggleResolved ? (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="h-7 gap-1 px-2 text-xs"
              disabled={resolving}
              onClick={onToggleResolved}
            >
              {resolving ? (
                <Loader2 className="size-3.5 animate-spin" />
              ) : comment.resolved ? (
                <RotateCcw className="size-3.5" />
              ) : (
                <Check className="size-3.5" />
              )}
              {comment.resolved ? 'Reopen' : 'Resolve'}
            </Button>
          ) : null}
        </div>
      ) : null}
      {replyOpen && onReply ? (
        <form
          className="flex flex-col gap-2 border-t pt-2"
          aria-label={`Reply to ${comment.author}`}
          onSubmit={(event) => {
            event.preventDefault();
            void sendReply();
          }}
        >
          <Textarea
            rows={2}
            value={replyText}
            disabled={replyPosting}
            aria-label="Reply"
            placeholder="Write a reply…"
            onChange={(event) => setReplyText(event.target.value)}
          />
          <div className="flex justify-end gap-2">
            <Button
              type="button"
              variant="outline"
              size="sm"
              disabled={replyPosting}
              onClick={() => {
                setReplyOpen(false);
                setReplyText('');
              }}
            >
              Cancel
            </Button>
            <Button type="submit" size="sm" disabled={!replyText.trim() || replyPosting}>
              {replyPosting ? <Loader2 className="size-3.5 animate-spin" /> : null}
              {replyPosting ? 'Replying…' : 'Reply'}
            </Button>
          </div>
        </form>
      ) : null}
      {replies.length > 0 ? (
        <div className="mt-1 flex flex-col gap-1.5 border-l-2 border-muted pl-2">
          {replies.map((reply) => (
            <CommentCard
              key={reply.cid}
              comment={reply}
              canComment={canComment}
              resolving={false}
              onReply={onReply}
              onQuoteNavigate={onQuoteNavigate}
              flashedCid={flashedCid}
              isReply
            />
          ))}
        </div>
      ) : null}
    </div>
  );
}
