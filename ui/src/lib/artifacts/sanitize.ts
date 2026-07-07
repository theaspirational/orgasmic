// Sanitization for raw-HTML block bodies (Wireframe/Screen, Diagram). These
// render inline (via dangerouslySetInnerHTML) so they can inherit the app's
// CSS custom properties (--wf-*, semantic tokens) across the light/dark
// theme boundary — that inheritance is why they are NOT put in an iframe.
// Prototype is the one block that renders in a sandboxed iframe instead (see
// blocks/Prototype.tsx) precisely because it needs neither app-theme
// inheritance nor DOM-level trust: content, subject nodes, and prior
// artifact text are all untrusted (dec_GPV4G / this task's Security note).
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import {
  ArrowLeft,
  ArrowRight,
  Bell,
  Calendar,
  Check,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  ChevronUp,
  Lock,
  Mail,
  MoreHorizontal,
  Plus,
  Search,
  Send,
  Settings,
  SquarePen,
  User,
  X,
  type LucideIcon,
} from 'lucide-react';
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

/** `data-icon="name"` → lucide-react component, one entry per name/alias the
 * wireframe.md quality bar documents. lucide-react ships the full Feather-
 * descended icon set (the brief's "Tabler/Feather set") as real React
 * components rather than the small hand-drawn stand-in paths this used to
 * inline directly. */
const ICON_COMPONENTS: Record<string, LucideIcon> = {
  mail: Mail,
  email: Mail,
  lock: Lock,
  password: Lock,
  search: Search,
  plus: Plus,
  add: Plus,
  x: X,
  close: X,
  check: Check,
  chevrondown: ChevronDown,
  chevronup: ChevronUp,
  chevronleft: ChevronLeft,
  chevronright: ChevronRight,
  dots: MoreHorizontal,
  more: MoreHorizontal,
  chevron: ChevronDown,
  caret: ChevronDown,
  dropdown: ChevronDown,
  user: User,
  settings: Settings,
  calendar: Calendar,
  bell: Bell,
  send: Send,
  edit: SquarePen,
  arrowleft: ArrowLeft,
  arrowright: ArrowRight,
};

const iconSvgCache = new Map<string, string>();

/** Render a lucide icon component to its inner SVG markup (server-side
 * string render, no DOM needed) and re-wrap it in the same minimal
 * `<svg>` shell the previous hand-authored set used, so `.wf-icon`'s
 * `width/height: 1em` sizing keeps working unchanged. The wrapper is
 * rebuilt rather than reused verbatim so lucide's own `class`/`width`/
 * `height`/`xmlns` attributes never leak into the sanitized fragment. */
function iconSvg(name: string): string {
  const key = name.trim().toLowerCase();
  const cached = iconSvgCache.get(key);
  if (cached !== undefined) return cached;
  const Icon = ICON_COMPONENTS[key];
  if (!Icon) {
    iconSvgCache.set(key, '');
    return '';
  }
  const markup = renderToStaticMarkup(
    createElement(Icon, { strokeWidth: 1.75, 'aria-hidden': true, focusable: false }),
  );
  const inner = /<svg[^>]*>([\s\S]*)<\/svg>/.exec(markup)?.[1] ?? '';
  const svg = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${inner}</svg>`;
  iconSvgCache.set(key, svg);
  return svg;
}

/** Replace `data-icon="name"` marker elements with an inline SVG sourced
 * from lucide-react (a Feather-descended outline set), per the wireframe.md
 * icon-substitution contract. Runs on already-sanitized markup; `name` only
 * ever selects from the fixed `ICON_COMPONENTS` map above, so no
 * user-controlled markup is introduced even though this bypasses DOMPurify
 * for the substituted content itself. */
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
