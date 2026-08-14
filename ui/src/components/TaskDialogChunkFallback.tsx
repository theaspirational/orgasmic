import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Skeleton } from '@/components/ui/skeleton';
import { PeekBackButton } from '@/components/PeekBackButton';

export function TaskDialogChunkFallback({
  taskId,
  historyDepth,
  onBack,
  onClose,
}: {
  taskId: string;
  historyDepth: number;
  onBack: () => void;
  onClose: () => void;
}) {
  return (
    <Dialog open onOpenChange={(next) => !next && onClose()}>
      <DialogContent
        showCloseButton
        className="grid h-[min(92dvh,54rem)] w-[min(96vw,90rem)] max-w-none grid-rows-[auto_auto_1fr] gap-0 overflow-hidden p-0 sm:max-w-none"
      >
        <DialogHeader className="flex-row items-start gap-3 border-b px-5 py-4 pr-12">
          <PeekBackButton depth={historyDepth} onBack={onBack} />
          <div className="min-w-0 flex-1">
            <DialogDescription className="font-mono text-xs">{taskId}</DialogDescription>
            <DialogTitle className="text-base font-semibold leading-snug sm:text-lg">
              Loading task
            </DialogTitle>
          </div>
        </DialogHeader>
        <div className="flex h-11 items-center justify-between border-b bg-muted/10 px-3">
          <Skeleton className="h-7 w-24" />
          <Skeleton className="h-7 w-24" />
        </div>
        <div className="grid min-h-0 grid-cols-1 md:grid-cols-[minmax(0,1fr)_20rem]">
          <div className="flex flex-col gap-4 px-5 py-6 sm:px-7 lg:px-8">
            <Skeleton className="h-4 w-40" />
            <Skeleton className="h-24" />
          </div>
          <div className="hidden border-l bg-muted/20 p-3 lg:block">
            <Skeleton className="h-20" />
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
