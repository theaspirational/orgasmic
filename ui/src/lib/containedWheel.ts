import { useEffect, useRef } from 'react';

function isVerticallyScrollable(element: HTMLElement): boolean {
  return element.scrollHeight > element.clientHeight + 1;
}

function canScrollVertically(element: HTMLElement, deltaY: number): boolean {
  if (deltaY === 0 || !isVerticallyScrollable(element)) return false;
  const atTop = element.scrollTop <= 0;
  const atBottom =
    Math.ceil(element.scrollTop + element.clientHeight) >= element.scrollHeight;
  if (deltaY < 0) return !atTop;
  return !atBottom;
}

function scrollableWheelTarget(event: WheelEvent, boundary: HTMLElement): HTMLElement | null {
  let node = event.target;
  while (node instanceof HTMLElement && node !== boundary) {
    if (canScrollVertically(node, event.deltaY)) return node;
    node = node.parentElement;
  }
  return null;
}

/** Block scroll chaining from `boundary` to ancestors (e.g. the page behind a dock). */
export function handleContainedWheelCapture(event: WheelEvent, boundary: HTMLElement): void {
  if (scrollableWheelTarget(event, boundary)) return;
  event.preventDefault();
  event.stopPropagation();
}

export function useContainedWheelRef<T extends HTMLElement>() {
  const ref = useRef<T | null>(null);
  useEffect(() => {
    const boundary = ref.current;
    if (!boundary) return undefined;
    const onWheel = (event: WheelEvent) => handleContainedWheelCapture(event, boundary);
    boundary.addEventListener('wheel', onWheel, { capture: true, passive: false });
    return () => boundary.removeEventListener('wheel', onWheel, { capture: true });
  }, []);
  return ref;
}
