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

import {
  emptyTransportSelection,
  TransportPicker,
  type TransportSelection,
} from './TransportPicker';

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
  const [transport, setTransport] = useState<TransportSelection>(emptyTransportSelection);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Deliberately keep `prompt` across close/reopen: an escaped or misclicked
  // dialog must not discard typed work. It clears only after a successful
  // submit.
  useEffect(() => {
    if (!open) return;
    setSubmitting(false);
    setError(null);
  }, [open]);

  const canSubmit =
    prompt.trim().length > 0 && transport.mode.length > 0 && transport.harness.length > 0;
  const suggestions = promptSuggestions(nodes, nodeLabels);

  async function submit() {
    if (!canSubmit) return;
    setSubmitting(true);
    setError(null);
    try {
      const result = await generateArtifact(
        {
          nodes,
          prompt: prompt.trim(),
          mode: transport.mode,
          harness: transport.harness,
          harness_args:
            transport.harness === 'custom' ? transport.harness_args : undefined,
          model: transport.model.length > 0 ? transport.model : null,
          effort: transport.effort.length > 0 ? transport.effort : null,
        },
        projectId,
      );
      toast.success('Artifact generation started');
      setPrompt('');
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

  // Starter prompts shaped by the subject, shown only while the field is
  // empty: a likely default beats a blank required textarea, and the user
  // still edits before submitting.
  function promptSuggestions(ids: string[], labels?: string[]): string[] {
    if (ids.length === 0) {
      return [
        'Project overview for a new teammate.',
        'Current risks and open questions.',
        'Sprint board from the open tasks.',
      ];
    }
    if (ids.length === 1) {
      const label = labels?.[0] ?? ids[0];
      return [
        `One-page brief on ${label}: context, mechanism, open questions.`,
        `Review packet for ${label} — what to check and what could break.`,
        `Diagram how ${label} connects to the rest of the system.`,
      ];
    }
    return [
      `Compare these ${ids.length} nodes: overlaps, gaps, and tensions.`,
      `One-page brief covering these ${ids.length} nodes for a new teammate.`,
      'Review packet: risks and open questions across the selection.',
    ];
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
          <TransportPicker kindLabel="artifactor" value={transport} onChange={setTransport} />
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
          {prompt.trim().length === 0 && suggestions.length > 0 ? (
            <div className="flex flex-wrap gap-1.5" aria-label="Prompt suggestions">
              {suggestions.map((suggestion) => (
                <Button
                  key={suggestion}
                  type="button"
                  variant="outline"
                  size="sm"
                  className="h-auto whitespace-normal py-1 text-left text-xs font-normal text-muted-foreground"
                  onClick={() => setPrompt(suggestion)}
                >
                  {suggestion}
                </Button>
              ))}
            </div>
          ) : null}
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
