// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { Button } from '../button';

afterEach(cleanup);

describe('Button press feedback', () => {
  it('keeps the hit target stationary and still dispatches clicks', () => {
    const onClick = vi.fn();

    render(<Button onClick={onClick}>Toggle panel</Button>);

    const button = screen.getByRole('button', { name: 'Toggle panel' });
    expect(button).toHaveClass('active:brightness-95');
    expect(button.className).not.toMatch(/active:.*translate/);

    fireEvent.click(button);
    expect(onClick).toHaveBeenCalledOnce();
  });
});
