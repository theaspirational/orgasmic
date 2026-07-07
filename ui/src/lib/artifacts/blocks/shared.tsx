import { Component, useEffect, useRef, type ErrorInfo, type ReactNode, type RefObject } from 'react';
import { AlertTriangle } from 'lucide-react';

import { cn } from '@/lib/utils';

/** Consistent outer shell most blocks render inside. */
export function BlockCard({
  children,
  className,
  padded = true,
}: {
  children: ReactNode;
  className?: string;
  padded?: boolean;
}) {
  return (
    <div className={cn('rounded-lg border bg-card', padded && 'p-4', className)}>{children}</div>
  );
}

export function BlockTitle({ children }: { children: ReactNode }) {
  if (!children) return null;
  return <h3 className="mb-2 text-sm font-semibold text-foreground">{children}</h3>;
}

/** Rendered in place of a block that failed to parse or is unknown — one bad
 * block must never blank the rest of the document. */
export function UnrenderableBlock({ name, message }: { name?: string; message: string }) {
  return (
    <div
      role="alert"
      className="flex items-start gap-2 rounded-lg border border-dashed border-destructive/40 bg-destructive/5 p-3 text-sm text-destructive"
    >
      <AlertTriangle className="mt-0.5 size-4 shrink-0" />
      <div className="min-w-0">
        <p className="font-medium">Unrenderable block{name ? ` — ${name}` : ''}</p>
        <p className="mt-0.5 text-xs text-destructive/80">{message}</p>
      </div>
    </div>
  );
}

type BoundaryProps = { name?: string; children: ReactNode };
type BoundaryState = { error: Error | null };

/** Catches a render-phase throw from exactly one block component. JSX
 * creation (`<Component node={node} />`) is lazy — React only calls the
 * component function during reconciliation — so a plain try/catch around
 * element creation cannot see this error; only a real error boundary can.
 * Without this, one buggy/malformed block would blank the whole document. */
export class BlockErrorBoundary extends Component<BoundaryProps, BoundaryState> {
  state: BoundaryState = { error: null };

  static getDerivedStateFromError(error: Error): BoundaryState {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    console.error(`Artifact block "${this.props.name ?? 'unknown'}" failed to render`, error, info);
  }

  render(): ReactNode {
    if (this.state.error) {
      return <UnrenderableBlock name={this.props.name} message={this.state.error.message} />;
    }
    return this.props.children;
  }
}

const SURFACE_FRAME: Record<string, string> = {
  browser: 'w-full max-w-[640px] aspect-[16/10] rounded-lg',
  desktop: 'w-full max-w-[760px] aspect-[16/10] rounded-md',
  mobile: 'w-full max-w-[280px] aspect-[9/19] rounded-[1.75rem]',
  popover: 'w-full max-w-[260px] rounded-md',
  panel: 'w-full max-w-[340px] rounded-md',
};

export type WireframeSurface = 'browser' | 'desktop' | 'mobile' | 'popover' | 'panel';

export function isWireframeSurface(value: unknown): value is WireframeSurface {
  return typeof value === 'string' && value in SURFACE_FRAME;
}

/** Fixed pixel footprint per surface, used only by Canvas board mode (see
 * Canvas.tsx). SURFACE_FRAME's `max-w-*`/responsive classes give a fluid
 * gallery layout no fixed box to anchor connectors/annotations to, so board
 * mode needs a deterministic width up front; height follows the same
 * aspect ratio SURFACE_FRAME uses for browser/desktop/mobile, and is an
 * estimate for popover/panel (content-sized surfaces) used only to size the
 * board container before the real DOM rect is measured post-mount. */
export const SURFACE_BOARD_SIZE: Record<WireframeSurface, { width: number; height: number }> = {
  browser: { width: 460, height: 288 },
  desktop: { width: 520, height: 325 },
  mobile: { width: 220, height: 464 },
  popover: { width: 200, height: 140 },
  panel: { width: 240, height: 220 },
};

/** The chrome around a wireframe/canvas artboard: the surface preset picks
 * footprint + aspect (never author-controlled width/height/coordinates, per
 * wireframe.md), with a minimal browser/mobile chrome bar and no decorative
 * shadows. */
export function WireframeFrame({
  surface,
  html,
  skeleton,
  label,
  className,
}: {
  surface: WireframeSurface;
  html: string;
  skeleton?: boolean;
  label?: string;
  className?: string;
}) {
  const containerRef = useRef<HTMLDivElement>(null);
  useRoughOverlay(containerRef, html);

  return (
    <div className={cn('flex flex-col gap-1.5', className)}>
      {label ? <h4 className="text-xs font-medium text-muted-foreground">{label}</h4> : null}
      <div
        className={cn(
          'orgasmic-wireframe overflow-hidden border border-border bg-[var(--wf-paper)]',
          SURFACE_FRAME[surface] ?? SURFACE_FRAME.browser,
          skeleton && 'wf-skeleton',
        )}
      >
        {surface === 'browser' ? (
          <div className="flex h-6 shrink-0 items-center gap-1.5 border-b border-[var(--wf-line)] bg-[var(--wf-card)] px-2.5">
            <span className="size-1.5 rounded-full bg-[var(--wf-line)]" />
            <span className="size-1.5 rounded-full bg-[var(--wf-line)]" />
            <span className="size-1.5 rounded-full bg-[var(--wf-line)]" />
            <span className="ml-2 h-3 flex-1 rounded-sm bg-[var(--wf-paper)]" />
          </div>
        ) : null}
        {surface === 'mobile' ? (
          <div className="flex h-4 shrink-0 items-center justify-center bg-[var(--wf-card)]">
            <span className="h-1 w-10 rounded-full bg-[var(--wf-line)]" />
          </div>
        ) : null}
        <div
          ref={containerRef}
          className="relative h-[calc(100%-0px)] flex-1 overflow-auto"
          // Sanitized upstream (renderableFragment) before ever reaching this
          // component — see sanitize.ts. Never pass raw author HTML here.
          dangerouslySetInnerHTML={{ __html: html }}
        />
      </div>
    </div>
  );
}

/** Sparse rough.js overlay for elements marked `data-rough` — a hand-drawn
 * rectangle traced over the element's box. Defensive: any failure (missing
 * layout, import failure) is a silent no-op, never a render error. */
export function useRoughOverlay(containerRef: RefObject<HTMLDivElement | null>, contentKey: string): void {
  useEffect(() => {
    let cancelled = false;
    const container = containerRef.current;
    if (!container) return undefined;

    const timer = window.setTimeout(() => {
      void (async () => {
        try {
          const targets = Array.from(container.querySelectorAll<HTMLElement>('[data-rough]'));
          if (targets.length === 0 || cancelled) return;
          const { default: rough } = await import('roughjs');
          if (cancelled) return;
          for (const el of targets) {
            if (el.querySelector(':scope > svg[data-rough-overlay]')) continue;
            const rect = el.getBoundingClientRect();
            if (rect.width < 4 || rect.height < 4) continue;
            const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
            svg.setAttribute('data-rough-overlay', 'true');
            svg.setAttribute('width', String(rect.width));
            svg.setAttribute('height', String(rect.height));
            svg.style.position = 'absolute';
            svg.style.inset = '0';
            svg.style.pointerEvents = 'none';
            const ink = getComputedStyle(el).color || 'currentColor';
            const rc = rough.svg(svg);
            const node = rc.rectangle(1, 1, rect.width - 2, rect.height - 2, {
              stroke: ink,
              roughness: 1.6,
              strokeWidth: 1.1,
              fill: 'none',
            });
            svg.appendChild(node);
            if (getComputedStyle(el).position === 'static') el.style.position = 'relative';
            el.appendChild(svg);
          }
        } catch {
          // Cosmetic only — never let a sketch-overlay failure surface.
        }
      })();
    }, 30);

    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [contentKey]);
}
