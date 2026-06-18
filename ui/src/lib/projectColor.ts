/**
 * Deterministic, browser-favicon-style identity for a project: a colored chip
 * (hue derived from the project id) with its initial. The chip is a solid color
 * independent of theme, so white text on it stays legible in both light and dark.
 */

import type { CSSProperties } from 'react';

function hashString(value: string): number {
  let hash = 2166136261;
  for (let i = 0; i < value.length; i += 1) {
    hash ^= value.charCodeAt(i);
    hash = Math.imul(hash, 16777619);
  }
  return hash >>> 0;
}

export function projectInitial(projectId: string | null | undefined): string {
  return projectId?.trim().slice(0, 1).toUpperCase() || '?';
}

export function projectHue(projectId: string): number {
  return hashString(projectId) % 360;
}

/**
 * Solid chip color + white foreground. Yellow/green hues read lighter at equal
 * lightness, so the chip is darkened there to keep white text at >=4.5:1.
 */
export function projectChipStyle(projectId: string): CSSProperties {
  const hue = projectHue(projectId);
  const lightness = hue >= 60 && hue <= 170 ? 0.48 : 0.56;
  return {
    backgroundColor: `oklch(${lightness} 0.15 ${hue})`,
    color: 'oklch(0.99 0 0)',
  };
}
