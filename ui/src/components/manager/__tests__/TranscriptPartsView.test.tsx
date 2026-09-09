// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, expect, it } from 'vitest';
import { TranscriptPartsView } from '../ManagerChatTranscript';
import type { TranscriptPart } from '@/lib/transcriptParts';
afterEach(cleanup);
it('groups tools without swallowing model text and keeps reasoning expandable', () => {
  const tools: TranscriptPart[] = Array.from({ length: 8 }, (_, i) => ({
    type: 'tool',
    id: String(i),
    name: 'command_execution',
    label: 'Ran command',
    summary: `command-${i}`,
    state: 'completed',
    input: null,
    output: null,
    ok: true,
    meta: [],
  }));
  render(
    <TranscriptPartsView
      parts={[
        {
          type: 'text',
          id: 'before',
          role: 'assistant',
          label: 'assistant',
          text: 'Before activity',
        },
        ...tools,
        {
          type: 'reasoning',
          id: 'thought',
          label: 'thinking',
          text: 'Vendor reasoning text',
          state: 'streaming',
        },
        {
          type: 'text',
          id: 'after',
          role: 'assistant',
          label: 'assistant',
          text: 'After activity',
        },
      ]}
    />,
  );
  expect(screen.getAllByTestId('transcript-activity')).toHaveLength(1);
  expect(screen.getByText('Before activity')).toBeVisible();
  expect(screen.getByText('After activity')).toBeVisible();
  expect(screen.queryByText('Vendor reasoning text')).not.toBeInTheDocument();
  const activity = screen.getByText(/8 tools/).closest('details')!;
  expect(activity.open).toBe(false);
  expect(screen.queryAllByTestId(/transcript-tool-/)).toHaveLength(0);
  activity.open = true;
  fireEvent(activity, new Event('toggle'));
  expect(screen.getAllByTestId(/transcript-tool-/)).toHaveLength(8);
  fireEvent.click(screen.getByRole('button', { name: /thinking/i }));
  expect(screen.getByText('Vendor reasoning text')).toBeVisible();
});

it('renders a sent conversation message as its chips plus the operator text', () => {
  const sent = ['<<<orgasmic-context', '{"kind":"node","id":"dec_1"}',
    '{"kind":"range","node":"MEET-1","attachment":"rec","revision":"sha","start_ms":90000,"end_ms":120000}',
    '>>>', 'what was decided?'].join('\n');
  render(
    <TranscriptPartsView
      parts={[
        { type: 'text', id: 'sent', role: 'user', label: 'user', text: sent },
        { type: 'text', id: 'plain', role: 'user', label: 'user', text: 'no block here' },
      ]}
    />,
  );
  const chips = screen.getByTestId('context-chips');
  expect(chips).toHaveTextContent('dec_1');
  expect(chips).toHaveTextContent('1:30–2:00');
  expect(screen.getByText('what was decided?')).toBeInTheDocument();
  expect(screen.queryByText(/orgasmic-context/)).toBeNull();
  expect(screen.getByText('no block here')).toBeInTheDocument();
  expect(screen.getAllByTestId('context-chips')).toHaveLength(1);
});
