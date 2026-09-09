// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { createElement } from 'react';
import { afterEach, describe, expect, it } from 'vitest';

import { RunDockProvider, useRunDock } from '../runDock';

const KEY = 'orgasmic.rundock.conversation.v1';

const CHIP = { kind: 'node', id: 'dec_2' } as const;

function ChatProbe() {
  const { activeTabId, chatTarget, openChat, setChatTarget, chatContext } = useRunDock();
  return createElement(
    'div',
    null,
    createElement('button', { onClick: () => openChat({ conversationId: 'CONV-1' }) }, 'Open CONV-1'),
    createElement('button', { onClick: () => openChat({ node: 'dec_1' }) }, 'Chat about node'),
    createElement('button', { onClick: () => openChat({ node: 'dec_1', context: [CHIP] }) }, 'Chat about node with chip'),
    createElement('button', { onClick: () => openChat() }, 'Open chat'),
    createElement('button', { onClick: () => chatContext([CHIP]) }, 'Set chips'),
    createElement('button', { onClick: () => chatContext(null) }, 'Clear chips'),
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

  it('carries optional chips on any current target and lets chatContext set or clear them', () => {
    const target = () => JSON.parse(screen.getByLabelText('chat target').textContent!);
    render(createElement(RunDockProvider, null, createElement(ChatProbe)));
    // A setup takes chips for its first message.
    fireEvent.click(screen.getByRole('button', { name: 'Set chips' }));
    expect(target()).toEqual({ kind: 'setup', context: [CHIP] });

    fireEvent.click(screen.getByRole('button', { name: 'Chat about node with chip' }));
    expect(target()).toEqual({ kind: 'lookup', node: 'dec_1', purpose: 'discuss', context: [CHIP] });
    // Right after openChat({node}), before the lookup resolves, chatContext lands on the lookup.
    fireEvent.click(screen.getByRole('button', { name: 'Chat about node' }));
    expect(target()).toEqual({ kind: 'lookup', node: 'dec_1', purpose: 'discuss' });
    fireEvent.click(screen.getByRole('button', { name: 'Set chips' }));
    expect(target()).toEqual({ kind: 'lookup', node: 'dec_1', purpose: 'discuss', context: [CHIP] });

    // A target change drops them; setting, a bare re-open, and clearing behave on a conversation.
    fireEvent.click(screen.getByRole('button', { name: 'Open CONV-1' }));
    expect(target()).toEqual({ kind: 'conversation', conversationId: 'CONV-1' });
    fireEvent.click(screen.getByRole('button', { name: 'Set chips' }));
    expect(target()).toEqual({ kind: 'conversation', conversationId: 'CONV-1', context: [CHIP] });
    fireEvent.click(screen.getByRole('button', { name: 'Open chat' }));
    expect(target().context).toEqual([CHIP]);
    fireEvent.click(screen.getByRole('button', { name: 'Clear chips' }));
    expect(target()).toEqual({ kind: 'conversation', conversationId: 'CONV-1' });
  });
});
