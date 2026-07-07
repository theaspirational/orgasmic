// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';

import { buildMermaidThemeVariables } from '../blocks/useMermaid';

// jsdom does not evaluate `var()` in getComputedStyle (verified empirically:
// it returns the literal unresolved string), so buildMermaidThemeVariables's
// live-CSS resolution path is unreachable here and every role falls back to
// its literal hex constant. That fallback path is exactly what these tests
// exercise: real browsers additionally resolve the live `--*` token via the
// same function (see the docstring on resolveRoleColor in useMermaid.ts).
const COLOR = /^(#[0-9a-f]{3,8}|rgba?\(|hsla?\()/i;

describe('buildMermaidThemeVariables', () => {
  it('covers the mermaid variables flowchart/sequence/error diagrams read', () => {
    const vars = buildMermaidThemeVariables(false);
    for (const key of [
      'background', 'primaryColor', 'primaryTextColor', 'primaryBorderColor', 'lineColor',
      'mainBkg', 'nodeBorder', 'actorBkg', 'actorBorder', 'noteBkgColor', 'noteTextColor',
      'errorBkgColor', 'errorTextColor', 'sequenceNumberColor',
    ]) {
      expect(vars, `missing mermaid var "${key}"`).toHaveProperty(key);
    }
  });

  it('every produced value is a color khroma can parse (never an unresolved var()/oklch() string)', () => {
    const vars = buildMermaidThemeVariables(false);
    for (const [key, value] of Object.entries(vars)) {
      expect(value, `"${key}" = "${value}"`).toMatch(COLOR);
    }
  });

  it('light and dark produce different palettes for every role', () => {
    const light = buildMermaidThemeVariables(false);
    const dark = buildMermaidThemeVariables(true);
    for (const key of Object.keys(light)) {
      expect(dark[key], `expected "${key}" to differ between themes`).not.toBe(light[key]);
    }
  });

  it('dark background is a dark color and light background is a light color (sanity-check the role mapping direction)', () => {
    const light = buildMermaidThemeVariables(false);
    const dark = buildMermaidThemeVariables(true);
    // Both fall back to their literal hex constants in jsdom; assert those
    // constants are actually oriented light-vs-dark, not just "different".
    const luminance = (hex: string) => {
      const m = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})/i.exec(hex);
      if (!m) return null;
      const [r, g, b] = m.slice(1).map((h) => Number.parseInt(h, 16));
      return 0.2126 * r + 0.7152 * g + 0.0722 * b;
    };
    const lightLuma = luminance(light.background);
    const darkLuma = luminance(dark.background);
    expect(lightLuma).not.toBeNull();
    expect(darkLuma).not.toBeNull();
    expect(lightLuma as number).toBeGreaterThan(darkLuma as number);
  });
});
