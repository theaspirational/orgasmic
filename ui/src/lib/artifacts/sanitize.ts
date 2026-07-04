// Sanitization for raw-HTML block bodies (Wireframe/Screen, Diagram). These
// render inline (via dangerouslySetInnerHTML) so they can inherit the app's
// CSS custom properties (--wf-*, semantic tokens) across the light/dark
// theme boundary — that inheritance is why they are NOT put in an iframe.
// Prototype is the one block that renders in a sandboxed iframe instead (see
// blocks/Prototype.tsx) precisely because it needs neither app-theme
// inheritance nor DOM-level trust: content, subject nodes, and prior
// artifact text are all untrusted (dec_GPV4G / this task's Security note).
import DOMPurify from 'dompurify';

const ALLOWED_TAGS = [
  'div', 'span', 'p', 'a', 'strong', 'em', 'b', 'i', 'u', 's', 'small', 'br', 'hr', 'code', 'pre', 'blockquote',
  'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'ul', 'ol', 'li',
  'table', 'thead', 'tbody', 'tfoot', 'tr', 'td', 'th',
  'button', 'input', 'label', 'select', 'option', 'textarea', 'fieldset', 'legend',
  'img',
  'svg', 'path', 'circle', 'rect', 'line', 'g', 'polyline', 'polygon', 'defs', 'marker', 'text', 'tspan', 'ellipse',
];

const ALLOWED_ATTR = [
  'class', 'style', 'id', 'href', 'src', 'alt', 'title', 'placeholder', 'value', 'type', 'name',
  'checked', 'disabled', 'readonly', 'selected', 'for', 'colspan', 'rowspan', 'target', 'rel',
  'data-icon', 'data-rough', 'data-primary', 'data-skeleton',
  'aria-label', 'aria-hidden', 'aria-checked', 'role',
  'width', 'height', 'viewBox', 'fill', 'stroke', 'stroke-width', 'stroke-linecap', 'stroke-linejoin',
  'd', 'x', 'y', 'x1', 'y1', 'x2', 'y2', 'cx', 'cy', 'r', 'rx', 'ry', 'points', 'transform', 'marker-end',
];

const FORBID_TAGS = ['script', 'style', 'iframe', 'link', 'object', 'embed', 'base', 'meta', 'head', 'html', 'body', 'form'];

/** Sanitize an inline HTML fragment (Wireframe/Screen html, Diagram html).
 * Strips `<script>`, event-handler attributes, `javascript:`/`data:text/html`
 * URLs, document-wrapper tags, and any `<iframe>`/`<link>` the author tried
 * to smuggle in — the renderer creates its own iframe only for Prototype. */
export function sanitizeHtmlFragment(html: string | null | undefined): string {
  if (!html) return '';
  return DOMPurify.sanitize(html, {
    ALLOWED_TAGS,
    ALLOWED_ATTR,
    FORBID_TAGS,
    FORBID_ATTR: ['srcdoc'],
    ALLOW_DATA_ATTR: false,
    WHOLE_DOCUMENT: false,
    RETURN_TRUSTED_TYPE: false,
  }) as unknown as string;
}

const ICON_PATHS: Record<string, string> = {
  mail: '<rect x="3" y="5" width="18" height="14" rx="2"/><path d="m3 7 9 6 9-6"/>',
  email: '<rect x="3" y="5" width="18" height="14" rx="2"/><path d="m3 7 9 6 9-6"/>',
  lock: '<rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/>',
  password: '<rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/>',
  search: '<circle cx="11" cy="11" r="7"/><path d="m21 21-4.3-4.3"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  add: '<path d="M12 5v14M5 12h14"/>',
  x: '<path d="M18 6 6 18M6 6l12 12"/>',
  close: '<path d="M18 6 6 18M6 6l12 12"/>',
  check: '<path d="m5 13 4 4 10-10"/>',
  chevrondown: '<path d="m6 9 6 6 6-6"/>',
  chevronup: '<path d="m18 15-6-6-6 6"/>',
  chevronleft: '<path d="m15 18-6-6 6-6"/>',
  chevronright: '<path d="m9 18 6-6-6-6"/>',
  dots: '<circle cx="5" cy="12" r="1.4"/><circle cx="12" cy="12" r="1.4"/><circle cx="19" cy="12" r="1.4"/>',
  more: '<circle cx="5" cy="12" r="1.4"/><circle cx="12" cy="12" r="1.4"/><circle cx="19" cy="12" r="1.4"/>',
  chevron: '<path d="m6 9 6 6 6-6"/>',
  caret: '<path d="m6 9 6 6 6-6"/>',
  dropdown: '<path d="m6 9 6 6 6-6"/>',
  user: '<circle cx="12" cy="8" r="4"/><path d="M4 21c1.5-4.5 5-6 8-6s6.5 1.5 8 6"/>',
  settings:
    '<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .34 1.87l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.7 1.7 0 0 0-1.87-.34 1.7 1.7 0 0 0-1.03 1.56V21a2 2 0 1 1-4 0v-.09A1.7 1.7 0 0 0 8.98 19.4a1.7 1.7 0 0 0-1.87.34l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.7 1.7 0 0 0 4.6 15a1.7 1.7 0 0 0-1.56-1.03H3a2 2 0 1 1 0-4h.09A1.7 1.7 0 0 0 4.6 8.98a1.7 1.7 0 0 0-.34-1.87l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.7 1.7 0 0 0 9 4.6a1.7 1.7 0 0 0 1.03-1.56V3a2 2 0 1 1 4 0v.09A1.7 1.7 0 0 0 15.02 4.6a1.7 1.7 0 0 0 1.87-.34l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.7 1.7 0 0 0 19.4 9c.1.36.5 1.03 1.56 1.03H21a2 2 0 1 1 0 4h-.09A1.7 1.7 0 0 0 19.4 15Z"/>',
  calendar: '<rect x="3" y="5" width="18" height="16" rx="2"/><path d="M8 3v4M16 3v4M3 10h18"/>',
  bell: '<path d="M6 9a6 6 0 0 1 12 0c0 5 2 6 2 6H4s2-1 2-6"/><path d="M10 20a2 2 0 0 0 4 0"/>',
  send: '<path d="M4 12 20 4l-6.5 16-3-6.5L4 12Z"/>',
  edit: '<path d="M4 20h4l10.5-10.5a2.1 2.1 0 0 0-3-3L5 17v3Z"/>',
  arrowleft: '<path d="M19 12H5M11 6l-6 6 6 6"/>',
  arrowright: '<path d="M5 12h14M13 6l6 6-6 6"/>',
};

function iconSvg(name: string): string {
  const path = ICON_PATHS[name.trim().toLowerCase()];
  if (!path) return '';
  return `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${path}</svg>`;
}

/** Replace `data-icon="name"` marker elements with an inline SVG (Tabler/
 * Feather-style outline set), per the wireframe.md icon-substitution
 * contract. Runs on already-sanitized markup; the icon set is a static,
 * hand-authored allowlist (no user-controlled markup is introduced). */
export function substituteWireframeIcons(html: string): string {
  if (!html || typeof document === 'undefined') return html;
  const container = document.createElement('div');
  container.innerHTML = html;
  const markers = container.querySelectorAll('[data-icon]');
  markers.forEach((el) => {
    const name = el.getAttribute('data-icon') ?? '';
    const svg = iconSvg(name);
    if (svg) {
      el.innerHTML = svg;
      el.classList.add('wf-icon');
    }
  });
  return container.innerHTML;
}

/** Sanitize then substitute icon markers — the standard pipeline for
 * Wireframe/Screen and Diagram bodies. */
export function renderableFragment(html: string | null | undefined): string {
  return substituteWireframeIcons(sanitizeHtmlFragment(html));
}
