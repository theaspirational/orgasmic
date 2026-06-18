import { useEffect, useState, type FormEvent } from 'react';
import { ChevronLeft, ChevronRight, FolderOpen, Loader2 } from 'lucide-react';

import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { addProject, fetchFilesystemEntries, fetchFilesystemRoots } from '@/lib/api';
import { useActiveProject } from '@/hooks/useActiveProject';
import { useRefreshBump } from '@/hooks/useRefreshBus';
import { useActiveProfile } from '@/lib/backend';
import { canUseNativeDirectoryPicker, pickLocalDirectory } from '@/lib/nativeBridge';
import type { FilesystemEntry, FilesystemRoot } from '@/lib/types';

export function ProjectAddDialog({
  open,
  onOpenChange,
  onOpenManage,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onOpenManage?: () => void;
}) {
  const [path, setPath] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [showScaffoldCta, setShowScaffoldCta] = useState(false);
  const [roots, setRoots] = useState<FilesystemRoot[]>([]);
  const [entries, setEntries] = useState<FilesystemEntry[]>([]);
  const [browserPath, setBrowserPath] = useState('');
  const [browserBusy, setBrowserBusy] = useState(false);
  const [browserBusyPath, setBrowserBusyPath] = useState<string | null>(null);
  const [browserError, setBrowserError] = useState<string | null>(null);
  const { setActiveProject } = useActiveProject();
  const activeProfile = useActiveProfile();
  const bumpRefresh = useRefreshBump();
  const nativePickerAvailable = canUseNativeDirectoryPicker(activeProfile);
  const browserItems = entries.length > 0
    ? entries.map((entry) => ({ ...entry, root: false }))
    : roots.map((root) => ({
        ...root,
        root: true,
        accessible: true,
        orgasmic_project: false,
        project_id: null,
      }));

  useEffect(() => {
    if (!open) return;
    setPath('');
    setSubmitting(false);
    setError(null);
    setShowScaffoldCta(false);
    setRoots([]);
    setEntries([]);
    setBrowserPath('');
    setBrowserBusy(false);
    setBrowserBusyPath(null);
    setBrowserError(null);
  }, [open]);

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    setBrowserBusy(true);
    setBrowserBusyPath('roots');
    setBrowserError(null);
    void fetchFilesystemRoots()
      .then((result) => {
        if (cancelled) return;
        setRoots(result);
      })
      .catch((err) => {
        if (cancelled) return;
        setBrowserError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (!cancelled) {
          setBrowserBusy(false);
          setBrowserBusyPath(null);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [open]);

  const canSubmit = path.trim().length > 0;

  async function submit(scaffold = false) {
    if (!canSubmit) return;
    setSubmitting(true);
    setError(null);
    setShowScaffoldCta(false);
    try {
      const result = await addProject({
        path: path.trim(),
        scaffold,
      });
      bumpRefresh();
      setActiveProject(result.project_id);
      onOpenChange(false);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setError(message);
      if (message.includes('not an orgasmic project')) {
        setShowScaffoldCta(true);
      }
    } finally {
      setSubmitting(false);
    }
  }

  function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    void submit(false);
  }

  async function chooseNativeDirectory() {
    setBrowserError(null);
    try {
      const selected = await pickLocalDirectory();
      if (selected) setPath(selected);
    } catch (err) {
      setBrowserError(err instanceof Error ? err.message : String(err));
    }
  }

  async function openDirectory(nextPath: string) {
    setBrowserBusy(true);
    setBrowserBusyPath(nextPath);
    setBrowserError(null);
    try {
      const result = await fetchFilesystemEntries(nextPath);
      setBrowserPath(nextPath);
      setPath(nextPath);
      setEntries(result);
    } catch (err) {
      setBrowserError(err instanceof Error ? err.message : String(err));
    } finally {
      setBrowserBusy(false);
      setBrowserBusyPath(null);
    }
  }

  function parentDirectory(currentPath: string): string | null {
    if (!currentPath) return null;
    const normalized = currentPath.replace(/[\\/]+$/, '');
    if (!normalized) return null;
    if (/^[A-Za-z]:\\?$/.test(normalized)) return null;
    const separator = normalized.includes('\\') ? '\\' : '/';
    const index = normalized.lastIndexOf(separator);
    if (index <= 0) return separator === '/' ? '/' : null;
    return normalized.slice(0, index);
  }

  const parentPath = parentDirectory(browserPath);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent showCloseButton className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Add project</DialogTitle>
          <DialogDescription>
            Register a repository on the active daemon.
          </DialogDescription>
        </DialogHeader>
        <form className="flex flex-col gap-4" onSubmit={handleSubmit}>
          <label className="flex flex-col gap-1.5 text-sm">
            <span className="font-medium">Path</span>
            <Input
              required
              value={path}
              onChange={(event) => setPath(event.target.value)}
              placeholder="/path/to/repo"
            />
          </label>
          <div className="grid gap-2 rounded-md border bg-muted/20 p-2">
            <div className="flex items-center justify-between gap-2">
              <p className="text-xs text-muted-foreground">
                {activeProfile.id === 'local' ? 'Local daemon filesystem' : 'Daemon host filesystem'}
              </p>
              {nativePickerAvailable ? (
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={() => void chooseNativeDirectory()}
                >
                  <FolderOpen />
                  Choose
                </Button>
              ) : null}
            </div>
            {browserError ? (
              <p className="text-xs text-destructive">{browserError}</p>
            ) : null}
            <div className="grid grid-cols-[auto_1fr] items-center gap-1">
              {parentPath ? (
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-sm"
                  disabled={browserBusy}
                  onClick={() => void openDirectory(parentPath)}
                  aria-label="Open parent directory"
                >
                  {browserBusyPath === parentPath ? <Loader2 className="animate-spin" /> : <ChevronLeft />}
                </Button>
              ) : <span />}
              {browserPath ? (
                <p className="truncate font-mono text-xs text-muted-foreground">{browserPath}</p>
              ) : (
                <p className="truncate text-xs text-muted-foreground">Roots</p>
              )}
            </div>
            <div className="grid max-h-48 gap-1 overflow-auto">
              {browserItems.map((item) => {
                const itemPath = item.path;
                const canOpen = item.root || item.kind === 'directory' || item.kind === 'symlink';
                return (
                  <div
                    key={`${item.kind}:${itemPath}`}
                    className="grid grid-cols-[1fr_auto_auto] items-center gap-1 rounded-sm px-1 py-0.5 text-sm"
                  >
                    <button
                      type="button"
                      className="truncate text-left font-mono text-xs hover:underline"
                      onClick={() => setPath(itemPath)}
                    >
                      {item.display_name}
                    </button>
                    {!item.root && item.orgasmic_project ? (
                      <span className="rounded-sm bg-primary/10 px-1.5 py-0.5 text-[0.68rem] text-primary">
                        project
                      </span>
                    ) : <span />}
                    <Button
                      type="button"
                      variant="ghost"
                      size="icon-sm"
                      disabled={!canOpen || browserBusy}
                      onClick={() => void openDirectory(itemPath)}
                      aria-label={`Open ${item.display_name}`}
                    >
                      {browserBusyPath === itemPath ? <Loader2 className="animate-spin" /> : <ChevronRight />}
                    </Button>
                  </div>
                );
              })}
              {browserBusy && roots.length === 0 && entries.length === 0 ? (
                <p className="px-1 py-2 text-xs text-muted-foreground">Loading...</p>
              ) : null}
            </div>
          </div>
          {error ? (
            <div
              role="alert"
              aria-live="polite"
              className="rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-sm text-amber-900 dark:text-amber-200"
            >
              <p>{error}</p>
              {showScaffoldCta ? (
                <Button
                  type="button"
                  variant="secondary"
                  className="mt-2"
                  disabled={!canSubmit || submitting}
                  onClick={() => void submit(true)}
                >
                  {submitting ? <Loader2 className="animate-spin" /> : null}
                  Scaffold a new orgasmic project here
                </Button>
              ) : null}
              {onOpenManage && !showScaffoldCta ? (
                <Button
                  type="button"
                  variant="link"
                  className="mt-1 h-auto p-0 text-sm"
                  onClick={onOpenManage}
                >
                  Open Manage
                </Button>
              ) : null}
            </div>
          ) : null}
          <DialogFooter className="mx-0 mb-0 mt-2 rounded-md">
            <Button
              type="button"
              variant="outline"
              onClick={() => onOpenChange(false)}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={!canSubmit || submitting}>
              {submitting ? <Loader2 className="animate-spin" /> : null}
              {submitting ? 'Adding...' : 'Add project'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
