// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const mocks = vi.hoisted(() => ({
  copyText: vi.fn(),
  toastSuccess: vi.fn(),
  toastError: vi.fn(),
}));

vi.mock('@/lib/clipboard', () => ({
  copyText: mocks.copyText,
}));

vi.mock('sonner', () => ({
  toast: {
    success: mocks.toastSuccess,
    error: mocks.toastError,
  },
}));

import { CopyIdBadge } from '../CopyIdBadge';

beforeEach(() => {
  vi.clearAllMocks();
  mocks.copyText.mockResolvedValue(undefined);
});

afterEach(cleanup);

describe('CopyIdBadge', () => {
  it('offers an explicit dismiss action on copy confirmation', async () => {
    render(<CopyIdBadge value="dec_4TVD9" />);

    fireEvent.click(screen.getByRole('button', { name: 'Copy dec_4TVD9' }));

    await waitFor(() => {
      expect(mocks.toastSuccess).toHaveBeenCalledWith('Copied dec_4TVD9', {
        cancel: {
          label: 'Dismiss',
          onClick: expect.any(Function),
        },
      });
    });
  });
});
