import { describe, expect, it } from 'vitest';

import { transcriptBlocks, type TranscriptEntry } from '@/lib/transcriptUtils';

function tool(id: string): TranscriptEntry {
  return { id, role: 'tool', label: 'command request', text: `cmd ${id}`, callId: id };
}

function assistant(id: string): TranscriptEntry {
  return { id, role: 'assistant', label: 'assistant', text: `msg ${id}` };
}

describe('transcriptBlocks tool-group keying', () => {
  it('keeps the tool-group id stable as new tool calls stream in', () => {
    // A consecutive run of tool calls forms one group. Appending another tool
    // call must NOT change the group's id, or the <details> remounts and the
    // user's manual expand is lost (issue: auto-collapse on new tool call).
    const before = transcriptBlocks([assistant('a0'), tool('t1'), tool('t2')]);
    const after = transcriptBlocks([assistant('a0'), tool('t1'), tool('t2'), tool('t3')]);

    const groupBefore = before.find((b) => b.type === 'tool-group');
    const groupAfter = after.find((b) => b.type === 'tool-group');

    expect(groupBefore?.id).toBe('t1:tools');
    expect(groupAfter?.id).toBe('t1:tools');
    expect(groupBefore?.id).toBe(groupAfter?.id);
  });

  it('groups consecutive tool entries and breaks the group on a non-tool entry', () => {
    const blocks = transcriptBlocks([tool('t1'), tool('t2'), assistant('a1'), tool('t3')]);
    const groups = blocks.filter((b) => b.type === 'tool-group');
    expect(groups).toHaveLength(2);
    expect(groups[0].id).toBe('t1:tools');
    expect(groups[1].id).toBe('t3:tools');
  });
});
