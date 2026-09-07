import { useEffect, useState } from 'react';
import { transport } from '@/lib/transport';
import { TranscriptStream } from '@/lib/transcriptStream';
import { parseSessionSource, type TranscriptPart } from '@/lib/transcriptParts';

export function useTranscriptStream(
  runId: string,
  initialSource?: string | null,
) {
  const [view, setView] = useState<{
    parts: TranscriptPart[];
    responseAt: number;
    loading: boolean;
    error: string | null;
  }>({
    parts: [],
    responseAt: 0,
    loading: true,
    error: null,
  });
  useEffect(() => {
    const stream = new TranscriptStream();
    stream.reset(parseSessionSource(initialSource ?? ''));
    setView({
      parts: stream.parts(),
      responseAt: stream.responseAt,
      loading: true,
      error: null,
    });
    let disposed = false;
    let socket: WebSocket | null = null;
    let retry: ReturnType<typeof setTimeout> | undefined;
    let attempt = 0;
    const connect = () => {
      if (disposed) return;
      const active = transport.openWebSocket(
        `/ws/transcript/${encodeURIComponent(runId)}`,
      );
      socket = active;
      active.onmessage = ({ data }) => {
        if (disposed || socket !== active) return;
        try {
          const frame = JSON.parse(String(data));
          if (frame.type === 'error') throw new Error(frame.message);
          if (
            !['snapshot', 'append'].includes(frame.type) ||
            !Array.isArray(frame.envelopes)
          )
            throw new Error('Invalid transcript frame.');
          if (frame.type === 'snapshot') stream.reset(frame.envelopes);
          else stream.append(frame.envelopes, false, frame.replay === true);
          attempt = 0;
          setView({
            parts: stream.parts(),
            responseAt: stream.responseAt,
            loading: false,
            error: null,
          });
        } catch (error) {
          setView((v) => ({ ...v, loading: false, error: String(error) }));
          active.close();
        }
      };
      active.onclose = () => {
        if (disposed || socket !== active) return;
        setView((v) => ({
          ...v,
          loading: false,
          error: 'Connection lost. Reconnecting…',
        }));
        retry = setTimeout(connect, Math.min(1000 * 2 ** attempt++, 15000));
      };
      active.onerror = () => active.close();
    };
    connect();
    return () => {
      disposed = true;
      clearTimeout(retry);
      socket?.close();
    };
    // initialSource is only a loading placeholder. The stream owns all subsequent data.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [runId]);
  return view;
}
