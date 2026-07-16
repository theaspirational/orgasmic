// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';

import type { TranscriptToolPart } from '@/lib/transcriptParts';

import { TranscriptToolCard } from '../ManagerChatTranscript';

afterEach(cleanup);

function tool(overrides: Partial<TranscriptToolPart> = {}): TranscriptToolPart {
  return {
    id: 'tool-1',
    type: 'tool',
    callId: 'call-1',
    name: 'exec_command',
    label: 'command request',
    state: 'completed',
    input: { cmd: 'npm test', workdir: '/repo' },
    output: 'Chunk ID: abc\nOutput:\n226 tests passed',
    ok: true,
    summary: 'exit 0',
    meta: [['cwd', '/repo']],
    time: '2026-07-16T10:00:00Z',
    ...overrides,
  };
}

describe('TranscriptToolCard', () => {
  it('renders the tool status and reveals input, output, and metadata', () => {
    render(<TranscriptToolCard part={tool()} />);

    expect(screen.getByText('Completed')).toBeInTheDocument();
    expect(screen.getByText('command request: exit 0')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: /command request: exit 0/i }));

    expect(screen.getByText('Parameters')).toBeInTheDocument();
    expect(screen.getByText('Result')).toBeInTheDocument();
    expect(screen.getByText('/repo')).toHaveAttribute('title', '/repo');
    expect(screen.getByText(/npm test/)).toBeInTheDocument();
    expect(screen.getByText(/226 tests passed/)).toBeInTheDocument();
  });

  it('opens errors by default and keeps the failed command output visible', () => {
    render(
      <TranscriptToolCard
        part={tool({ state: 'error', ok: false, output: 'permission denied', summary: 'exit 1' })}
      />,
    );

    expect(screen.getAllByText('Error')).toHaveLength(2);
    expect(screen.getByText('Tool returned an error.')).toBeInTheDocument();
    expect(screen.getByText('permission denied')).toBeInTheDocument();
  });

  it('shows the fallback error detail when a failed tool has empty output', () => {
    render(<TranscriptToolCard part={tool({ state: 'error', ok: false, output: null })} />);

    expect(screen.getByText('Tool returned an error.')).toBeInTheDocument();
  });

  it('uses the streaming status label for partial tool input', () => {
    render(<TranscriptToolCard part={tool({ id: 'tool-stream', state: 'streaming', output: null, ok: null })} />);
    expect(screen.getByText('Streaming')).toBeInTheDocument();
  });
});
