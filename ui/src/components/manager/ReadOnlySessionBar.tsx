import { Eye } from 'lucide-react';

// Shown in place of a run composer when the viewer is a member without the
// sessions.interact capability: they may watch the live stream but cannot send.
export function ReadOnlySessionBar() {
  return (
    <div
      className="flex h-full min-h-0 items-center justify-center gap-2 border-t bg-muted/30 px-3 py-4 text-xs text-muted-foreground"
      role="status"
    >
      <Eye className="size-3.5" />
      <span>Read-only session — you can watch but not send input.</span>
    </div>
  );
}
