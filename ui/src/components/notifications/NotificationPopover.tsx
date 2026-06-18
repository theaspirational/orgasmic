// @arch arch_MK2Q2.4
import { ArrowRight, RotateCw, X } from 'lucide-react';

import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';

export type NotificationRow = {
  key: string;
  title: string;
  detail: string;
  actionLabel: string;
  actionIcon?: 'reload';
  onAction: () => void;
  onDismiss: () => void;
};

export type NotificationSections = {
  questions: NotificationRow[];
  parseErrors: NotificationRow[];
};

function sectionTotal(sections: NotificationSections): number {
  return sections.questions.length + sections.parseErrors.length;
}

export function NotificationPopover({
  open,
  mobile,
  sections,
  contentId,
  titleId,
  onOpenChange,
  onNavigateRuns,
}: {
  open: boolean;
  mobile: boolean;
  sections: NotificationSections;
  contentId?: string;
  titleId?: string;
  onOpenChange: (open: boolean) => void;
  onNavigateRuns: () => void;
}) {
  const content = (
    <NotificationContent
      sections={sections}
      titleId={titleId}
      onNavigateRuns={() => {
        onNavigateRuns();
        onOpenChange(false);
      }}
    />
  );

  if (mobile) {
    return (
      <Sheet open={open} onOpenChange={onOpenChange}>
        <SheetContent
          id={contentId}
          side="bottom"
          className="h-full"
          aria-labelledby={titleId}
          showCloseButton
        >
          <SheetHeader className="sr-only">
            <SheetTitle>Notifications</SheetTitle>
            <SheetDescription>Items that need a user decision or parse-error action.</SheetDescription>
          </SheetHeader>
          {content}
        </SheetContent>
      </Sheet>
    );
  }

  return content;
}

function NotificationContent({
  sections,
  titleId,
  onNavigateRuns,
}: {
  sections: NotificationSections;
  titleId?: string;
  onNavigateRuns: () => void;
}) {
  const total = sectionTotal(sections);

  return (
    <div className="flex max-h-[min(680px,80vh)] w-full max-w-[calc(100vw-1rem)] flex-col">
      <div className="border-b p-3 md:p-4">
        <div className="flex items-center justify-between gap-3">
          <h2 id={titleId} className="text-sm font-medium">
            Needs you
          </h2>
          <span className="rounded-full border px-2 py-0.5 font-mono text-xs text-muted-foreground">
            {total}
          </span>
        </div>
        {total === 0 ? (
          <p className="mt-2 text-sm text-muted-foreground">All clear - nothing needs you.</p>
        ) : null}
      </div>
      <ScrollArea className="min-h-0 flex-1">
        <div className="space-y-4 p-3 md:p-4">
          <Section title="Questions" rows={sections.questions} empty="No questions." />
          <Section title="Parse Errors" rows={sections.parseErrors} empty="No parse errors." />
        </div>
      </ScrollArea>
      <div className="border-t p-2">
        <Button type="button" variant="link" className="w-full justify-center" onClick={onNavigateRuns}>
          <span>View activity</span>
          <ArrowRight className="size-4" />
        </Button>
      </div>
    </div>
  );
}

function Section({ title, rows, empty }: { title: string; rows: NotificationRow[]; empty: string }) {
  return (
    <section>
      <header className="mb-2 flex items-center justify-between gap-2">
        <h3 className="text-xs font-medium uppercase text-muted-foreground">{title}</h3>
        <span className="rounded-full border px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground">
          {rows.length}
        </span>
      </header>
      {rows.length === 0 ? (
        <p className="rounded-md border border-dashed p-3 text-sm text-muted-foreground">{empty}</p>
      ) : (
        <div className="min-w-0 space-y-2">
          {rows.map((row) => (
            <article key={row.key} className="min-w-0 overflow-hidden rounded-lg border bg-card p-3">
              <div className="flex min-w-0 items-start gap-2">
                <div className="min-w-0 flex-1">
                  <h4 className="min-w-0 font-mono text-sm font-medium leading-snug [overflow-wrap:anywhere]">
                    {row.title}
                  </h4>
                  <p className="mt-1 line-clamp-2 min-w-0 text-xs text-muted-foreground [overflow-wrap:anywhere]">
                    {row.detail}
                  </p>
                </div>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-xs"
                  className="shrink-0"
                  aria-label="Dismiss"
                  onClick={row.onDismiss}
                >
                  <X className="size-3.5" />
                </Button>
              </div>
              <Button type="button" size="sm" className="mt-3" onClick={row.onAction}>
                {row.actionIcon === 'reload' ? <RotateCw className="size-3.5" /> : null}
                {row.actionLabel}
              </Button>
            </article>
          ))}
        </div>
      )}
    </section>
  );
}
