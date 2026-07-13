import { useMemo, useState } from 'react';
import { Circle, Loader2, Square, Star, ThumbsUp } from 'lucide-react';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Textarea } from '@/components/ui/textarea';
import type { CommentRecord } from '@/lib/types';
import { cn } from '@/lib/utils';
import type { AttrValue, MdxNode } from '../types';
import { useArtifactInteraction, type ArtifactInteraction } from '../interaction';
import { questionKey } from '../questionKey';
import {
  agreeAuthors,
  buildAnswerMessage,
  isAnswerComplete,
  latestAnswersPerAuthor,
  type AnswerSelection,
} from '../questionAnswers';
import { asArray, asBool, asOptionalString, asRecord, asString } from './propUtils';
import { BlockCard } from './shared';

type QuestionOption = { label: string; detail?: string; recommended: boolean };
type Question = {
  type: 'single' | 'multi' | 'freeform';
  prompt: string;
  options: QuestionOption[];
  allowOther: boolean;
};

function readOption(raw: AttrValue): QuestionOption {
  const record = asRecord(raw);
  return {
    label: asString(record.label, asString(raw, '')),
    detail: asOptionalString(record.detail),
    recommended: asBool(record.recommended),
  };
}

function readQuestion(raw: AttrValue): Question {
  const record = asRecord(raw);
  const type = asString(record.type, 'single');
  return {
    type: type === 'multi' || type === 'freeform' ? type : 'single',
    prompt: asString(record.prompt),
    options: asArray(record.options).map(readOption),
    allowOther: asBool(record.allowOther, true),
  };
}

/** QuestionForm renders the questions read-only by default. When an
 * `ArtifactInteractionContext` is present (only inside `ArtifactComments`) and
 * the viewer may answer, each question becomes interactive: answers post as
 * normal artifact comments carrying a structured question anchor, and existing
 * teammates' answers (with Agree) are shown inline. Absent the context — every
 * other embed, fixtures, tests — it renders exactly as before. */
export function QuestionForm({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const title = asString(node.props.title, 'Open Questions');
  const questions = asArray(node.props.questions).map(readQuestion);
  const interaction = useArtifactInteraction();
  if (questions.length === 0) return null;

  return (
    <BlockCard className="flex flex-col gap-4">
      <h3 className="text-sm font-semibold">{title}</h3>
      {questions.map((question, index) => {
        const key = questionKey(question.prompt);
        return (
          <div
            key={index}
            data-question-key={key}
            className="flex flex-col gap-2 border-t pt-3 first:border-t-0 first:pt-0"
          >
            <p className="text-sm font-medium">{question.prompt}</p>
            {interaction?.canAnswer ? (
              <InteractiveQuestion question={question} questionKey={key} interaction={interaction} />
            ) : (
              <ReadOnlyQuestion question={question} />
            )}
            {interaction ? (
              <ExistingAnswers questionKey={key} interaction={interaction} />
            ) : null}
          </div>
        );
      })}
    </BlockCard>
  );
}

/** The original read-only presentation: option rows and inert placeholders. */
function ReadOnlyQuestion({ question }: { question: Question }) {
  const Icon = question.type === 'multi' ? Square : Circle;
  if (question.type === 'freeform') {
    return (
      <div className="rounded-md border border-dashed bg-muted/20 px-3 py-2 text-xs text-muted-foreground">
        Free-form answer
      </div>
    );
  }
  return (
    <div className="flex flex-col gap-1.5">
      {question.options.map((option, optionIndex) => (
        <div
          key={optionIndex}
          className="flex items-start gap-2 rounded-md border bg-muted/10 px-2.5 py-1.5"
        >
          <Icon className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />
          <div className="min-w-0 flex-1">
            <div className="flex flex-wrap items-center gap-1.5">
              <span className={cn('text-sm', option.recommended && 'font-medium')}>{option.label}</span>
              {option.recommended ? (
                <Badge variant="outline" className="gap-1 text-[0.65rem]">
                  <Star className="size-2.5" /> Recommended
                </Badge>
              ) : null}
            </div>
            {option.detail ? <p className="text-xs text-muted-foreground">{option.detail}</p> : null}
          </div>
        </div>
      ))}
      {question.allowOther ? (
        <div className="rounded-md border border-dashed bg-muted/10 px-2.5 py-1.5 text-xs text-muted-foreground">
          Other (write-in)
        </div>
      ) : null}
    </div>
  );
}

const OTHER_VALUE = '__other__';

/** Interactive answer control for one question. Holds its own selection state
 * and posts through the interaction bridge. */
function InteractiveQuestion({
  question,
  questionKey: key,
  interaction,
}: {
  question: Question;
  questionKey: string;
  interaction: ArtifactInteraction;
}) {
  const [single, setSingle] = useState<string | null>(null);
  const [multi, setMulti] = useState<Set<string>>(() => new Set());
  const [otherChecked, setOtherChecked] = useState(false);
  const [otherText, setOtherText] = useState('');
  const [freeform, setFreeform] = useState('');
  const [posting, setPosting] = useState(false);

  const selection = buildSelection(question, {
    single,
    multi,
    otherChecked,
    otherText,
    freeform,
  });
  const complete = isAnswerComplete(selection);

  async function submit() {
    if (!complete || posting) return;
    setPosting(true);
    try {
      await interaction.submitAnswer({
        questionKey: key,
        prompt: question.prompt,
        message: buildAnswerMessage(selection),
      });
      setSingle(null);
      setMulti(new Set());
      setOtherChecked(false);
      setOtherText('');
      setFreeform('');
    } catch {
      // Failure is surfaced via toast by the interaction bridge; keep the
      // in-progress answer so the member can retry.
    } finally {
      setPosting(false);
    }
  }

  const otherActive = question.type === 'single' ? single === OTHER_VALUE : otherChecked;

  return (
    <form
      className="flex flex-col gap-2"
      aria-label={`Answer: ${question.prompt}`}
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      {question.type === 'freeform' ? (
        <Textarea
          rows={3}
          value={freeform}
          disabled={posting}
          aria-label="Free-form answer"
          placeholder="Type your answer…"
          onChange={(event) => setFreeform(event.target.value)}
        />
      ) : (
        <div className="flex flex-col gap-1.5">
          {question.options.map((option, optionIndex) => (
            <label
              key={optionIndex}
              className="flex cursor-pointer items-start gap-2 rounded-md border bg-muted/10 px-2.5 py-1.5"
            >
              <input
                type={question.type === 'multi' ? 'checkbox' : 'radio'}
                name={`q-${key}`}
                className="mt-0.5 size-3.5 shrink-0 accent-primary"
                disabled={posting}
                checked={
                  question.type === 'multi' ? multi.has(option.label) : single === option.label
                }
                onChange={(event) => {
                  if (question.type === 'multi') {
                    setMulti((prev) => {
                      const next = new Set(prev);
                      if (event.target.checked) next.add(option.label);
                      else next.delete(option.label);
                      return next;
                    });
                  } else {
                    setSingle(option.label);
                  }
                }}
              />
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-1.5">
                  <span className={cn('text-sm', option.recommended && 'font-medium')}>{option.label}</span>
                  {option.recommended ? (
                    <Badge variant="outline" className="gap-1 text-[0.65rem]">
                      <Star className="size-2.5" /> Recommended
                    </Badge>
                  ) : null}
                </div>
                {option.detail ? <p className="text-xs text-muted-foreground">{option.detail}</p> : null}
              </div>
            </label>
          ))}
          {question.allowOther ? (
            <div className="flex flex-col gap-1.5 rounded-md border bg-muted/10 px-2.5 py-1.5">
              <label className="flex cursor-pointer items-center gap-2">
                <input
                  type={question.type === 'multi' ? 'checkbox' : 'radio'}
                  name={`q-${key}`}
                  className="size-3.5 shrink-0 accent-primary"
                  disabled={posting}
                  checked={otherActive}
                  onChange={(event) => {
                    if (question.type === 'multi') {
                      setOtherChecked(event.target.checked);
                    } else {
                      setSingle(OTHER_VALUE);
                    }
                  }}
                />
                <span className="text-sm">Other</span>
              </label>
              {otherActive ? (
                <Input
                  value={otherText}
                  disabled={posting}
                  aria-label="Other answer"
                  placeholder="Write in an answer…"
                  onChange={(event) => setOtherText(event.target.value)}
                />
              ) : null}
            </div>
          ) : null}
        </div>
      )}
      <div className="flex justify-end">
        <Button type="submit" size="sm" disabled={!complete || posting}>
          {posting ? <Loader2 className="size-3.5 animate-spin" /> : null}
          {posting ? 'Submitting…' : 'Submit'}
        </Button>
      </div>
    </form>
  );
}

function buildSelection(
  question: Question,
  state: {
    single: string | null;
    multi: Set<string>;
    otherChecked: boolean;
    otherText: string;
    freeform: string;
  },
): AnswerSelection {
  if (question.type === 'freeform') return { type: 'freeform', text: state.freeform };
  if (question.type === 'single') {
    if (state.single === OTHER_VALUE) return { type: 'single', label: null, other: state.otherText };
    return { type: 'single', label: state.single, other: null };
  }
  return {
    type: 'multi',
    labels: question.options.map((o) => o.label).filter((label) => state.multi.has(label)),
    other: state.otherChecked ? state.otherText : null,
  };
}

/** Teammates' current-version answers to one question, latest per author, each
 * with an Agree affordance and the names of members who agreed. */
function ExistingAnswers({
  questionKey: key,
  interaction,
}: {
  questionKey: string;
  interaction: ArtifactInteraction;
}) {
  const [agreeing, setAgreeing] = useState<string | null>(null);
  const answers = useMemo(
    () => latestAnswersPerAuthor(interaction.comments, key),
    [interaction.comments, key],
  );
  if (answers.length === 0) return null;

  async function agree(comment: CommentRecord) {
    setAgreeing(comment.cid);
    try {
      await interaction.agree({ cid: comment.cid });
    } catch {
      // Surfaced via toast by the interaction bridge.
    } finally {
      setAgreeing(null);
    }
  }

  return (
    <div className="flex flex-col gap-1.5">
      {answers.map((answer) => {
        const agreed = agreeAuthors(interaction.comments, answer.cid);
        return (
          <div key={answer.cid} className="rounded-md border bg-card/40 px-2.5 py-1.5">
            <div className="flex items-center justify-between gap-2">
              <span className="truncate text-xs font-semibold">{answer.author}</span>
              {interaction.canAnswer ? (
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  className="h-6 gap-1 px-2 text-xs"
                  disabled={agreeing === answer.cid}
                  onClick={() => void agree(answer)}
                >
                  {agreeing === answer.cid ? (
                    <Loader2 className="size-3 animate-spin" />
                  ) : (
                    <ThumbsUp className="size-3" />
                  )}
                  Agree
                </Button>
              ) : null}
            </div>
            <p className="whitespace-pre-wrap text-sm leading-snug">{answer.message}</p>
            {agreed.length > 0 ? (
              <p className="mt-0.5 text-xs text-muted-foreground">Agreed: {agreed.join(', ')}</p>
            ) : null}
          </div>
        );
      })}
    </div>
  );
}
