// Render oracle + primary render-test input (TASK-T25XQ). Exercises all 19
// registered block types, including the structural edge cases the daemon's
// own validator cannot handle: a nested Columns/Tabs, and a Code body
// carrying a literal `</Code>`-shaped substring (safe here because it is
// authored as a backtick template-literal attribute, never as JSX children —
// see the "Raw-text convention" note in artifact-generator.org).
//
// Wireframe/Screen/Diagram html is authored as CHILDREN, not attributes —
// verified live against the real daemon submit gate
// (crates/orgasmic-daemon/src/artifacts.rs::scan_tag_header):
// it tracks `"`/`'` quote state byte-by-byte while scanning an opening tag's
// attribute list but has no concept of backticks, so a self-closing tag whose
// attribute value is a multi-line HTML blob (many embedded `>`/quote
// characters) can desync that tracker and misdetect the tag's own end. A short
// plain-attribute header (`surface="panel"`) plus body content found via a
// simple `</Name>` substring search — which children get — sidesteps that
// entirely. Code/AnnotatedCode keep the attribute form because their content
// legitimately needs the opposite property (immunity to looking like a
// same-name close tag), and code samples are short enough in practice that
// the daemon's header scan reliably lands on the right `>`.
export const ALL_BLOCKS_MDX = `
<RichText>
## Widget Settings — dark mode toggle

This artifact walks through adding a dark mode toggle to the Widget Settings
panel: the UI change, the persisted preference, and the rollout plan.
</RichText>

<Callout tone="decision">
Store the preference as a per-widget override rather than a global setting —
most operators run several widgets with different embed contexts, and a
single global toggle would fight per-page dark/light detection.
</Callout>

<Section title="Before / after">
<Columns>
<Column label="Before">
<RichText>
No dark mode option exists; the widget always renders with the light
palette regardless of the embedding page's theme.
</RichText>
</Column>
<Column label="After">
<Checklist items={[
  { label: "Settings panel exposes a theme selector", done: true },
  { label: "Widget iframe reads the stored preference on boot", done: true },
  { label: "Falls back to system preference when unset", done: false, note: "TASK-follow-up" }
]} />
</Column>
</Columns>
</Section>

<Tabs>
<Tab label="widget-settings.tsx">
<Code language="tsx" filename="widget-settings.tsx" code={\`export function ThemeField({ value, onChange }: ThemeFieldProps) {
  return (
    <Select value={value} onValueChange={onChange}>
      <SelectItem value="system">System</SelectItem>
      <SelectItem value="light">Light</SelectItem>
      <SelectItem value="dark">Dark</SelectItem>
    </Select>
  );
}\`} />
</Tab>
<Tab label="theme-store.ts">
<Code language="ts" filename="theme-store.ts" code={\`export const THEME_KEY_PREFIX = 'widget-theme:';\`} />
</Tab>
</Tabs>

<Callout tone="info">
A worked example of why code bodies use a template-literal attribute, not
children: this block's own source text contains a literal closing tag.
</Callout>

<Code language="mdx" filename="example.mdx" caption="A literal closing-tag substring inside the code attribute, unambiguous because it lives inside a backtick string rather than JSX children." code={\`<!-- This line intentionally contains a literal closing tag: -->
<Code>const done = true;</Code>
\`} />

<AnnotatedCode language="ts" filename="theme-store.ts" annotations={[
  { lines: "1-2", label: "Persistence", note: "Reads/writes the per-widget override, never a global key." },
  { lines: "4-6", label: "Fallback", note: "Falls back to prefers-color-scheme when no override is stored." }
]} code={\`export function readWidgetTheme(widgetId: string): Theme {
  const stored = localStorage.getItem(themeKey(widgetId));
  if (stored === 'light' || stored === 'dark') return stored;
  return window.matchMedia('(prefers-color-scheme: dark)').matches
    ? 'dark'
    : 'light';
}\`} />

<Table
  headers={["Field", "Type", "Notes"]}
  rows={[
    ["widget_id", "string", "Primary key for the override row"],
    ["theme", "'system' | 'light' | 'dark'", "Defaults to 'system'"],
    ["updated_at", "timestamp", "Set on every PATCH"]
  ]}
  caption="widget_theme_overrides table"
/>

<DataModel
  entities={[
    { name: "Widget", fields: [
      { name: "id", type: "uuid", pk: true },
      { name: "name", type: "text" }
    ] },
    { name: "WidgetThemeOverride", fields: [
      { name: "widget_id", type: "uuid", pk: true, fk: true },
      { name: "theme", type: "text" },
      { name: "updated_at", type: "timestamptz", nullable: true }
    ] }
  ]}
  relations={[
    { from: "WidgetThemeOverride.widget_id", to: "Widget.id", label: "overrides" }
  ]}
/>

<EntityRelationship
  entities={[
    { name: "Widget", fields: [ { name: "id", type: "uuid", pk: true } ] },
    { name: "Embed", fields: [ { name: "widget_id", type: "uuid", fk: true } ] }
  ]}
  relations={[ { from: "Embed.widget_id", to: "Widget.id", label: "embeds" } ]}
/>

<FileTree nodes={[
  { name: "src", type: "dir", children: [
    { name: "widget-settings.tsx", type: "file", note: "theme selector UI" },
    { name: "theme-store.ts", type: "file", note: "read/write override" },
    { name: "__tests__", type: "dir", children: [
      { name: "theme-store.test.ts", type: "file" }
    ] }
  ] }
]} />

<Diagram caption="Theme resolution at widget boot">
<div class="diagram-panel" style="display:flex;flex-direction:column;gap:10px">
  <div style="display:flex;gap:10px;align-items:center">
    <span class="diagram-node">Boot</span>
    <span class="diagram-muted">&#8594;</span>
    <span class="diagram-node">Read override</span>
    <span class="diagram-muted">&#8594;</span>
    <span class="diagram-node">Apply theme</span>
  </div>
  <span class="diagram-pill">Falls back to system preference when unset</span>
</div>
</Diagram>

<Diagram caption="Override decision (diagram-design editorial SVG)" height={220}>
<style>
.dd-label { font-family: var(--sans); font-size: 13px; font-weight: 600; }
</style>
<svg viewBox="0 0 620 160" role="img" aria-label="Theme override decision flow">
  <defs>
    <marker id="dd-arrow" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto">
      <path d="M0,0 L8,4 L0,8 z" fill="currentColor" />
    </marker>
  </defs>
  <rect x="20" y="58" width="140" height="44" rx="8" fill="none" stroke="currentColor" stroke-width="1.5" />
  <text x="90" y="85" text-anchor="middle" class="dd-label" fill="currentColor">Boot</text>
  <line x1="160" y1="80" x2="230" y2="80" stroke="currentColor" stroke-width="1.5" marker-end="url(#dd-arrow)" />
  <rect x="234" y="58" width="160" height="44" rx="8" fill="none" stroke="currentColor" stroke-width="1.5" stroke-dasharray="4 3" />
  <text x="314" y="85" text-anchor="middle" class="dd-label" fill="currentColor">Override stored?</text>
  <line x1="394" y1="80" x2="464" y2="80" stroke="currentColor" stroke-width="1.5" marker-end="url(#dd-arrow)" />
  <rect x="468" y="58" width="132" height="44" rx="8" fill="none" stroke="currentColor" stroke-width="1.5" />
  <text x="534" y="85" text-anchor="middle" class="dd-label" fill="currentColor">Apply theme</text>
</svg>
</Diagram>

<Timeline items={[
  { date: "2026-06-30", label: "Design review", body: "Confirmed per-widget override over a global setting." },
  { date: "2026-07-02", label: "Backend ships", body: "widget_theme_overrides table + PATCH route." },
  { date: "2026-07-04", label: "UI ships", body: "Theme selector + read-on-boot." }
]} />

<Wireframe surface="panel">
<div style="display:flex;flex-direction:column;gap:12px;padding:16px;height:100%">
  <h3>Appearance</h3>
  <label>Theme
    <select>
      <option>System</option>
      <option>Light</option>
      <option selected>Dark</option>
    </select>
  </label>
  <div class="wf-card" data-rough style="display:flex;align-items:center;gap:8px">
    <span data-icon="check"></span>
    <small class="wf-muted">Saved a moment ago</small>
  </div>
  <button class="primary">Save changes</button>
</div>
</Wireframe>

<Canvas>
<Screen surface="mobile" label="Before">
<div style="display:flex;flex-direction:column;height:100%;padding:14px;gap:10px">
  <h3>Appearance</h3>
  <p class="wf-muted">No theme option available.</p>
</div>
</Screen>
<Screen surface="mobile" label="After">
<div style="display:flex;flex-direction:column;height:100%;padding:14px;gap:10px">
  <h3>Appearance</h3>
  <label>Theme<select><option>System</option><option selected>Dark</option></select></label>
</div>
</Screen>
</Canvas>

<Prototype start="settings">
<Screen id="settings" label="Settings">
<div style="font-family:sans-serif;padding:16px">
  <h3>Appearance</h3>
  <button onclick="document.getElementById('saved').style.display='block'">Save</button>
  <p id="saved" style="display:none;color:green">Saved!</p>
</div>
</Screen>
</Prototype>

<QuestionForm title="Open Questions" questions={[
  {
    type: "single",
    prompt: "Should the toggle live in the widget's own settings panel or the host page's embed config?",
    options: [
      { label: "Widget settings panel", detail: "Keeps the control next to the rest of the widget's appearance options.", recommended: true },
      { label: "Host embed config", detail: "Lets the host page set it once for every widget it embeds." }
    ],
    allowOther: true
  },
  {
    type: "freeform",
    prompt: "Any constraints on the fallback behavior when localStorage is unavailable (e.g. third-party cookie blocking)?"
  }
]} />

<Image
  src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
  alt="Theme selector control, dark option selected"
  caption="Final rendered control (placeholder pixel — replace with a real screenshot when available)."
/>
`;
