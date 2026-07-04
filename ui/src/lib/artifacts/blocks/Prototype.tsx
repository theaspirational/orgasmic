import { useState } from 'react';

import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';
import type { MdxNode } from '../types';
import { textBody } from '../parseMdx';
import { asOptionalString, asString } from './propUtils';
import { UnrenderableBlock } from './shared';

/**
 * Live/interactive HTML renders in a sandboxed iframe with no
 * `allow-same-origin` — the isolated opaque origin is what makes it safe to
 * skip DOMPurify here (unlike Wireframe/Diagram): the whole point of
 * Prototype is that its `<script>`/interactivity survives, which sanitizing
 * would defeat, and the sandbox boundary is what stops that script from ever
 * touching the parent app's DOM, cookies, or storage. Cross-screen
 * navigation (`data-goto`) is the interactive loop owned by TASK-EDQPG; this
 * stage renders each screen and a plain tab switcher between them.
 */
export function Prototype({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const screens = node.children.filter(
    (child): child is Extract<MdxNode, { kind: 'element' }> => child.kind === 'element' && child.name === 'Screen',
  );
  const startId = asOptionalString(node.props.start);
  const [activeIndex, setActiveIndex] = useState(() => {
    const found = startId ? screens.findIndex((s) => asString(s.props.id) === startId) : -1;
    return found >= 0 ? found : 0;
  });

  if (screens.length === 0) {
    return <UnrenderableBlock name="Prototype" message="no <Screen> screens found" />;
  }
  const active = screens[activeIndex];
  const html = active ? textBody(active, 'html') : '';

  return (
    <div className="flex flex-col gap-2 rounded-lg border bg-card p-3">
      {screens.length > 1 ? (
        <div className="flex flex-wrap gap-1.5">
          {screens.map((screen, index) => {
            const label = asOptionalString(screen.props.label) ?? asString(screen.props.id, `Screen ${index + 1}`);
            return (
              <Button
                key={index}
                type="button"
                size="sm"
                variant={index === activeIndex ? 'default' : 'outline'}
                onClick={() => setActiveIndex(index)}
                className={cn(index === activeIndex && 'pointer-events-none')}
              >
                {label}
              </Button>
            );
          })}
        </div>
      ) : null}
      <iframe
        title={asOptionalString(active?.props.label) ?? 'Prototype screen'}
        srcDoc={html}
        sandbox="allow-scripts"
        className="h-80 w-full resize-y overflow-auto rounded-md border bg-background"
      />
    </div>
  );
}
