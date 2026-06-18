// @arch arch_MK2Q2.4
import { useCallback, useEffect, useId, useMemo, useState } from 'react';
import { Bell } from 'lucide-react';
import { toast } from 'sonner';

import { Button } from '@/components/ui/button';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { useIsMobile } from '@/hooks/use-mobile';
import { useEventStream } from '@/hooks/useEventStream';
import {
  fetchParseErrors,
  fetchTx,
} from '@/lib/api';
import type { DaemonEvent, ParseError, QuestionEntry, TxRecord, ViewName } from '@/lib/types';
import { useResource } from '@/lib/useResource';

import { NotificationPopover, type NotificationRow, type NotificationSections } from './NotificationPopover';

const DISMISSED_KEY_PREFIX = 'orgasmic.notifications.dismissed.v2';

function dismissedStorageKey(projectId: string | null): string {
  return `${DISMISSED_KEY_PREFIX}.${projectId ?? 'global'}`;
}

function loadDismissed(projectId: string | null): Set<string> {
  if (typeof window === 'undefined') return new Set();
  try {
    const parsed = JSON.parse(window.localStorage.getItem(dismissedStorageKey(projectId)) ?? '[]') as unknown;
    return new Set(Array.isArray(parsed) ? parsed.filter((item): item is string => typeof item === 'string') : []);
  } catch {
    return new Set();
  }
}

function extraValue(record: TxRecord, key: string): string | null {
  const pair = record.entry.extra.find(([entryKey]) => entryKey === key);
  return pair?.[1] ?? null;
}

function openQuestions(records: TxRecord[]): QuestionEntry[] {
  const answered = new Set(
    records
      .filter((record) => record.entry.ty === 'question.answered')
      .map((record) => extraValue(record, 'QUESTION_ID') ?? record.entry.target ?? '')
      .filter(Boolean),
  );

  return records
    .filter((record) => record.entry.ty === 'question.raised')
    .map((record) => {
      const questionId = extraValue(record, 'QUESTION_ID') ?? record.entry.target ?? record.entry.tx_id;
      return {
        tx_id: record.entry.tx_id,
        question_id: questionId,
        task_id: record.entry.task ?? extraValue(record, 'TASK_ID'),
        reason: record.entry.reason,
        time: record.entry.time,
      };
    })
    .filter((question) => !answered.has(question.question_id));
}

function parseErrorKey(error: ParseError): string {
  return `${error.path}:${error.line ?? ''}:${error.at}`;
}

function total(sections: NotificationSections): number {
  return sections.questions.length + sections.parseErrors.length;
}

export function NotificationBell({
  projectId,
  onNavigate,
  onOpenTask,
}: {
  projectId: string | null;
  onNavigate: (next: ViewName) => void;
  onOpenTask: (taskId: string) => void;
}) {
  const isMobile = useIsMobile();
  const mobileSheetContentId = useId();
  const mobileSheetTitleId = useId();
  const [open, setOpen] = useState(false);
  const [dismissed, setDismissed] = useState<Set<string>>(() => loadDismissed(projectId));
  const tx = useResource(`notifications-tx:${projectId ?? 'all'}`, () => fetchTx(projectId, 200));
  const parseErrors = useResource('notifications-parse-errors', fetchParseErrors);

  useEffect(() => {
    setDismissed(loadDismissed(projectId));
  }, [projectId]);

  // Row keys stay compact because the localStorage key carries the active project namespace.
  const dismiss = useCallback((key: string) => {
    setDismissed((current) => {
      const next = new Set(current);
      next.add(key);
      window.localStorage.setItem(dismissedStorageKey(projectId), JSON.stringify(Array.from(next)));
      return next;
    });
  }, [projectId]);

  const refreshAll = useCallback(async () => {
    await Promise.all([tx.refresh(), parseErrors.refresh()]);
  }, [parseErrors, tx]);

  useEventStream(
    useCallback(
      (event: DaemonEvent) => {
        if (event.topic === 'graph' || event.topic === 'manager') {
          void refreshAll();
          return;
        }
        if (event.topic === 'run' && event.payload.kind !== 'run_event') {
          void refreshAll();
        }
      },
      [refreshAll],
    ),
  );

  const sections = useMemo<NotificationSections>(() => {
    const questions: NotificationRow[] = openQuestions(tx.data ?? [])
      .filter((question) => !dismissed.has(`question:${question.tx_id}`))
      .map((question) => ({
        key: `question:${question.tx_id}`,
        title: question.task_id ? `${question.question_id} / ${question.task_id}` : question.question_id,
        detail: question.reason ?? question.time,
        actionLabel: 'Answer',
        onAction: () => {
          if (question.task_id) onOpenTask(question.task_id);
          onNavigate('tasks');
          setOpen(false);
        },
        onDismiss: () => dismiss(`question:${question.tx_id}`),
      }));

    const parseErrorRows: NotificationRow[] = (parseErrors.data ?? [])
      .filter((error) => !dismissed.has(`parse:${parseErrorKey(error)}`))
      .map((error) => ({
        key: `parse:${parseErrorKey(error)}`,
        title: error.path,
        detail: error.line ? `Line ${error.line}: ${error.message}` : error.message,
        actionLabel: 'Reload',
        actionIcon: 'reload',
        onAction: () => {
          void (async () => {
            try {
              const errors = await fetchParseErrors();
              toast.success(`Reloaded parse errors: ${errors.length}`);
              await parseErrors.refresh();
            } catch (err) {
              toast.error('Reload failed', {
                description: err instanceof Error ? err.message : String(err),
              });
            }
          })();
        },
        onDismiss: () => dismiss(`parse:${parseErrorKey(error)}`),
      }));

    return { questions, parseErrors: parseErrorRows };
  }, [
    dismiss,
    dismissed,
    onNavigate,
    onOpenTask,
    parseErrors,
    projectId,
    refreshAll,
    tx.data,
  ]);

  const count = total(sections);
  const renderButton = (withSheetControls = false) => (
    <Button
      type="button"
      variant="ghost"
      size="icon"
      className="relative"
      aria-label={`Notifications${count ? `: ${count} unread` : ''}`}
      aria-expanded={withSheetControls ? open : undefined}
      aria-controls={withSheetControls ? mobileSheetContentId : undefined}
      onClick={isMobile ? () => setOpen(true) : undefined}
    >
      <Bell />
      {count > 0 ? (
        <span className="absolute -right-0.5 -top-0.5 flex min-w-4 items-center justify-center rounded-full bg-primary px-1 font-mono text-[10px] leading-4 text-primary-foreground">
          {count}
        </span>
      ) : null}
    </Button>
  );

  if (isMobile) {
    return (
      <>
        {renderButton(true)}
        <NotificationPopover
          open={open}
          mobile
          sections={sections}
          contentId={mobileSheetContentId}
          titleId={mobileSheetTitleId}
          onOpenChange={setOpen}
          onNavigateRuns={() => onNavigate('runs')}
        />
      </>
    );
  }

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>{renderButton(false)}</PopoverTrigger>
      <PopoverContent align="end" className="w-[min(calc(100vw-1rem),28rem)] max-w-[calc(100vw-1rem)]">
        <NotificationPopover
          open={open}
          mobile={false}
          sections={sections}
          onOpenChange={setOpen}
          onNavigateRuns={() => onNavigate('runs')}
        />
      </PopoverContent>
    </Popover>
  );
}
