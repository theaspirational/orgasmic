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
    expect(screen.getByText('Parameters').nextElementSibling).toHaveClass(
      'max-h-64',
      'overflow-auto',
    );
    expect(screen.getByText('Result').nextElementSibling).toHaveClass(
      'max-h-80',
      'overflow-auto',
    );
  });

  it('renders canonical commands in the compact T3-style header', () => {
    render(
      <TranscriptToolCard
        part={tool({
          name: 'command_execution',
          label: 'Ran command',
          input: { command: 'orgasmic entry' },
          output: 'ORGASMIC_ENTRY_FAST_V1 abc123',
          summary: 'orgasmic entry',
        })}
      />,
    );

    const trigger = screen.getByRole('button', { name: /Ran command orgasmic entry/i });
    expect(trigger).toBeInTheDocument();
    expect(screen.queryByText(/ORGASMIC_ENTRY_FAST_V1/)).not.toBeInTheDocument();

    fireEvent.click(trigger);
    expect(screen.getByText(/ORGASMIC_ENTRY_FAST_V1/)).toBeInTheDocument();
  });

  it('keeps a long command summary inside the tool header', () => {
    const summary =
      "rg -n 'capabilit|supports_.*input|input.*support|is_chat|chat_surface|RunSurfaceKind|surface' crates/orgasmic-core/src/session.rs crates/orgasmic-drivers/src/trait.rs";
    render(
      <TranscriptToolCard
        part={tool({
          name: 'command_execution',
          label: 'Ran command',
          summary,
        })}
      />,
    );

    const title = screen.getByText(`Ran command ${summary}`);
    const trigger = screen.getByRole('button', { name: /Ran command rg -n/i });
    expect(trigger).toHaveClass('min-w-0', 'text-left');
    expect(title).toHaveClass('min-w-0', 'flex-1', 'truncate');
    expect(title).toHaveAttribute('title', `Ran command ${summary}`);
    expect(screen.getByText('Completed')).toHaveClass('shrink-0');
  });

  it('keeps fetched page output behind a compact activity row', () => {
    const output = 'Providers | OpenCode — very large fetched page body';
    render(
      <TranscriptToolCard
        part={tool({
          name: 'web_search',
          label: 'Fetched page',
          input: { url: 'https://opencode.ai/docs/providers/' },
          output,
          summary: 'https://opencode.ai/docs/providers/',
        })}
      />,
    );

    const trigger = screen.getByRole('button', {
      name: /Fetched page https:\/\/opencode\.ai\/docs\/providers\//i,
    });
    expect(trigger).toBeInTheDocument();
    expect(screen.queryByText(output)).not.toBeInTheDocument();

    fireEvent.click(trigger);
    expect(screen.getByText(output)).toBeInTheDocument();
  });

  it('opens errors by default and keeps the failed command output visible', () => {
    render(
      <TranscriptToolCard
        part={tool({ state: 'error', ok: false, output: 'permission denied', summary: 'exit 1' })}
      />,
    );

    expect(screen.getAllByText('Error')).toHaveLength(2);
    expect(screen.getByRole('alert')).toBeInTheDocument();
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
