import { useState } from 'react';
import { Trash2 } from 'lucide-react';
import { toast } from 'sonner';

import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from '@/components/ui/alert-dialog';
import { Button } from '@/components/ui/button';
import { useRefreshBump } from '@/hooks/useRefreshBus';
import { postOrgNodeDelete } from '@/lib/api';
import { HttpError } from '@/lib/transport';

import type { NodeKind } from './orgNodes';

function deleteErrorMessage(error: unknown): string {
  if (error instanceof HttpError) return error.detail ?? error.body ?? error.message;
  return error instanceof Error ? error.message : String(error);
}

export function NodeDeleteControl({
  projectId,
  id,
  kind,
  title,
  baseVersion,
  childCount = 0,
  onDeleted,
}: {
  projectId: string;
  id: string;
  kind: NodeKind;
  title: string;
  baseVersion: string | null;
  childCount?: number;
  onDeleted: () => void;
}) {
  const refreshBump = useRefreshBump();
  const [open, setOpen] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const kindLabel = kind === 'decision' ? 'decision' : 'glossary term';
  const blockedByChildren = kind === 'decision' && childCount > 0;

  async function confirmDelete() {
    if (!baseVersion || deleting || blockedByChildren) return;
    setDeleting(true);
    setError(null);
    try {
      await postOrgNodeDelete(id, { baseVersion }, projectId, kind);
      setOpen(false);
      refreshBump();
      toast.success(`Deleted ${id}`);
      onDeleted();
    } catch (nextError) {
      setError(deleteErrorMessage(nextError));
    } finally {
      setDeleting(false);
    }
  }

  return (
    <AlertDialog
      open={open}
      onOpenChange={(next) => {
        if (deleting) return;
        setOpen(next);
        if (!next) setError(null);
      }}
    >
      <AlertDialogTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          disabled={!baseVersion}
          aria-label={`Delete ${kindLabel} ${id}`}
        >
          <Trash2 data-icon="inline-start" />
          Delete
        </Button>
      </AlertDialogTrigger>
      <AlertDialogContent size="sm">
        <AlertDialogHeader>
          <AlertDialogTitle>Delete this {kindLabel}?</AlertDialogTitle>
          <AlertDialogDescription>
            “{title}” ({id}) will be permanently removed. This cannot be undone.
          </AlertDialogDescription>
        </AlertDialogHeader>
        {blockedByChildren ? (
          <p role="alert" className="text-sm text-destructive">
            This decision has {childCount} child {childCount === 1 ? 'decision' : 'decisions'}.
            Re-parent or delete {childCount === 1 ? 'it' : 'them'} first.
          </p>
        ) : null}
        {error ? (
          <p role="alert" className="text-sm text-destructive">
            {error}
          </p>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={deleting}>Cancel</AlertDialogCancel>
          <AlertDialogAction
            variant="destructive"
            disabled={!baseVersion || deleting || blockedByChildren}
            onClick={(event) => {
              event.preventDefault();
              void confirmDelete();
            }}
          >
            {deleting ? 'Deleting…' : 'Delete node'}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
