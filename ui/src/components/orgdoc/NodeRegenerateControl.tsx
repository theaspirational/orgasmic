import { useState, type FormEvent } from 'react';
import { Loader2, RefreshCw } from 'lucide-react';
import { toast } from 'sonner';

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
import { postOrgNodeRegenerate } from '@/lib/api';

import {
  emptyTransportSelection,
  harnessArgTokens,
  TransportPicker,
  type TransportSelection,
} from '../TransportPicker';

export function NodeRegenerateControl({
  projectId,
  nodeId,
  label,
}: {
  projectId: string;
  nodeId: string;
  label: string;
}) {
  const [open, setOpen] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [extraPrompt, setExtraPrompt] = useState('');
  const [transport, setTransport] = useState<TransportSelection>(emptyTransportSelection);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!transport.mode || !transport.harness || submitting) return;
    setSubmitting(true);
    try {
      await postOrgNodeRegenerate(
        nodeId,
        {
          ...(extraPrompt.trim() ? { extraPrompt: extraPrompt.trim() } : {}),
          mode: transport.mode,
          harness: transport.harness,
          harness_args:
            transport.harness === 'custom' ? harnessArgTokens(transport.harness_args) : undefined,
          model: transport.model || null,
          effort: transport.effort || null,
        },
        projectId,
      );
      toast.success(`${label} regeneration started`);
      setExtraPrompt('');
      setOpen(false);
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error));
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <>
      <Button type="button" variant="outline" size="sm" onClick={() => setOpen(true)}>
        <RefreshCw />
        Regenerate {label.toLocaleLowerCase()}
      </Button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent showCloseButton className="sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>Regenerate {label.toLocaleLowerCase()}</DialogTitle>
            <DialogDescription>
              Launches the prompt declared by this node type descriptor with the current node and
              journal as context.
            </DialogDescription>
          </DialogHeader>
          <form className="flex flex-col gap-4" onSubmit={submit}>
            <TransportPicker kindLabel={label} value={transport} onChange={setTransport} />
            <label className="flex flex-col gap-1.5 text-sm">
              <span className="font-medium">Extra prompt (optional)</span>
              <Textarea
                rows={4}
                value={extraPrompt}
                disabled={submitting}
                onChange={(event) => setExtraPrompt(event.target.value)}
                placeholder="Anything extra to steer this regeneration…"
              />
            </label>
            <DialogFooter className="mx-0 mb-0 mt-2 rounded-md">
              <Button type="button" variant="outline" disabled={submitting} onClick={() => setOpen(false)}>
                Cancel
              </Button>
              <Button type="submit" disabled={!transport.mode || !transport.harness || submitting}>
                {submitting ? <Loader2 className="animate-spin" /> : null}
                {submitting ? 'Regenerating…' : 'Regenerate'}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>
    </>
  );
}
