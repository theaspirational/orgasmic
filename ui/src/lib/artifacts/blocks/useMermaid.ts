import { useEffect, useState } from 'react';

let renderCounter = 0;

/** A handful of semantic color roles, each backed by an app CSS custom
 * property plus a plain hex fallback per theme. Mermaid's `theme: 'base'`
 * runs every seed color through `khroma` (lighten/darken/adjust) to derive
 * the rest of its palette, and khroma only understands hex/rgb/hsl — not the
 * `oklch()` values several of the app's tokens use (verified empirically:
 * `mermaid.initialize` throws "Unsupported color format" on an oklch string
 * for `theme: 'base'`). So each role is read live via a resolver that asks
 * the browser to compute the custom property down to `rgb()` (which always
 * succeeds for a real stylesheet), falling back to the literal hex only when
 * that resolution doesn't come back as a color at all — e.g. jsdom, which
 * does not evaluate `var()` in `getComputedStyle` and would otherwise hand
 * khroma the literal unresolved string. */
type ColorRole = { cssVar: string; light: string; dark: string };

const ROLES = {
  background: { cssVar: '--background', light: '#f2ece1', dark: '#160d08' },
  foreground: { cssVar: '--foreground', light: '#3a332c', dark: '#e8dcd4' },
  card: { cssVar: '--card', light: '#faf7f1', dark: '#21140c' },
  border: { cssVar: '--border', light: '#cec5b4', dark: '#3a2a20' },
  muted: { cssVar: '--muted', light: '#e6e0d3', dark: '#2a1a10' },
  mutedForeground: { cssVar: '--muted-foreground', light: '#6f6659', dark: '#a89a8e' },
  primary: { cssVar: '--primary', light: '#2f6f74', dark: '#966d4f' },
  primaryForeground: { cssVar: '--primary-foreground', light: '#faf7f1', dark: '#160d08' },
  accent: { cssVar: '--accent', light: '#ddd0c0', dark: '#ba977d' },
  accentSoft: { cssVar: '--accent-soft', light: '#cfe0df', dark: '#3a2a20' },
  destructive: { cssVar: '--destructive', light: '#b5432a', dark: '#c9603f' },
} as const satisfies Record<string, ColorRole>;

type RoleName = keyof typeof ROLES;

/** Every mermaid diagram kind's theme variable, mapped to one of the roles
 * above (flowchart/state/class nodes + edges, sequence-diagram actors/notes,
 * error diagram). Unlisted derived variables (hover states, `background2`,
 * etc.) are computed by mermaid itself from these seeds. */
const MERMAID_VARIABLE_ROLES: Record<string, RoleName> = {
  background: 'background',
  primaryColor: 'card',
  primaryTextColor: 'foreground',
  primaryBorderColor: 'border',
  secondaryColor: 'muted',
  secondaryTextColor: 'foreground',
  secondaryBorderColor: 'border',
  tertiaryColor: 'accent',
  tertiaryTextColor: 'foreground',
  tertiaryBorderColor: 'border',
  lineColor: 'mutedForeground',
  textColor: 'foreground',
  mainBkg: 'card',
  secondBkg: 'muted',
  nodeBorder: 'border',
  clusterBkg: 'muted',
  clusterBorder: 'border',
  edgeLabelBackground: 'background',
  titleColor: 'foreground',
  actorBkg: 'card',
  actorBorder: 'border',
  actorTextColor: 'foreground',
  actorLineColor: 'border',
  signalColor: 'foreground',
  signalTextColor: 'foreground',
  labelBoxBkgColor: 'card',
  labelBoxBorderColor: 'border',
  labelTextColor: 'foreground',
  loopTextColor: 'foreground',
  noteBkgColor: 'accentSoft',
  noteBorderColor: 'border',
  noteTextColor: 'foreground',
  activationBorderColor: 'primary',
  activationBkgColor: 'accentSoft',
  sequenceNumberColor: 'primaryForeground',
  errorBkgColor: 'destructive',
  errorTextColor: 'background',
};

const RESOLVED_COLOR = /^(rgba?|hsla?)\(/i;

/** Resolve a `var(--token)` reference to a real `rgb()`/`rgba()` string via
 * an off-screen probe element, so khroma (mermaid's internal color math)
 * gets something it can parse regardless of which color syntax the token
 * itself uses (oklch, hex, color-mix, ...). Falls back to a plain hex
 * constant when that resolution doesn't produce a color — the case in any
 * environment that doesn't run the app's real stylesheet through a layout
 * engine (jsdom in tests; a detached render before styles.css loads). */
function resolveRoleColor(role: ColorRole, dark: boolean): string {
  const fallback = dark ? role.dark : role.light;
  if (typeof document === 'undefined') return fallback;
  try {
    const probe = document.createElement('span');
    probe.style.position = 'absolute';
    probe.style.visibility = 'hidden';
    probe.style.pointerEvents = 'none';
    probe.style.color = `var(${role.cssVar})`;
    document.body.appendChild(probe);
    const resolved = getComputedStyle(probe).color;
    document.body.removeChild(probe);
    return RESOLVED_COLOR.test(resolved) ? resolved : fallback;
  } catch {
    return fallback;
  }
}

/** Build mermaid's `themeVariables` for `theme: 'base'` from the app's own
 * `--*` design tokens (per-theme, live) instead of mermaid's built-in
 * default/dark palettes. Exported for unit testing the role→variable
 * mapping directly. */
export function buildMermaidThemeVariables(dark: boolean): Record<string, string> {
  const vars: Record<string, string> = {};
  for (const [mermaidKey, roleName] of Object.entries(MERMAID_VARIABLE_ROLES)) {
    vars[mermaidKey] = resolveRoleColor(ROLES[roleName], dark);
  }
  return vars;
}

/** Render Mermaid source to an SVG string. `securityLevel: 'strict'` disables
 * HTML labels/click bindings so untrusted diagram text (subject nodes, prior
 * artifact content) cannot inject markup through a node label. Re-renders
 * when `dark` flips so diagrams re-theme with the app instead of keeping
 * whichever palette was baked in at first render. */
export function useMermaidSvg(source: string, dark: boolean): { svg: string | null; error: string | null } {
  const [svg, setSvg] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setSvg(null);
    setError(null);
    const trimmed = source.trim();
    if (!trimmed) return undefined;

    void (async () => {
      try {
        const { default: mermaid } = await import('mermaid');
        mermaid.initialize({
          startOnLoad: false,
          securityLevel: 'strict',
          theme: 'base',
          themeVariables: buildMermaidThemeVariables(dark),
          fontFamily: 'var(--sans)',
        });
        renderCounter += 1;
        const id = `orgasmic-mermaid-${renderCounter}`;
        const { svg: rendered } = await mermaid.render(id, trimmed);
        if (!cancelled) setSvg(rendered);
      } catch (err) {
        if (!cancelled) setError(err instanceof Error ? err.message : String(err));
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [source, dark]);

  return { svg, error };
}
