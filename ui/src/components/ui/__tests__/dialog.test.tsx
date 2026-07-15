// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { Dialog, DialogContent, DialogDescription, DialogTitle } from '../dialog';

afterEach(cleanup);

describe('DialogContent close button', () => {
  it('keeps its hit target stationary while pressed and closes normally', () => {
    const onOpenChange = vi.fn();

    render(
      <Dialog open onOpenChange={onOpenChange}>
        <DialogContent>
          <DialogTitle>Example dialog</DialogTitle>
          <DialogDescription>Example description</DialogDescription>
        </DialogContent>
      </Dialog>,
    );

    const close = screen.getByRole('button', { name: 'Close' });
    expect(close).toHaveClass('active:not-aria-[haspopup]:translate-y-0');
    expect(close).not.toHaveClass('active:not-aria-[haspopup]:translate-y-px');

    fireEvent.click(close);
    expect(onOpenChange).toHaveBeenCalledWith(false);
  });
});
