// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';

import { handleContainedWheelCapture } from '../containedWheel';

function dispatchWheel(target: HTMLElement, deltaY: number) {
  const event = new WheelEvent('wheel', { bubbles: true, cancelable: true, deltaY });
  target.dispatchEvent(event);
  return event;
}

function installContainedWheelHandler(boundary: HTMLElement) {
  boundary.addEventListener(
    'wheel',
    (event) => handleContainedWheelCapture(event, boundary),
    { capture: true, passive: false },
  );
}

afterEach(() => {
  document.body.replaceChildren();
});

describe('handleContainedWheelCapture', () => {
  it('blocks a wheel event when no descendant can consume it', () => {
    const boundary = document.createElement('div');
    const target = document.createElement('div');
    const targetHandler = vi.fn();
    boundary.append(target);
    document.body.append(boundary);
    installContainedWheelHandler(boundary);
    target.addEventListener('wheel', targetHandler);

    const event = dispatchWheel(target, 100);

    expect(event.defaultPrevented).toBe(true);
    expect(targetHandler).not.toHaveBeenCalled();
  });

  it('allows a scrollable descendant to consume the wheel event', () => {
    const boundary = document.createElement('div');
    const target = document.createElement('div');
    const targetHandler = vi.fn();
    Object.defineProperties(target, {
      clientHeight: { configurable: true, value: 100 },
      scrollHeight: { configurable: true, value: 300 },
      scrollTop: { configurable: true, value: 0 },
    });
    boundary.append(target);
    document.body.append(boundary);
    installContainedWheelHandler(boundary);
    target.addEventListener('wheel', targetHandler);

    const event = dispatchWheel(target, 100);

    expect(event.defaultPrevented).toBe(false);
    expect(targetHandler).toHaveBeenCalledOnce();
  });

  it('allows a marked component to handle wheel input without DOM scrollback', () => {
    const boundary = document.createElement('div');
    const terminalHost = document.createElement('div');
    const terminalScreen = document.createElement('div');
    const terminalHandler = vi.fn((event: WheelEvent) => event.preventDefault());
    terminalHost.setAttribute('data-contained-wheel-passthrough', '');
    terminalHost.append(terminalScreen);
    boundary.append(terminalHost);
    document.body.append(boundary);
    installContainedWheelHandler(boundary);
    terminalScreen.addEventListener('wheel', terminalHandler);

    const event = dispatchWheel(terminalScreen, -100);

    expect(event.defaultPrevented).toBe(true);
    expect(terminalHandler).toHaveBeenCalledOnce();
  });
});
