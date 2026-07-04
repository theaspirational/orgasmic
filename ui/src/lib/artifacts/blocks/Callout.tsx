import { AlertTriangle, CheckCircle2, Info, Lightbulb, ShieldAlert } from 'lucide-react';

import { cn } from '@/lib/utils';
import type { MdxNode } from '../types';
import { textBody } from '../parseMdx';
import { asString } from './propUtils';
import { Markdown } from './Markdown';

type Tone = 'info' | 'decision' | 'risk' | 'warning' | 'success';

const TONE_CONFIG: Record<Tone, { icon: typeof Info; className: string }> = {
  info: { icon: Info, className: 'border-border bg-muted/40 text-foreground' },
  decision: { icon: Lightbulb, className: 'border-primary/30 bg-primary/5 text-foreground' },
  risk: { icon: ShieldAlert, className: 'border-destructive/30 bg-destructive/5 text-foreground' },
  warning: { icon: AlertTriangle, className: 'border-[color-mix(in_oklab,var(--warn)_40%,var(--border))] bg-[color-mix(in_oklab,var(--warn)_10%,transparent)] text-foreground' },
  success: { icon: CheckCircle2, className: 'border-[color-mix(in_oklab,var(--ok)_40%,var(--border))] bg-[color-mix(in_oklab,var(--ok)_10%,transparent)] text-foreground' },
};

function toneOf(value: string): Tone {
  return (['info', 'decision', 'risk', 'warning', 'success'] as const).includes(value as Tone)
    ? (value as Tone)
    : 'info';
}

export function Callout({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const tone = toneOf(asString(node.props.tone, 'info'));
  const { icon: Icon, className } = TONE_CONFIG[tone];
  const body = textBody(node, 'body');
  return (
    <div className={cn('flex gap-2.5 rounded-lg border p-3', className)}>
      <Icon className="mt-0.5 size-4 shrink-0" />
      <Markdown text={body} className="min-w-0 flex-1" />
    </div>
  );
}
