import { useCallback, useEffect, useRef, useState, type FormEvent } from 'react';
import { useNavigate, useParams, useSearch } from '@tanstack/react-router';
import { ArrowLeft, Check, Loader2, MessageSquarePlus, RefreshCw, RotateCcw, X } from 'lucide-react';
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
      await regenerateArtifact(artifactId, extraPrompt ? { extraPrompt } : {}, projectId);
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

export function ArtifactComments({
  data,
  projectId,
  artifactId,
  canComment,
  onChanged,
}: {
  data: ArtifactDetail;
  projectId: string;
  artifactId: string;
  canComment: boolean;
  onChanged: () => void;
}) {
  const bodyRef = useRef<HTMLDivElement | null>(null);
  const composerRef = useRef<HTMLTextAreaElement | null>(null);
  const inlineComposerRef = useRef<HTMLTextAreaElement | null>(null);
  const selectionTimerRef = useRef<number | null>(null);
  const [message, setMessage] = useState('');
  const [inlineMessage, setInlineMessage] = useState('');
  const [inlineSelection, setInlineSelection] = useState<InlineSelection | null>(null);
  const [posting, setPosting] = useState(false);
  const [inlinePosting, setInlinePosting] = useState(false);
  const [resolvingCid, setResolvingCid] = useState<string | null>(null);

  const isRegenerating = data.state === 'regenerating';
  const comments = data.comments ?? [];

  const captureSelection = useCallback((focusOnOpen: boolean) => {
    if (!canComment || isRegenerating) return;
    const selection = window.getSelection();
    if (!selection || selection.isCollapsed || !bodyRef.current) {
      return;
    }
    const text = selection.toString().trim();
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
    setInlineSelection({
      anchor: text.slice(0, MAX_ANCHOR_LEN),
      virtualElement: {
        contextElement: bodyRef.current,
        getBoundingClientRect: () => range.getBoundingClientRect(),
      },
      focusOnOpen,
    });
    setInlineMessage('');
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
    if (!inlineSelection?.focusOnOpen) return;
    inlineComposerRef.current?.focus();
  }, [inlineSelection]);

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
      >
        <ArtifactRenderer content={data.content} />
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
            comments.map((comment) => (
              <CommentCard
                key={comment.cid}
                comment={comment}
                canComment={canComment}
                resolving={resolvingCid === comment.cid}
                onToggleResolved={() => void toggleResolved(comment)}
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
  canComment,
  resolving,
  onToggleResolved,
}: {
  comment: CommentRecord;
  canComment: boolean;
  resolving: boolean;
  onToggleResolved: () => void;
}) {
  return (
    <div
      className={cn(
        'flex flex-col gap-1.5 rounded-lg border bg-card/40 p-3',
        comment.resolved && 'opacity-70',
      )}
    >
      <div className="flex items-center gap-2">
        <span className="truncate text-xs font-semibold">{comment.author}</span>
        <Badge variant="outline" className="h-4 px-1 font-mono text-[10px]">
          v{comment.version}
        </Badge>
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
      {comment.anchor ? (
        <blockquote className="truncate border-l-2 border-primary/40 pl-2 text-xs italic text-muted-foreground">
          {comment.anchor}
        </blockquote>
      ) : null}
      <p className="whitespace-pre-wrap text-sm leading-snug">{comment.message}</p>
      {canComment ? (
        <div className="flex justify-end">
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
        </div>
      ) : null}
    </div>
  );
}
