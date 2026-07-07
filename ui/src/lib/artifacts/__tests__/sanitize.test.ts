// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';

import { renderableFragment, sanitizeHtmlFragment, substituteWireframeIcons } from '../sanitize';

describe('sanitizeHtmlFragment', () => {
  it('strips <script> tags', () => {
    const out = sanitizeHtmlFragment('<div>hello<script>alert(1)</script></div>');
    expect(out).not.toContain('<script');
    expect(out).not.toContain('alert(1)');
    expect(out).toContain('hello');
  });

  it('strips event-handler attributes', () => {
    const out = sanitizeHtmlFragment('<button onclick="alert(1)">Click</button>');
    expect(out).not.toContain('onclick');
    expect(out).not.toContain('alert(1)');
    expect(out).toContain('Click');
  });

  it('strips javascript: URLs', () => {
    const out = sanitizeHtmlFragment('<a href="javascript:alert(1)">link</a>');
    expect(out.toLowerCase()).not.toContain('javascript:');
  });

  it('strips <style>/<html>/<body> wrapper tags and inline <iframe>/<link>', () => {
    const out = sanitizeHtmlFragment(
      '<html><head><link rel="stylesheet" href="x.css" /></head><body><style>body{color:red}</style><iframe src="https://evil.example"></iframe><p>content</p></body></html>',
    );
    expect(out).not.toContain('<html');
    expect(out).not.toContain('<body');
    expect(out).not.toContain('<style');
    expect(out).not.toContain('<link');
    expect(out).not.toContain('<iframe');
    expect(out).toContain('content');
  });

  it('preserves the wireframe/diagram helper classes and data-icon markers', () => {
    const out = sanitizeHtmlFragment('<div class="wf-card"><span data-icon="mail"></span></div>');
    expect(out).toContain('wf-card');
    expect(out).toContain('data-icon="mail"');
  });
});

describe('substituteWireframeIcons', () => {
  it('replaces a data-icon marker with an inline svg and the wf-icon class', () => {
    const out = substituteWireframeIcons('<span data-icon="mail"></span>');
    expect(out).toContain('<svg');
    expect(out).toContain('wf-icon');
  });

  it('leaves unknown icon names untouched (no crash, no empty svg)', () => {
    const out = substituteWireframeIcons('<span data-icon="not-a-real-icon"></span>');
    expect(out).not.toContain('<svg');
  });

  it('sources the mail icon path data from lucide-react, not the old hand-authored stand-in', () => {
    const out = substituteWireframeIcons('<span data-icon="mail"></span>');
    // lucide's "mail" icon (dist/esm/icons/mail.mjs): an envelope flap path
    // starting "m22 7-8.991..." plus a 20x16 rx2 body rect — distinct from
    // the previous hand-drawn "m3 7 9 6 9-6" / 18x14 rect stand-in.
    expect(out).toContain('m22 7');
    expect(out).toContain('width="20" height="16"');
    expect(out).not.toContain('m3 7 9 6 9-6');
  });

  it('is case-insensitive and resolves every documented wireframe.md alias to a real icon', () => {
    const names = [
      'mail', 'email', 'lock', 'password', 'search', 'plus', 'add', 'x', 'close', 'check',
      'chevronDown', 'chevronUp', 'chevronLeft', 'chevronRight', 'dots', 'more',
      'chevron', 'caret', 'dropdown', 'user', 'settings', 'calendar', 'bell', 'send',
      'edit', 'arrowLeft', 'arrowRight',
    ];
    for (const name of names) {
      const out = substituteWireframeIcons(`<span data-icon="${name}"></span>`);
      expect(out, `expected an svg for data-icon="${name}"`).toContain('<svg');
    }
  });
});

describe('renderableFragment (sanitize + icon substitution pipeline)', () => {
  it('produces script-free, icon-substituted markup in one pass', () => {
    const out = renderableFragment('<div><script>alert(1)</script><span data-icon="check"></span></div>');
    expect(out).not.toContain('<script');
    expect(out).toContain('<svg');
  });
});
