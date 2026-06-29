// Bridge the Android shell's window insets into CSS. The shell runs the WebView
// edge-to-edge (drawing under the status/navigation bars) to satisfy Android 15+
// enforcement, so the web layer is responsible for keeping its chrome clear of
// those bars. Android WebView does NOT reliably populate env(safe-area-inset-*)
// for the system bars (only display cutouts, and Chromium < 140 reports 0px), so
// the native side exposes the measured insets through a JS interface and we mirror
// them onto --sai-* custom properties. styles.css folds those into --safe-* with
// an env() fallback for every other platform.
//
// This is a *pull* model on purpose: navigating from the bootstrap origin to the
// daemon origin loads a fresh document, and the native inset listener does not
// re-fire on same-WebView navigation — so each document reads the insets itself
// on load (and again on resize/rotation, which Android fires when bars change).

type InsetBridge = { get: () => string };

declare global {
  interface Window {
    __orgasmicInsets?: InsetBridge;
  }
}

function applyInsets(): void {
  const bridge = window.__orgasmicInsets;
  if (!bridge) return; // not the Android shell — env() fallback in CSS handles it
  let insets: { top: number; right: number; bottom: number; left: number };
  try {
    insets = JSON.parse(bridge.get());
  } catch {
    return;
  }
  const root = document.documentElement.style;
  root.setProperty('--sai-top', `${insets.top}px`);
  root.setProperty('--sai-right', `${insets.right}px`);
  root.setProperty('--sai-bottom', `${insets.bottom}px`);
  root.setProperty('--sai-left', `${insets.left}px`);
}

/** Mirror native window insets onto --sai-* and keep them current on rotation.
 *  No-op (beyond a missing-bridge check) on every non-Android platform. */
export function initAndroidInsets(): void {
  if (!window.__orgasmicInsets) return;
  applyInsets();
  // Rotation / system-bar visibility changes resize the viewport; re-read then.
  window.addEventListener('resize', applyInsets);
  window.addEventListener('orientationchange', applyInsets);
}
