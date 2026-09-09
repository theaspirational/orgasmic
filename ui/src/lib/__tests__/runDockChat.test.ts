// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { createElement } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';

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

  it('carries optional chips on the target and lets chatContext set or clear them on an open conversation', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    render(createElement(RunDockProvider, null, createElement(ChatProbe)));
    // Not on a conversation: a warning no-op.
    fireEvent.click(screen.getByRole('button', { name: 'Set chips' }));
    expect(warn).toHaveBeenCalledOnce();
    expect(screen.getByLabelText('chat target').textContent).toBe('{"kind":"setup"}');

    fireEvent.click(screen.getByRole('button', { name: 'Chat about node with chip' }));
    expect(JSON.parse(screen.getByLabelText('chat target').textContent!)).toEqual({ kind: 'lookup', node: 'dec_1', purpose: 'discuss', context: [CHIP] });

    fireEvent.click(screen.getByRole('button', { name: 'Open CONV-1' }));
    expect(JSON.parse(screen.getByLabelText('chat target').textContent!)).toEqual({ kind: 'conversation', conversationId: 'CONV-1' });
    fireEvent.click(screen.getByRole('button', { name: 'Set chips' }));
    expect(JSON.parse(screen.getByLabelText('chat target').textContent!)).toEqual({ kind: 'conversation', conversationId: 'CONV-1', context: [CHIP] });
    // A bare re-open keeps them; clearing drops the field.
    fireEvent.click(screen.getByRole('button', { name: 'Open chat' }));
    expect(JSON.parse(screen.getByLabelText('chat target').textContent!).context).toEqual([CHIP]);
    fireEvent.click(screen.getByRole('button', { name: 'Clear chips' }));
    expect(JSON.parse(screen.getByLabelText('chat target').textContent!)).toEqual({ kind: 'conversation', conversationId: 'CONV-1' });
    expect(warn).toHaveBeenCalledOnce();
    warn.mockRestore();
  });
});
