// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { useState } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { TransportSelection } from '../TransportPicker';

const fetchManagerDriversMock = vi.fn();

vi.mock('@/lib/api', () => ({
  fetchManagerDrivers: (...args: unknown[]) => fetchManagerDriversMock(...args),
}));

import {
  emptyTransportSelection,
  harnessArgTokens,
  TransportPicker,
} from '../TransportPicker';

const driverProfiles = [
  {
    mode: 'rmux',
    harness: 'claude',
    binary: 'claude',
    display_name: 'rmux / claude',
    mode_label: 'rmux',
    harness_label: 'claude',
    installed: true,
    mode_installed: true,
  },
  {
    mode: 'rmux',
    harness: 'custom',
    binary: 'custom',
    display_name: 'rmux / custom',
    mode_label: 'rmux',
    harness_label: 'custom',
    installed: true,
    mode_installed: true,
  },
];

function ControlledPicker({ initial }: { initial: TransportSelection }) {
  const [value, setValue] = useState(initial);
  return (
    <>
      <TransportPicker kindLabel="artifactor" value={value} onChange={setValue} />
      <output data-testid="argv-payload">{JSON.stringify(harnessArgTokens(value.harness_args))}</output>
    </>
  );
}

beforeEach(() => {
  fetchManagerDriversMock.mockResolvedValue({ drivers: driverProfiles });
  Object.defineProperty(window.HTMLElement.prototype, 'hasPointerCapture', {
    value: () => false,
    configurable: true,
  });
  Object.defineProperty(window.HTMLElement.prototype, 'setPointerCapture', {
    value: () => {},
    configurable: true,
  });
  Object.defineProperty(window.HTMLElement.prototype, 'releasePointerCapture', {
    value: () => {},
    configurable: true,
  });
  Object.defineProperty(window.HTMLElement.prototype, 'scrollIntoView', {
    value: () => {},
    configurable: true,
  });
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe('TransportPicker custom argv', () => {
  it('preserves exact tokens including spaces, repeats, and empty strings', async () => {
    render(
      <ControlledPicker
        initial={{
          ...emptyTransportSelection(),
          mode: 'rmux',
          harness: 'custom',
          harness_args: [
            { id: 'row-1', token: 'opencode' },
            { id: 'row-2', token: ' spaced ' },
            { id: 'row-3', token: '' },
            { id: 'row-4', token: 'opencode' },
          ],
        }}
      />,
    );

    await waitFor(() => {
      expect(fetchManagerDriversMock).toHaveBeenCalled();
    });

    expect(screen.getByTestId('argv-payload')).toHaveTextContent(
      JSON.stringify(['opencode', ' spaced ', '', 'opencode']),
    );

    const argvInputs = screen
      .getAllByRole('textbox')
      .filter((input) => (input as HTMLInputElement).readOnly === false);
    expect(screen.getAllByRole('button', { name: 'Remove' })).toHaveLength(4);

    fireEvent.change(argvInputs[0]!, { target: { value: 'changed' } });
    await waitFor(() => {
      expect(screen.getByTestId('argv-payload')).toHaveTextContent(
        JSON.stringify(['changed', ' spaced ', '', 'opencode']),
      );
    });
  });

  it('clears custom argv rows when switching to a built-in harness', async () => {
    render(
      <ControlledPicker
        initial={{
          ...emptyTransportSelection(),
          mode: 'rmux',
          harness: 'custom',
          harness_args: [{ id: 'row-1', token: 'opencode' }],
        }}
      />,
    );

    await waitFor(() => {
      expect(fetchManagerDriversMock).toHaveBeenCalled();
    });

    fireEvent.click(screen.getByRole('combobox'));
    fireEvent.click(await screen.findByText('rmux / claude (rmux/claude)'));

    await waitFor(() => {
      expect(screen.getByTestId('argv-payload')).toHaveTextContent('[]');
      expect(screen.queryByText('Custom argv')).not.toBeInTheDocument();
    });
  });

  it('adds a new argv row with a stable id', async () => {
    render(
      <ControlledPicker
        initial={{
          ...emptyTransportSelection(),
          mode: 'rmux',
          harness: 'custom',
          harness_args: [{ id: 'row-1', token: 'keep' }],
        }}
      />,
    );

    await waitFor(() => {
      expect(fetchManagerDriversMock).toHaveBeenCalled();
    });

    fireEvent.click(screen.getByRole('button', { name: 'Add token' }));

    await waitFor(() => {
      const payload = JSON.parse(screen.getByTestId('argv-payload').textContent ?? '[]') as string[];
      expect(payload).toEqual(['keep', '']);
    });
    expect(screen.getAllByRole('button', { name: 'Remove' })).toHaveLength(2);
  });
});
