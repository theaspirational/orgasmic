import { projectAcpEnvelope } from './acpTranscript';
import {
  createTranscriptReducer,
  extractPromptBundle,
  type SessionEnvelope,
  type TranscriptPart,
} from './transcriptParts';

/** Sequence-checked incremental fold. Reconnect snapshots replace, never append. */
export class TranscriptStream {
  private envelopes: SessionEnvelope[] = [];
  private reducer = createTranscriptReducer();
  private canonical = false;
  private prompt: string | null = null;
  private lastSeq: number | null = null;
  responseAt = 0;

  reset(envelopes: SessionEnvelope[]): void {
    this.envelopes = [];
    this.lastSeq = null;
    this.responseAt = 0;
    this.canonical = false;
    this.append(envelopes, true, true);
  }

  append(incoming: SessionEnvelope[], reset = false, replay = false): void {
    const fresh: SessionEnvelope[] = [];
    let cursor = this.lastSeq;
    for (const envelope of incoming) {
      const seq = envelope.delivery_seq ?? envelope.seq;
      if (!Number.isSafeInteger(seq))
        throw new Error('Invalid transcript sequence.');
      if (cursor !== null && seq! <= cursor) continue;
      if (!replay && cursor !== null && seq !== cursor + 1) {
        throw new Error(
          'Transcript sequence gap; reconnecting to recover history.',
        );
      }
      cursor = seq!;
      fresh.push(envelope);
    }
    if (!fresh.length && !reset) return;
    this.lastSeq = cursor;
    this.envelopes.push(...fresh);
    const firstCanonical =
      !this.canonical &&
      fresh.some((e) =>
        ['provider_runtime', 'acp'].includes(String(e.event?.type)),
      );
    this.canonical ||= firstCanonical;
    // Lifecycle/user echoes can refer backwards (prompt metadata and composer dedup).
    // Replay at these infrequent boundaries; content/tool deltas only fold new events.
    const rebase =
      reset || firstCanonical || fresh.some((e) => e.kind === 'lifecycle');
    if (rebase) {
      this.prompt = extractPromptBundle(this.envelopes) ?? this.prompt;
      this.reducer = createTranscriptReducer(this.envelopes, {
        promptOverride: this.prompt,
      });
    } else {
      this.reducer.append(fresh);
    }
    for (const envelope of fresh) {
      const event = projectAcpEnvelope(envelope).event ?? {};
      const runtime =
        event.type === 'provider_runtime'
          ? (event.event as Record<string, unknown>)
          : undefined;
      const payload = runtime?.payload as Record<string, unknown> | undefined;
      const response =
        (event.type === 'text_chunk' && event.stream === 'assistant') ||
        ['run_complete', 'run_fail', 'driver_error'].includes(
          String(event.type),
        ) ||
        ['turn.completed', 'turn.aborted', 'runtime.error'].includes(
          String(runtime?.type),
        ) ||
        (runtime?.type === 'content.delta' &&
          payload?.streamKind === 'assistant_text');
      if (response)
        this.responseAt = Math.max(
          this.responseAt,
          Date.parse(String(runtime?.createdAt ?? envelope.time)) || 0,
        );
    }
  }

  parts(): TranscriptPart[] {
    return this.reducer.parts();
  }
}
