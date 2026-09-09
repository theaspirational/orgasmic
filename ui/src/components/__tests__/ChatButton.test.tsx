// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';

const mocks = vi.hoisted(() => ({ openChat: vi.fn(), dock: true, write: true }));
vi.mock('@/lib/runDock', () => ({ useOptionalRunDock: () => (mocks.dock ? { openChat: mocks.openChat } : null) }));
vi.mock('@/hooks/useMe', () => ({ useMe: () => ({ can: (_project: string, action: string) => action !== 'chat.write' || mocks.write }) }));

import { ChatButton } from '../ChatButton';

afterEach(() => { cleanup(); vi.clearAllMocks(); mocks.dock = true; mocks.write = true; });

it('opens the node conversation in the dock', () => {
  render(<ChatButton projectId="demo" node="dec_1" />);
  fireEvent.click(screen.getByRole('button', { name: 'Chat about dec_1' }));
  expect(mocks.openChat).toHaveBeenCalledWith({ node: 'dec_1' });
});

it('opens a project-scoped chat without a node', () => {
  render(<ChatButton projectId="demo" />);
  fireEvent.click(screen.getByRole('button', { name: 'Chat' }));
  expect(mocks.openChat).toHaveBeenCalledWith({});
});

it('is hidden without chat.write and outside the dock provider', () => {
  mocks.write = false;
  render(<ChatButton projectId="demo" node="dec_1" />);
  expect(screen.queryByRole('button')).toBeNull();
  cleanup();
  mocks.write = true;
  mocks.dock = false;
  render(<ChatButton projectId="demo" node="dec_1" />);
  expect(screen.queryByRole('button')).toBeNull();
});
