import { useState } from 'react';
import { FolderGit2, Plus } from 'lucide-react';

import { BoardView } from '@/components/BoardView';
import { ErrorPanel } from '@/components/Primitives';
import { ProjectAddDialog } from '@/components/ProjectAddDialog';
import { ProjectsManageDialog } from '@/components/ProjectsManageDialog';
import { Button } from '@/components/ui/button';
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { useActiveProject } from '@/hooks/useActiveProject';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import { fetchProjects } from '@/lib/api';
import { useResource } from '@/lib/useResource';

export function BoardRouteView() {
  const refresh = useRefreshToken();
  const { setActiveProject } = useActiveProject();
  const { data, error, loading } = useResource(`projects:${refresh}:board`, fetchProjects);
  const [addOpen, setAddOpen] = useState(false);
  const [manageOpen, setManageOpen] = useState(false);

  if (error) return <ErrorPanel error={error} />;

  if (loading && !data) {
    return (
      <div className="flex min-h-[calc(100vh-8rem)] items-center justify-center">
        <Card className="w-full max-w-lg">
          <CardHeader>
            <Skeleton className="size-8 rounded-md" />
            <Skeleton className="h-6 w-48" />
            <Skeleton className="h-4 w-full" />
          </CardHeader>
          <CardContent>
            <Skeleton className="h-8 w-40" />
          </CardContent>
        </Card>
      </div>
    );
  }

  if ((data ?? []).length === 0) {
    return (
      <>
        <div className="flex min-h-[calc(100vh-8rem)] items-center justify-center">
          <Card className="w-full max-w-lg">
            <CardHeader>
              <FolderGit2 className="size-8 text-muted-foreground" />
              <CardTitle>No projects yet</CardTitle>
              <CardDescription>
                Register a repo with orgasmic to make the project-scoped sidebar useful.
              </CardDescription>
            </CardHeader>
            <CardContent>
              <Button type="button" onClick={() => setAddOpen(true)}>
                <Plus />
                Add your first project
              </Button>
            </CardContent>
          </Card>
        </div>
        <ProjectAddDialog
          open={addOpen}
          onOpenChange={setAddOpen}
          onOpenManage={() => {
            setAddOpen(false);
            setManageOpen(true);
          }}
        />
        <ProjectsManageDialog open={manageOpen} onOpenChange={setManageOpen} />
      </>
    );
  }

  return <BoardView onSelectProject={setActiveProject} />;
}
