// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { createElement } from 'react';
import { afterEach, describe, expect, it } from 'vitest';

import { RunDockProvider, useRunDock } from '../runDock';

const KEY = 'orgasmic.rundock.conversation.v1';

function ChatProbe() {
  const { activeTabId, chatTarget, openChat, setChatTarget } = useRunDock();
  return createElement(
    'div',
    null,
    createElement('button', { onClick: () => openChat({ conversationId: 'CONV-1' }) }, 'Open CONV-1'),
    createElement('button', { onClick: () => openChat({ node: 'dec_1' }) }, 'Chat about node'),
    createElement('button', { onClick: () => openChat() }, 'Open chat'),
    createElement('button', { onClick: () => setChatTarget({ kind: 'setup' }) }, 'New chat'),
    createElement('output', { 'aria-label': 'active dock surface' }, activeTabId ?? 'none'),
    createElement('output', { 'aria-label': 'chat target' }, JSON.stringify(chatTarget)),
  );
}

afterEach(() => {
  cleanup();
  window.localStorage.clear();
});

describe('Run Dock chat target', () => {
  it('opens a conversation, persists it, and restores it on the next mount', () => {
    const first = render(createElement(RunDockProvider, null, createElement(ChatProbe)));
    expect(screen.getByLabelText('chat target').textContent).toBe('{"kind":"setup"}');

    fireEvent.click(screen.getByRole('button', { name: 'Open CONV-1' }));
    expect(screen.getByLabelText('active dock surface').textContent).toBe('chat');
    expect(screen.getByLabelText('chat target').textContent).toBe('{"kind":"conversation","conversationId":"CONV-1"}');
    expect(window.localStorage.getItem(KEY)).toBe('CONV-1');
    first.unmount();

    render(createElement(RunDockProvider, null, createElement(ChatProbe)));
    expect(screen.getByLabelText('chat target').textContent).toBe('{"kind":"conversation","conversationId":"CONV-1"}');
  });

  it('turns a node into a lookup the dock resolves, and keeps the last conversation across a bare open', () => {
    render(createElement(RunDockProvider, null, createElement(ChatProbe)));
    fireEvent.click(screen.getByRole('button', { name: 'Open CONV-1' }));
    fireEvent.click(screen.getByRole('button', { name: 'Chat about node' }));
    expect(screen.getByLabelText('chat target').textContent).toBe('{"kind":"lookup","node":"dec_1","purpose":"discuss"}');
    // A transient lookup never replaces the remembered conversation.
    expect(window.localStorage.getItem(KEY)).toBe('CONV-1');

    fireEvent.click(screen.getByRole('button', { name: 'Open chat' }));
    expect(screen.getByLabelText('chat target').textContent).toContain('lookup');

    fireEvent.click(screen.getByRole('button', { name: 'New chat' }));
    expect(screen.getByLabelText('chat target').textContent).toBe('{"kind":"setup"}');
    expect(window.localStorage.getItem(KEY)).toBeNull();
  });
});
