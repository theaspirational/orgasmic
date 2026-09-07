import type { SessionEnvelope } from './transcriptParts';

type RecordValue = Record<string, unknown>;
const record = (v: unknown): RecordValue =>
  v !== null && typeof v === 'object' && !Array.isArray(v)
    ? (v as RecordValue)
    : {};
const text = (v: unknown) => (typeof v === 'string' ? v : '');

/** ACP stays intact in storage. This projection only decides how to display it. */
export function projectAcpEnvelope(envelope: SessionEnvelope): SessionEnvelope {
  if (envelope.event?.type !== 'acp') return envelope;
  const message = record(envelope.event.message);
  const params = record(message.params ?? message);
  const update = record(params.update ?? params);
  const type = text(update.sessionUpdate);
  const content = record(update.content);
  const canonical = (
    type: string,
    payload: RecordValue,
    itemId?: string,
  ): SessionEnvelope => ({
    ...envelope,
    event: {
      type: 'provider_runtime',
      event: { type, payload, itemId, createdAt: envelope.time },
    },
  });
  const notice = (
    label: string,
    detail: unknown = params,
  ): SessionEnvelope => ({
    ...envelope,
    event: { type: 'protocol_notice', label, detail },
  });
  if (message.method === 'session/prompt') {
    const reason = text(params.stopReason);
    return canonical('turn.completed', {
      state: ['end_turn', 'max_tokens'].includes(reason)
        ? 'completed'
        : 'failed',
      reason: ['end_turn', 'max_tokens'].includes(reason) ? undefined : reason,
    });
  }
  if (message.method !== 'session/update') {
    if (
      [
        'session/new',
        'session/set_config_option',
        'session/set_model',
        'session/set_mode',
      ].includes(text(message.method))
    ) {
      return canonical('thread.metadata.updated', {});
    }
    return notice(`ACP ${text(message.method) || 'message'}`);
  }
  // The client records submitted text before sending session/prompt. Vendor
  // user-message notifications echo that same input.
  if (type === 'user_message_chunk')
    return canonical('thread.metadata.updated', {});
  if (['agent_message_chunk', 'agent_thought_chunk'].includes(type)) {
    if (content.type !== 'text')
      return notice(`Agent ${text(content.type) || 'content'}`, content);
    // Legacy stream merging already handles adjacent message chunks with no message id.
    if (!update.messageId)
      return {
        ...envelope,
        event: {
          type: 'text_chunk',
          stream: type === 'agent_message_chunk' ? 'assistant' : 'system',
          chunk: text(content.text),
          acpReasoning: type === 'agent_thought_chunk',
        },
      };
    return canonical(
      'content.delta',
      {
        streamKind:
          type === 'agent_message_chunk' ? 'assistant_text' : 'reasoning_text',
        delta: text(content.text),
      },
      text(update.messageId),
    );
  }
  if (type === 'tool_call' || type === 'tool_call_update') {
    const kind = text(update.kind);
    const names: Record<string, string> = {
      execute: 'command_execution',
      read: 'file_read',
      edit: 'file_change',
      delete: 'file_change',
      move: 'file_change',
      search: 'web_search',
      fetch: 'web_search',
      think: 'agent_task',
    };
    const output = record(update.rawOutput);
    const exitCode =
      output.exitCode ?? output.exit_code ?? record(output.metadata).exit;
    const status =
      update.status === 'completed' &&
      typeof exitCode === 'number' &&
      exitCode !== 0
        ? 'failed'
        : text(update.status);
    const terminal = status === 'completed' || status === 'failed';
    return canonical(
      terminal
        ? 'item.completed'
        : type === 'tool_call'
          ? 'item.started'
          : 'item.updated',
      {
        itemType: names[kind] ?? 'tool_call',
        acp: true,
        status: status === 'in_progress' ? 'inProgress' : status,
        title: update.title,
        data: {
          input: update.rawInput,
          output: update.rawOutput ?? update.content,
          locations: update.locations,
        },
      },
      text(update.toolCallId),
    );
  }
  if (type === 'plan') return notice('Plan', update.entries);
  if (
    [
      'usage_update',
      'config_option_update',
      'available_commands_update',
      'current_mode_update',
      'session_info_update',
    ].includes(type)
  ) {
    return canonical('thread.metadata.updated', {});
  }
  return notice(`Unrecognized ACP update: ${type || 'unknown'}`, update);
}
