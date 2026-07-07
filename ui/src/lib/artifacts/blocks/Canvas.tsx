import { useId, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type RefObject } from 'react';

import type { MdxNode } from '../types';
import { textBody } from '../parseMdx';
import { renderableFragment } from '../sanitize';
import { asBool, asNumber, asOptionalString, asString } from './propUtils';
import { SURFACE_BOARD_SIZE, UnrenderableBlock, WireframeFrame, isWireframeSurface } from './shared';

type Placement = 'top' | 'right' | 'bottom' | 'left';

type ScreenSpec = {
  id: string;
  surface: string;
  label: string | undefined;
  html: string;
  skeleton: boolean;
  x: number;
  y: number;
  hasCoord: boolean;
};

type ConnectorSpec = { from: string; to: string; label?: string };
type AnnotationSpec = { targetId: string; placement: Placement; label?: string; note: string };
type Rect = { left: number; top: number; width: number; height: number };

const BOARD_SCALE = 0.42;
const BOARD_LANE_GAP = 1200;
const BOARD_PADDING = 48;

function isElement(node: MdxNode): node is Extract<MdxNode, { kind: 'element' }> {
  return node.kind === 'element';
}

function readScreens(children: MdxNode[]): ScreenSpec[] {
  const screenNodes = children.filter((child): child is Extract<MdxNode, { kind: 'element' }> =>
    isElement(child) && child.name === 'Screen',
  );

  // Auto-lane fallback: a screen authored without x/y is placed after the
  // rightmost explicit (or already-assigned) screen so far, one lane apart —
  // mixed explicit/implicit authoring never collides.
  let nextAutoX = 0;
  return screenNodes.map((screenNode, index) => {
    const hasCoord = screenNode.props.x !== undefined && screenNode.props.y !== undefined;
    const x = hasCoord ? asNumber(screenNode.props.x, 0) : nextAutoX;
    const y = hasCoord ? asNumber(screenNode.props.y, 0) : 0;
    nextAutoX = Math.max(nextAutoX, x) + BOARD_LANE_GAP;
    return {
      id: asOptionalString(screenNode.props.id) ?? `screen-${index}`,
      surface: asString(screenNode.props.surface, 'browser'),
      label: asOptionalString(screenNode.props.label),
      html: renderableFragment(textBody(screenNode, 'html')),
      skeleton: asBool(screenNode.props.skeleton),
      x,
      y,
      hasCoord,
    };
  });
}

function readConnectors(children: MdxNode[]): ConnectorSpec[] {
  return children
    .filter((child): child is Extract<MdxNode, { kind: 'element' }> => isElement(child) && child.name === 'Connector')
    .map((child) => ({
      from: asString(child.props.from),
      to: asString(child.props.to),
      label: asOptionalString(child.props.label),
    }))
    .filter((connector) => connector.from && connector.to);
}

function isPlacement(value: string): value is Placement {
  return value === 'top' || value === 'right' || value === 'bottom' || value === 'left';
}

function readAnnotations(children: MdxNode[]): AnnotationSpec[] {
  return children
    .filter((child): child is Extract<MdxNode, { kind: 'element' }> => isElement(child) && child.name === 'Annotation')
    .map((child) => {
      const placementRaw = asString(child.props.placement, 'right');
      return {
        targetId: asString(child.props.targetId),
        placement: isPlacement(placementRaw) ? placementRaw : 'right',
        label: asOptionalString(child.props.label),
        note: textBody(child, 'note'),
      };
    })
    .filter((annotation) => annotation.targetId && annotation.note);
}

/** Measures each screen's rendered box (keyed by screen id) relative to the
 * board container, re-measuring on resize/reflow (webfont load, container
 * width change) via `ResizeObserver`. Falls back to the layout-estimate box
 * callers pass in when a real rect isn't available yet (first paint) or
 * never will be (no layout engine, e.g. jsdom) — width 0 marks "not real". */
function useBoardRects(
  containerRef: RefObject<HTMLDivElement | null>,
  frameRefs: RefObject<Record<string, HTMLDivElement | null>>,
  deps: unknown[],
): Record<string, Rect> {
  const [rects, setRects] = useState<Record<string, Rect>>({});

  useLayoutEffect(() => {
    const container = containerRef.current;
    if (!container) return undefined;

    const measure = () => {
      const containerBox = container.getBoundingClientRect();
      const next: Record<string, Rect> = {};
      for (const [id, el] of Object.entries(frameRefs.current)) {
        if (!el) continue;
        const box = el.getBoundingClientRect();
        next[id] = { left: box.left - containerBox.left, top: box.top - containerBox.top, width: box.width, height: box.height };
      }
      setRects(next);
    };

    measure();
    if (typeof ResizeObserver === 'undefined') return undefined;
    const observer = new ResizeObserver(measure);
    observer.observe(container);
    return () => observer.disconnect();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);

  return rects;
}

function boxFor(id: string, rects: Record<string, Rect>, estimates: Record<string, Rect>): Rect | undefined {
  const measured = rects[id];
  if (measured && measured.width > 0 && measured.height > 0) return measured;
  return estimates[id];
}

/** Anchor a connector at the nearest facing edges of its two boxes: side-to-
 * side when the boxes are more horizontally than vertically separated,
 * top-to-bottom otherwise — a lightweight version of canvas.md's "connect
 * only neighboring steps" rule without full path-routing/collision-avoidance. */
function connectorPoints(from: Rect, to: Rect) {
  const fromCenter = { x: from.left + from.width / 2, y: from.top + from.height / 2 };
  const toCenter = { x: to.left + to.width / 2, y: to.top + to.height / 2 };
  const dx = toCenter.x - fromCenter.x;
  const dy = toCenter.y - fromCenter.y;
  let x1: number, y1: number, x2: number, y2: number;
  if (Math.abs(dx) >= Math.abs(dy)) {
    if (dx >= 0) {
      x1 = from.left + from.width;
      x2 = to.left;
    } else {
      x1 = from.left;
      x2 = to.left + to.width;
    }
    y1 = fromCenter.y;
    y2 = toCenter.y;
  } else {
    if (dy >= 0) {
      y1 = from.top + from.height;
      y2 = to.top;
    } else {
      y1 = from.top;
      y2 = to.top + to.height;
    }
    x1 = fromCenter.x;
    x2 = toCenter.x;
  }
  return { x1, y1, x2, y2, midX: (x1 + x2) / 2, midY: (y1 + y2) / 2 };
}

/** Position a plain-text annotation note beside its target box per
 * canvas.md — no border/shadow, parked in a gutter on the named side. */
function annotationStyle(rect: Rect, placement: Placement): CSSProperties {
  const gap = 14;
  const width = 176;
  switch (placement) {
    case 'top':
      return { left: rect.left, top: rect.top - gap, width, transform: 'translateY(-100%)' };
    case 'bottom':
      return { left: rect.left, top: rect.top + rect.height + gap, width };
    case 'left':
      return { left: rect.left - gap, top: rect.top, width, transform: 'translateX(-100%)' };
    case 'right':
    default:
      return { left: rect.left + rect.width + gap, top: rect.top, width };
  }
}

function ScreenArtboard({ screen, className }: { screen: ScreenSpec; className: string }) {
  if (!isWireframeSurface(screen.surface)) {
    return <UnrenderableBlock name="Screen" message={`unknown surface "${screen.surface}"`} />;
  }
  return (
    <WireframeFrame surface={screen.surface} html={screen.html} skeleton={screen.skeleton} label={screen.label} className={className} />
  );
}

/** The spatial x/y board: screens placed at their authored coordinates
 * (auto-laned when omitted), connectors drawn between named screens, and
 * annotations parked beside the screen they explain. */
function CanvasBoard({
  screens,
  connectors,
  annotations,
}: {
  screens: ScreenSpec[];
  connectors: ConnectorSpec[];
  annotations: AnnotationSpec[];
}) {
  const markerId = useId().replace(/:/g, '');
  const containerRef = useRef<HTMLDivElement>(null);
  const frameRefs = useRef<Record<string, HTMLDivElement | null>>({});

  const minX = Math.min(...screens.map((s) => s.x));
  const minY = Math.min(...screens.map((s) => s.y));

  const estimates: Record<string, Rect> = {};
  let boardWidth = 0;
  let boardHeight = 0;
  for (const screen of screens) {
    const size = SURFACE_BOARD_SIZE[isWireframeSurface(screen.surface) ? screen.surface : 'browser'];
    const left = (screen.x - minX) * BOARD_SCALE;
    const top = (screen.y - minY) * BOARD_SCALE;
    estimates[screen.id] = { left, top, width: size.width, height: size.height };
    boardWidth = Math.max(boardWidth, left + size.width);
    boardHeight = Math.max(boardHeight, top + size.height);
  }
  boardWidth += BOARD_PADDING * 2 + (annotations.length > 0 ? 200 : 0);
  boardHeight += BOARD_PADDING * 2;

  const rects = useBoardRects(containerRef, frameRefs, [screens, connectors, annotations]);

  return (
    <div ref={containerRef} className="relative overflow-auto rounded-lg border bg-muted/5 p-6">
      <div className="relative" style={{ width: boardWidth, height: boardHeight }}>
        <svg
          className="pointer-events-none absolute inset-0"
          width={boardWidth}
          height={boardHeight}
          style={{ zIndex: 0 }}
          aria-hidden="true"
        >
          <defs>
            <marker id={`canvas-arrow-${markerId}`} markerWidth="8" markerHeight="8" refX="6" refY="4" orient="auto">
              <path d="M0,0 L8,4 L0,8 Z" fill="var(--muted-foreground)" />
            </marker>
          </defs>
          {connectors.map((connector, index) => {
            const from = boxFor(connector.from, rects, estimates);
            const to = boxFor(connector.to, rects, estimates);
            if (!from || !to) return null;
            const { x1, y1, x2, y2, midX, midY } = connectorPoints(from, to);
            return (
              <g key={index} data-connector-from={connector.from} data-connector-to={connector.to}>
                <line
                  x1={x1}
                  y1={y1}
                  x2={x2}
                  y2={y2}
                  stroke="var(--muted-foreground)"
                  strokeWidth={1.5}
                  markerEnd={`url(#canvas-arrow-${markerId})`}
                />
                {connector.label ? (
                  <text x={midX} y={midY - 6} textAnchor="middle" fontSize="11" fill="var(--muted-foreground)">
                    {connector.label}
                  </text>
                ) : null}
              </g>
            );
          })}
        </svg>
        {screens.map((screen) => {
          const box = estimates[screen.id];
          return (
            <div
              key={screen.id}
              ref={(el) => {
                frameRefs.current[screen.id] = el;
              }}
              className="absolute"
              style={{ left: box.left, top: box.top, width: box.width, zIndex: 1 }}
            >
              <ScreenArtboard screen={screen} className="w-full" />
            </div>
          );
        })}
        {annotations.map((annotation, index) => {
          const rect = boxFor(annotation.targetId, rects, estimates);
          if (!rect) return null;
          return (
            <div
              key={index}
              data-annotation-target={annotation.targetId}
              data-annotation-placement={annotation.placement}
              className="absolute text-xs leading-snug text-muted-foreground"
              style={{ ...annotationStyle(rect, annotation.placement), zIndex: 2 }}
            >
              {annotation.label ? <strong className="block text-foreground">{annotation.label}</strong> : null}
              {annotation.note}
            </div>
          );
        })}
      </div>
    </div>
  );
}

/** Multiple wireframe artboards laid out together (~canvas.md). Two modes:
 * a flex-wrap gallery (no screen carries `x`/`y`) for the common case a
 * generated artifact actually produces today, and — the moment any `<Screen
 * x= y=>` supplies board coordinates — the full spatial board: screens
 * placed at their coordinates, `<Connector from= to= label?>` drawn between
 * named screens, and `<Annotation targetId= placement?>` parked beside the
 * screen it explains. Connector/Annotation are recognized only here,
 * mirroring how Column/Tab are scoped to their own parent block. */
export function Canvas({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  // Memoized on `node.children` (a stable reference for a given parsed
  // artifact — see ArtifactRenderer's content-keyed useMemo): readScreens/
  // readConnectors/readAnnotations each build a fresh array on every call,
  // so calling them unmemoized here would hand CanvasBoard's effect a new
  // array identity on every render and loop forever re-measuring.
  const screens = useMemo(() => readScreens(node.children), [node.children]);
  const connectors = useMemo(() => readConnectors(node.children), [node.children]);
  const annotations = useMemo(() => readAnnotations(node.children), [node.children]);

  if (screens.length === 0) {
    return <UnrenderableBlock name="Canvas" message="no <Screen> artboards found" />;
  }

  const boardMode = screens.some((screen) => screen.hasCoord);
  if (boardMode) {
    return <CanvasBoard screens={screens} connectors={connectors} annotations={annotations} />;
  }

  return (
    <div className="flex flex-wrap gap-4">
      {screens.map((screen) => (
        <ScreenArtboard key={screen.id} screen={screen} className="flex-1" />
      ))}
    </div>
  );
}
