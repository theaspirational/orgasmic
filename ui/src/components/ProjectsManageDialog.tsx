import { BoardView } from '@/components/BoardView';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { ScrollArea } from '@/components/ui/scroll-area';
import { useActiveProject } from '@/hooks/useActiveProject';

export function ProjectsManageDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const { setActiveProject } = useActiveProject();

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        showCloseButton
        className="sm:max-w-none w-[min(96vw,80rem)] h-[85vh] p-0 overflow-hidden grid grid-rows-[auto_1fr_auto] gap-0"
      >
        <DialogHeader className="border-b px-5 py-4 pr-12">
          <DialogTitle>Manage projects</DialogTitle>
          <DialogDescription>
            Switch between projects registered on this daemon.
          </DialogDescription>
        </DialogHeader>
        <ScrollArea className="min-h-0">
          <div className="p-5">
            <BoardView
              onSelectProject={(projectId) => {
                setActiveProject(projectId);
                onOpenChange(false);
              }}
            />
          </div>
        </ScrollArea>
        <DialogFooter className="mx-0 mb-0 rounded-none border-t px-5 py-3">
          <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
            Close
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
