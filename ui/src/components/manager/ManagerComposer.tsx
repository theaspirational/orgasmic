import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
  type ReactNode,
} from 'react';
import { Loader2, Send } from 'lucide-react';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { fetchSkills } from '@/lib/api';
import type { SkillSummary } from '@/lib/types';
import { cn } from '@/lib/utils';

import type { TmuxPaneConnectionState } from './ManagerTmuxPane';

type ManagerSendInput = (text: string) => boolean | Promise<boolean>;
type SlashRange = { start: number; end: number; query: string };
type SkillSuggestion = {
  key: string;
  command: string;
  skill: SkillSummary;
  source: 'shipped' | 'user' | 'skill';
};

const MAX_SKILL_SUGGESTIONS = 8;

function slashRangeAt(value: string, cursor: number | null | undefined): SlashRange | null {
  if (cursor === null || cursor === undefined) return null;
  const before = value.slice(0, cursor);
  const match = /(^|\s)(\/[^\s]*)$/.exec(before);
  if (!match) return null;
  const token = match[2];
  const start = cursor - token.length;
  return { start, end: cursor, query: token.slice(1).toLowerCase() };
}

function skillSource(skill: SkillSummary): SkillSuggestion['source'] {
  const path = `${skill.source_path ?? ''} ${skill.absolute_path ?? ''}`;
  if (path.includes('/shipped/skills/')) return 'shipped';
  if (path.includes('/user/skills/')) return 'user';
  return 'skill';
}

function slashCommands(skill: SkillSummary): string[] {
  const commands = skill.triggers.filter((trigger) => trigger.startsWith('/'));
  return commands.length > 0 ? commands : [`/${skill.id}`];
}

function buildSkillSuggestions(skills: SkillSummary[], query: string): SkillSuggestion[] {
  const normalizedQuery = query.toLowerCase();
  const suggestions = skills.flatMap((skill) =>
    slashCommands(skill).map((command) => ({
      key: `${skill.id}:${command}`,
      command,
      skill,
      source: skillSource(skill),
    })),
  );

  return suggestions
    .filter(({ command, skill }) => {
      if (!normalizedQuery) return true;
      const haystack = [
        command.slice(1),
        skill.id,
        skill.title,
        skill.description ?? '',
      ]
        .join(' ')
        .toLowerCase();
      return haystack.includes(normalizedQuery);
    })
    .sort((a, b) => {
      const aExact = a.command.slice(1).startsWith(normalizedQuery) ? 0 : 1;
      const bExact = b.command.slice(1).startsWith(normalizedQuery) ? 0 : 1;
      return aExact - bExact || a.command.localeCompare(b.command);
    })
    .slice(0, MAX_SKILL_SUGGESTIONS);
}

export function ManagerComposer({
  runId,
  connectionState,
  onSend,
  initialDraft,
  placeholder = 'Send a message to manager',
  readyLabel = 'Enter sends. Shift+Enter adds a line. Arrow-up recalls the last send.',
  unavailableLabel,
  onSent,
  controls,
}: {
  runId: string | null;
  connectionState: TmuxPaneConnectionState;
  onSend: ManagerSendInput;
  initialDraft?: string | null;
  placeholder?: string;
  readyLabel?: string;
  unavailableLabel?: string;
  onSent?: (sentAt: string) => void;
  controls?: ReactNode;
}) {
  const [draft, setDraft] = useState('');
  const [history, setHistory] = useState<string[]>([]);
  const [sendError, setSendError] = useState<string | null>(null);
  const [sendBusy, setSendBusy] = useState(false);
  const [slashRange, setSlashRange] = useState<SlashRange | null>(null);
  const [skills, setSkills] = useState<SkillSummary[]>([]);
  const [skillsLoaded, setSkillsLoaded] = useState(false);
  const [skillsLoading, setSkillsLoading] = useState(false);
  const [skillsError, setSkillsError] = useState<string | null>(null);
  const [activeSuggestion, setActiveSuggestion] = useState(0);
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);
  const draftKeyRef = useRef<string | null>(null);
  const suggestionListRef = useRef<HTMLDivElement | null>(null);
  const suggestionOptionRefs = useRef<Array<HTMLButtonElement | null>>([]);
  const disabled = !runId || connectionState !== 'open' || sendBusy;
  const canSubmit = !disabled && draft.trim().length > 0;
  const slashActive = slashRange !== null;
  const skillSuggestions = useMemo(
    () => (slashRange ? buildSkillSuggestions(skills, slashRange.query) : []),
    [skills, slashRange],
  );
  const slashSuggestionsOpen = Boolean(
    slashRange && (skillsLoading || skillsError || skillsLoaded || skillSuggestions.length > 0),
  );

  useEffect(() => {
    const key = `${runId ?? 'none'}:${initialDraft ?? ''}`;
    if (draftKeyRef.current === key) return;
    draftKeyRef.current = key;
    setDraft(initialDraft ?? '');
    setSendError(null);
    setSlashRange(null);
    window.requestAnimationFrame(() => resizeTextarea(textareaRef.current));
  }, [initialDraft, runId]);

  useEffect(() => {
    if (!slashActive || skillsLoaded) return undefined;
    let cancelled = false;
    setSkillsLoading(true);
    setSkillsError(null);
    void fetchSkills()
      .then((next) => {
        if (cancelled) return;
        setSkills([...next].sort((a, b) => a.id.localeCompare(b.id)));
      })
      .catch((err) => {
        if (cancelled) return;
        setSkillsError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (cancelled) return;
        setSkillsLoaded(true);
        setSkillsLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [slashActive, skillsLoaded]);

  useEffect(() => {
    setActiveSuggestion(0);
  }, [slashRange?.query, skillSuggestions.length]);

  useEffect(() => {
    suggestionOptionRefs.current = suggestionOptionRefs.current.slice(0, skillSuggestions.length);
  }, [skillSuggestions.length]);

  useEffect(() => {
    if (!slashSuggestionsOpen || skillSuggestions.length === 0) return;
    const list = suggestionListRef.current;
    const option = suggestionOptionRefs.current[activeSuggestion];
    if (!list || !option) return;

    const listRect = list.getBoundingClientRect();
    const optionRect = option.getBoundingClientRect();

    if (optionRect.top < listRect.top) {
      list.scrollTop -= listRect.top - optionRect.top;
    } else if (optionRect.bottom > listRect.bottom) {
      list.scrollTop += optionRect.bottom - listRect.bottom;
    }
  }, [activeSuggestion, skillSuggestions.length, slashSuggestionsOpen]);

  async function submit() {
    if (!canSubmit) return;
    const text = draft.trim();
    const sentAt = new Date().toISOString();
    setSendBusy(true);
    try {
      const sent = await onSend(text);
      if (!sent) {
        setSendError('Manager input channel is not connected.');
        return;
      }
      setHistory((prev) => [...prev, text]);
      setDraft('');
      setSlashRange(null);
      setSendError(null);
      window.requestAnimationFrame(() => resizeTextarea(textareaRef.current));
      onSent?.(sentAt);
    } catch (err) {
      setSendError(err instanceof Error ? err.message : String(err));
    } finally {
      setSendBusy(false);
    }
  }

  function handleKeyDown(event: KeyboardEvent<HTMLTextAreaElement>) {
    if (slashSuggestionsOpen) {
      if (event.key === 'Escape') {
        event.preventDefault();
        setSlashRange(null);
        return;
      }
      if (skillSuggestions.length > 0 && event.key === 'ArrowDown') {
        event.preventDefault();
        setActiveSuggestion((current) => (current + 1) % skillSuggestions.length);
        return;
      }
      if (skillSuggestions.length > 0 && event.key === 'ArrowUp') {
        event.preventDefault();
        setActiveSuggestion((current) =>
          current === 0 ? skillSuggestions.length - 1 : current - 1,
        );
        return;
      }
      if (
        skillSuggestions.length > 0 &&
        (event.key === 'Enter' || event.key === 'Tab')
      ) {
        event.preventDefault();
        applySlashSuggestion(skillSuggestions[activeSuggestion] ?? skillSuggestions[0]);
        return;
      }
    }
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      void submit();
      return;
    }
    if (event.key === 'ArrowUp' && draft.length === 0 && history.length > 0) {
      event.preventDefault();
      setDraft(history[history.length - 1] ?? '');
      window.requestAnimationFrame(() => {
        const textarea = textareaRef.current;
        if (!textarea) return;
        textarea.selectionStart = textarea.value.length;
        textarea.selectionEnd = textarea.value.length;
      });
    }
  }

  function updateSlashRange(value: string, cursor: number | null | undefined) {
    setSlashRange(slashRangeAt(value, cursor));
  }

  function applySlashSuggestion(suggestion: SkillSuggestion) {
    if (!slashRange) return;
    const after = draft.slice(slashRange.end);
    const needsTrailingSpace = after.length === 0 || !/^\s/.test(after);
    const next =
      draft.slice(0, slashRange.start) +
      suggestion.command +
      (needsTrailingSpace ? ' ' : '') +
      after;
    const cursor = slashRange.start + suggestion.command.length + (needsTrailingSpace ? 1 : 0);

    setDraft(next);
    setSlashRange(null);
    window.requestAnimationFrame(() => {
      const textarea = textareaRef.current;
      if (!textarea) return;
      textarea.focus();
      textarea.selectionStart = cursor;
      textarea.selectionEnd = cursor;
    });
  }

  return (
    <form
      className="flex h-full min-h-0 flex-col overflow-visible rounded-xl border bg-card shadow-sm outline-none transition-[border-color,box-shadow] focus-within:border-ring focus-within:ring-3 focus-within:ring-ring/20"
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      <div className="relative min-h-0 flex-1">
        <textarea
          ref={textareaRef}
          aria-label={placeholder}
          className="max-h-40 min-h-20 w-full resize-none bg-transparent px-3.5 pb-2 pt-3 text-sm leading-relaxed outline-none placeholder:text-muted-foreground focus-visible:ring-0 disabled:cursor-not-allowed disabled:opacity-60"
          value={draft}
          rows={3}
          disabled={disabled}
          placeholder={placeholder}
          onChange={(event) => {
            const next = event.currentTarget.value;
            setDraft(next);
            resizeTextarea(event.currentTarget);
            updateSlashRange(next, event.currentTarget.selectionStart);
          }}
          onClick={(event) => updateSlashRange(draft, event.currentTarget.selectionStart)}
          onFocus={(event) => updateSlashRange(draft, event.currentTarget.selectionStart)}
          onKeyDown={handleKeyDown}
          onSelect={(event) => updateSlashRange(draft, event.currentTarget.selectionStart)}
          onBlur={() => {
            window.setTimeout(() => {
              if (document.activeElement !== textareaRef.current) setSlashRange(null);
            }, 0);
          }}
        />
        {slashSuggestionsOpen ? (
          <div
            className="absolute bottom-full left-0 right-0 z-50 mb-2 overflow-hidden rounded-lg border bg-popover text-popover-foreground shadow-lg"
            onMouseDown={(event) => event.preventDefault()}
          >
            <div className="flex items-center justify-between border-b px-2.5 py-1.5 text-xs text-muted-foreground">
              <span>Skills</span>
              <span className="font-mono">/{slashRange?.query ?? ''}</span>
            </div>
            {skillsLoading ? (
              <div className="px-2.5 py-2 text-xs text-muted-foreground">Loading skills...</div>
            ) : skillsError ? (
              <div className="px-2.5 py-2 text-xs text-destructive">{skillsError}</div>
            ) : skillSuggestions.length === 0 ? (
              <div className="px-2.5 py-2 text-xs text-muted-foreground">No matching skills</div>
            ) : (
              <div
                ref={suggestionListRef}
                className="max-h-56 overflow-y-auto overscroll-contain p-1"
                role="listbox"
                aria-label="Skill suggestions"
              >
                {skillSuggestions.map((suggestion, index) => (
                  <button
                    key={suggestion.key}
                    ref={(element) => {
                      suggestionOptionRefs.current[index] = element;
                    }}
                    type="button"
                    role="option"
                    aria-selected={index === activeSuggestion}
                    className={cn(
                      'flex w-full min-w-0 items-start gap-2 rounded-sm px-2 py-2 text-left text-sm outline-none transition-colors',
                      index === activeSuggestion ? 'bg-accent text-accent-foreground' : null,
                    )}
                    onClick={() => applySlashSuggestion(suggestion)}
                  >
                    <span className="min-w-0 flex-1">
                      <span className="flex min-w-0 items-center gap-2">
                        <span className="truncate font-mono text-xs font-medium">
                          {suggestion.command}
                        </span>
                        <Badge variant="outline" className="h-4 px-1.5 text-[10px]">
                          {suggestion.source}
                        </Badge>
                      </span>
                      <span className="mt-0.5 block truncate text-xs text-muted-foreground">
                        {suggestion.skill.description ?? suggestion.skill.title}
                      </span>
                    </span>
                  </button>
                ))}
              </div>
            )}
          </div>
        ) : null}
      </div>
      {controls ? <div className="border-t px-3 py-2">{controls}</div> : null}
      <div className="flex shrink-0 items-center gap-2 px-2.5 pb-2.5 pt-1">
        <span
          className="hidden min-w-0 flex-1 truncate text-xs text-muted-foreground sm:block"
          aria-live="polite"
        >
          {disabled
            ? sendBusy
              ? 'Sending...'
              : connectionState === 'connecting'
              ? 'Connecting terminal...'
              : (unavailableLabel ?? 'No manager run attached.')
            : readyLabel}
        </span>
        <span
          className="min-w-0 flex-1 truncate text-xs text-muted-foreground sm:hidden"
          aria-live="polite"
        >
          {sendBusy
            ? 'Sending…'
            : disabled
              ? (unavailableLabel ?? 'Agent unavailable')
              : 'Enter to send'}
        </span>
        <Button
          type="button"
          size="icon"
          className="size-9"
          aria-label={sendBusy ? 'Sending message' : 'Send message'}
          title={sendBusy ? 'Sending message' : 'Send message'}
          disabled={!canSubmit}
          onClick={() => void submit()}
        >
          {sendBusy ? (
            <Loader2 className="size-4 animate-spin motion-reduce:animate-none" />
          ) : (
            <Send className="size-4" />
          )}
        </Button>
      </div>
      {sendError ? <p className="px-3 pb-2.5 text-xs text-destructive">{sendError}</p> : null}
    </form>
  );
}

function resizeTextarea(textarea: HTMLTextAreaElement | null) {
  if (!textarea) return;
  textarea.style.height = 'auto';
  textarea.style.height = `${Math.min(textarea.scrollHeight, 160)}px`;
}
