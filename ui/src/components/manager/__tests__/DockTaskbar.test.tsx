// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { TooltipProvider } from '@/components/ui/tooltip';

import { DockTaskbar } from '../DockTaskbar';

afterEach(cleanup);

function taskbar(maximized: boolean, onMaximize: () => void) {
  return (
    <TooltipProvider>
      <DockTaskbar
        open
        maximized={maximized}
        readOnly={false}
        terminalBusy={false}
        chatActive={false}
        buttons={[]}
        activeTabId={null}
        onTerminalLaunch={vi.fn()}
        onChatOpen={vi.fn()}
        onSelect={vi.fn()}
        onStop={vi.fn()}
        onDismiss={vi.fn()}
        onMaximize={onMaximize}
        onMinimize={vi.fn()}
        onRestore={vi.fn()}
        resizeHandlers={{
          onPointerDown: vi.fn(),
          onPointerMove: vi.fn(),
          onPointerUp: vi.fn(),
          onPointerCancel: vi.fn(),
        }}
        runningAgents={null}
      />
    </TooltipProvider>
  );
}

describe('DockTaskbar maximize control', () => {
  it('offers a 44px maximize button only while the dock is partial', () => {
    const onMaximize = vi.fn();
    const { rerender } = render(taskbar(false, onMaximize));

    const maximize = screen.getByRole('button', { name: 'Expand dock to full screen' });
    expect(maximize).toHaveClass('size-11');
    fireEvent.click(maximize);
    expect(onMaximize).toHaveBeenCalledOnce();

    rerender(taskbar(true, onMaximize));
    expect(screen.queryByRole('button', { name: 'Expand dock to full screen' })).toBeNull();
    expect(screen.getByRole('button', { name: 'Minimize dock' })).toBeInTheDocument();
  });
});
