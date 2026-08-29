// @vitest-environment jsdom

import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { NestedTreeRow } from '../NestedTreeRow';

afterEach(cleanup);

describe('NestedTreeRow', () => {
  it('keeps the complete id visible in the shared compact hierarchy shell', () => {
    const { container } = render(
      <NestedTreeRow
        depth={3}
        nodeId="TASK-AJP4A.1.1.1"
        nodeKind="task"
        title="Finish the nested implementation"
        hasChildren
        expanded
        toggleLabel="Collapse Finish the nested implementation"
        onToggle={vi.fn()}
        onOpen={vi.fn()}
        openLabel="Open TASK-AJP4A.1.1.1"
      />,
    );

    const id = screen.getByRole('button', {
      name: 'Copy task id TASK-AJP4A.1.1.1',
    });
    expect(id).toHaveTextContent('TASK-AJP4A.1.1.1');
    expect(id).not.toHaveClass('w-full', 'truncate');
    expect(screen.getByRole('button', { name: 'Open TASK-AJP4A.1.1.1' })).toHaveClass(
      'min-h-11',
    );
    expect(container.querySelector('[aria-hidden]')).toBeInTheDocument();
  });

  it('keeps toggle and row-open interactions independent and keyboard reachable', () => {
    const onToggle = vi.fn();
    const onOpen = vi.fn();

    render(
      <NestedTreeRow
        depth={1}
        nodeId="dec_G38EC"
        nodeKind="decision"
        title="Project add/init is explicit."
        secondary="How do projects register?"
        hasChildren
        expanded
        toggleLabel="Collapse How do projects register?"
        onToggle={onToggle}
        onOpen={onOpen}
        openLabel="Open dec_G38EC"
      />,
    );

    fireEvent.click(screen.getByRole('button', { name: 'Collapse How do projects register?' }));
    expect(onToggle).toHaveBeenCalledOnce();
    expect(onOpen).not.toHaveBeenCalled();

    const openRow = screen.getByRole('button', { name: 'Open dec_G38EC' });
    fireEvent.keyDown(openRow, { key: 'Enter' });
    fireEvent.keyDown(openRow, { key: ' ' });
    expect(onOpen).toHaveBeenCalledTimes(2);
  });

  it('supports narrow hierarchy rails without losing two-line titles', () => {
    const { container } = render(
      <NestedTreeRow
        depth={1}
        nodeId="TASK-GFKFW.1"
        nodeKind="task"
        title="Restore or codify nightly auto-publish behavior"
        indent="compact"
        titleLines={2}
        onOpen={vi.fn()}
        openLabel="Open TASK-GFKFW.1"
      />,
    );

    expect(container.firstElementChild).toHaveStyle('--tree-row-left-desktop: 1.75rem');
    expect(screen.getByText('Restore or codify nightly auto-publish behavior')).toHaveClass(
      'line-clamp-2',
    );
  });

  it('keeps a wide corner control in flow without overlaying the title', () => {
    render(
      <NestedTreeRow
        depth={2}
        nodeId="TASK-O2JJW.2"
        nodeKind="task"
        title="Extract — hermes · google gemini 3 flash chrome"
        corner={<span>Implementer-hermes-studio · working</span>}
        onOpen={vi.fn()}
        openLabel="Open TASK-O2JJW.2"
      />,
    );

    const corner = screen.getByText('Implementer-hermes-studio · working').parentElement;
    expect(corner).toHaveClass('shrink-0');
    expect(corner).not.toHaveClass('absolute');
    expect(corner?.parentElement).toHaveClass('flex', 'flex-col', 'sm:flex-row');
  });
});
