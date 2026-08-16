// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/api', () => ({
  fetchSkills: vi.fn().mockResolvedValue([]),
}));

import { ManagerComposer } from '../ManagerComposer';

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe('ManagerComposer', () => {
  it('keeps run controls with the message input and sends on Enter', async () => {
    const onSend = vi.fn().mockResolvedValue(true);
    const onSent = vi.fn();

    render(
      <ManagerComposer
        runId="run-live"
        connectionState="open"
        onSend={onSend}
        onSent={onSent}
        controls={<span>Cursor runtime</span>}
        placeholder="Send to agent"
      />,
    );

    expect(screen.getByText('Cursor runtime')).toBeInTheDocument();
    const input = screen.getByRole('textbox', { name: 'Send to agent' });
    fireEvent.change(input, { target: { value: 'Inspect the current task' } });
    fireEvent.keyDown(input, { key: 'Enter' });

    await waitFor(() => {
      expect(onSend).toHaveBeenCalledWith('Inspect the current task');
      expect(onSent).toHaveBeenCalledTimes(1);
    });
    expect(input).toHaveValue('');
  });
});
