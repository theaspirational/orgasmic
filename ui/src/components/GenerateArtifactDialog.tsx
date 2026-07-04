import { useEffect, useState, type FormEvent } from 'react';
import { useNavigate } from '@tanstack/react-router';
import { Loader2 } from 'lucide-react';
import { toast } from 'sonner';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Textarea } from '@/components/ui/textarea';
import { generateArtifact } from '@/lib/api';

export function GenerateArtifactDialog({
  projectId,
  open,
  onOpenChange,
  nodes,
  nodeLabels,
}: {
  projectId: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Subject node ids — empty is a valid prompt-only artifact. */
  nodes: string[];
  /** Optional human-readable labels for `nodes`, same order, for display only. */
  nodeLabels?: string[];
}) {
  const navigate = useNavigate();
  const [prompt, setPrompt] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setPrompt('');
    setSubmitting(false);
    setError(null);
  }, [open]);

  const canSubmit = prompt.trim().length > 0;

  async function submit() {
    if (!canSubmit) return;
    setSubmitting(true);
    setError(null);
    try {
      const result = await generateArtifact({ nodes, prompt: prompt.trim() }, projectId);
      toast.success('Artifact generation started');
      onOpenChange(false);
      void navigate({
        to: '/projects/$projectId/artifacts/$artifactId',
        params: { projectId, artifactId: result.artifact_id },
      });
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSubmitting(false);
    }
  }

  function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    void submit();
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent showCloseButton className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Generate artifact</DialogTitle>
          <DialogDescription>
            Launches an artifact-generator run against the daemon. The artifact appears live once submitted.
          </DialogDescription>
        </DialogHeader>
        <form className="flex flex-col gap-4" onSubmit={handleSubmit}>
          <div className="flex flex-col gap-1.5 text-sm">
            <span className="font-medium">Subject</span>
            {nodes.length === 0 ? (
              <p className="text-xs text-muted-foreground">Prompt-only — no subject nodes attached.</p>
            ) : (
              <div className="flex flex-wrap gap-1.5">
                {nodes.map((id, index) => (
                  <Badge key={id} variant="outline" className="font-mono">
                    {nodeLabels?.[index] ?? id}
                  </Badge>
                ))}
              </div>
            )}
          </div>
          <label className="flex flex-col gap-1.5 text-sm">
            <span className="font-medium">Prompt</span>
            <Textarea
              required
              autoFocus
              rows={5}
              value={prompt}
              onChange={(event) => setPrompt(event.target.value)}
              placeholder="What should this artifact cover?"
            />
          </label>
          {error ? (
            <div role="alert" aria-live="polite" className="rounded-md border border-destructive/30 bg-destructive/5 px-3 py-2 text-sm text-destructive">
              {error}
            </div>
          ) : null}
          <DialogFooter className="mx-0 mb-0 mt-2 rounded-md">
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" disabled={!canSubmit || submitting}>
              {submitting ? <Loader2 className="animate-spin" /> : null}
              {submitting ? 'Generating...' : 'Generate'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
