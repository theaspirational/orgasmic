import { Circle, Square, Star } from 'lucide-react';

import { Badge } from '@/components/ui/badge';
import { cn } from '@/lib/utils';
import type { AttrValue, MdxNode } from '../types';
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

// Read-only per this task's scope: the interactive answer/submit loop is
// TASK-EDQPG. This renders the questions and their options so a reviewer can
// see what will be asked, with no submit affordance.
export function QuestionForm({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const title = asString(node.props.title, 'Open Questions');
  const questions = asArray(node.props.questions).map(readQuestion);
  if (questions.length === 0) return null;

  return (
    <BlockCard className="flex flex-col gap-4">
      <h3 className="text-sm font-semibold">{title}</h3>
      {questions.map((question, index) => {
        const Icon = question.type === 'multi' ? Square : Circle;
        return (
          <div key={index} className="flex flex-col gap-2 border-t pt-3 first:border-t-0 first:pt-0">
            <p className="text-sm font-medium">{question.prompt}</p>
            {question.type === 'freeform' ? (
              <div className="rounded-md border border-dashed bg-muted/20 px-3 py-2 text-xs text-muted-foreground">
                Free-form answer
              </div>
            ) : (
              <div className="flex flex-col gap-1.5">
                {question.options.map((option, optionIndex) => (
                  <div key={optionIndex} className="flex items-start gap-2 rounded-md border bg-muted/10 px-2.5 py-1.5">
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
            )}
          </div>
        );
      })}
    </BlockCard>
  );
}
