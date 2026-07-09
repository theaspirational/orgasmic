import { useEffect, useMemo, useRef, useState } from 'react';
import { StreamLanguage, type StringStream } from '@codemirror/language';
import { EditorState } from '@codemirror/state';
import { EditorView } from '@codemirror/view';
import { basicSetup } from 'codemirror';
import { toast } from 'sonner';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import { fetchOrgFile, postOrgFile } from '@/lib/api';
import { useResource } from '@/lib/useResource';

import { ErrorPanel } from './Primitives';

const ORG_FILES = [
  '.orgasmic/tasks/backlog.org',
  '.orgasmic/tasks/todo.org',
  '.orgasmic/tasks/in_progress.org',
  '.orgasmic/tasks/in_review.org',
  '.orgasmic/tasks/done.org',
  '.orgasmic/tasks/cancelled.org',
  '.orgasmic/tasks/goal.org',
  '.orgasmic/decisions.org',
  '.orgasmic/architecture.org',
  '.orgasmic/glossary.org',
  '.orgasmic/project.org',
];

type Heading = {
  line: number;
  level: number;
  title: string;
};

const orgLanguage = StreamLanguage.define({
  token(stream: StringStream) {
    if (stream.sol()) {
      if (stream.match(/^\*+\s+.*/)) return 'heading';
      if (stream.match(/^#\+[\w_-]+:/i)) return 'meta';
      if (stream.match(/^:[A-Z0-9_@#%+-]+:/)) return 'attributeName';
      if (stream.match(/^:PROPERTIES:|^:END:/)) return 'keyword';
    }
    if (stream.match(/=[^=]+=/)) return 'atom';
    if (stream.match(/\[[^\]]+\]/)) return 'number';
    stream.next();
    return null;
  },
});

function outline(contents: string): Heading[] {
  return contents.split(/\r?\n/).flatMap((line, index) => {
    const match = /^(\*+)\s+(.+)$/.exec(line);
    if (!match) return [];
    return [{ line: index + 1, level: match[1].length, title: match[2] }];
  });
}

export function OrgView({ projectId }: { projectId: string | null }) {
  const refresh = useRefreshToken();
  const [path, setPath] = useState(ORG_FILES[0]);
  const [draft, setDraft] = useState('');
  const [saveBusy, setSaveBusy] = useState(false);
  const editorHost = useRef<HTMLDivElement | null>(null);
  const viewRef = useRef<EditorView | null>(null);
  const file = useResource(`org-file:${projectId ?? 'default'}:${path}:${refresh}`, () =>
    fetchOrgFile(path, projectId),
  );
  const headings = useMemo(() => outline(draft), [draft]);

  useEffect(() => {
    if (file.data) setDraft(file.data.contents);
  }, [file.data]);

  useEffect(() => {
    const host = editorHost.current;
    if (!host || !file.data) return undefined;
    viewRef.current?.destroy();
    const view = new EditorView({
      parent: host,
      state: EditorState.create({
        doc: file.data.contents,
        extensions: [
          basicSetup,
          orgLanguage,
          EditorView.lineWrapping,
          EditorView.theme({
            '&': { height: '100%' },
            '.cm-scroller': { fontFamily: 'var(--mono)', fontSize: '13px' },
            '.cm-content': { minHeight: '34rem' },
          }),
          EditorView.updateListener.of((update) => {
            if (update.docChanged) setDraft(update.state.doc.toString());
          }),
        ],
      }),
    });
    viewRef.current = view;
    return () => {
      view.destroy();
      if (viewRef.current === view) viewRef.current = null;
    };
  }, [file.data, path]);

  async function handleSave() {
    setSaveBusy(true);
    try {
      const result = await postOrgFile(path, draft, projectId);
      toast.success('Org file saved', { description: result.tx_id });
      await file.refresh();
    } catch (err) {
      toast.error('Save failed', {
        description: err instanceof Error ? err.message : String(err),
      });
    } finally {
      setSaveBusy(false);
    }
  }

  function scrollTo(lineNumber: number) {
    const view = viewRef.current;
    if (!view) return;
    const line = view.state.doc.line(lineNumber);
    view.dispatch({
      selection: { anchor: line.from },
      effects: EditorView.scrollIntoView(line.from, { y: 'start' }),
    });
    view.focus();
  }

  return (
    <section className="flex flex-col gap-4">
      <Card>
        <CardHeader>
          <CardTitle className="text-base">Org</CardTitle>
          {projectId ? <Badge variant="outline">{projectId}</Badge> : <span className="text-sm text-muted-foreground">default scope</span>}
        </CardHeader>
        <CardContent className="flex flex-wrap items-center gap-2">
          <Select value={path} onValueChange={setPath}>
            <SelectTrigger className="w-[18rem]">
              <SelectValue placeholder="Org file" />
            </SelectTrigger>
            <SelectContent>
              <SelectGroup>
                {ORG_FILES.map((candidate) => (
                  <SelectItem key={candidate} value={candidate}>
                    {candidate}
                  </SelectItem>
                ))}
              </SelectGroup>
            </SelectContent>
          </Select>
          <Button type="button" variant="outline" onClick={() => void file.refresh()}>
            Reload
          </Button>
          <Button type="button" disabled={saveBusy} onClick={() => void handleSave()}>
            {saveBusy ? 'Saving...' : 'Save'}
          </Button>
        </CardContent>
      </Card>

      {file.error ? <ErrorPanel error={file.error} /> : null}

      <Card className="min-h-[38rem] overflow-hidden p-0">
        <div className="grid min-h-[38rem] grid-cols-1 md:grid-cols-[18rem_1fr]">
          <aside className="border-b bg-muted/20 md:border-b-0 md:border-r">
            <div className="border-b px-4 py-3 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
              Outline
            </div>
            <div className="flex max-h-[34rem] flex-col gap-1 overflow-auto p-2">
              {headings.length === 0 ? (
                <p className="px-2 py-3 text-sm text-muted-foreground">No headings.</p>
              ) : (
                headings.map((heading) => (
                  <button
                    key={`${heading.line}:${heading.title}`}
                    type="button"
                    onClick={() => scrollTo(heading.line)}
                    className="truncate rounded-md px-2 py-1.5 text-left text-xs hover:bg-muted focus-visible:bg-muted focus-visible:outline-none"
                    style={{ paddingLeft: `${0.5 + (heading.level - 1) * 0.75}rem` }}
                  >
                    {heading.title}
                  </button>
                ))
              )}
            </div>
          </aside>
          <div className="min-w-0">
            {file.loading && !file.data ? (
              <p className="p-4 text-sm text-muted-foreground">Loading...</p>
            ) : (
              <div ref={editorHost} className="h-full min-h-[38rem]" />
            )}
          </div>
        </div>
      </Card>
    </section>
  );
}
