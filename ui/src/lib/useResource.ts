// @arch arch_MK2Q2.3
import { useEffect, useRef, useState } from 'react';

export type UseResourceOptions = {
  enabled?: boolean;
  immediate?: boolean;
  onError?: (err: unknown) => void;
};

export type UseResourceResult<T> = {
  data: T | null;
  error: unknown | null;
  loading: boolean;
  refresh: () => Promise<void>;
};

export function useResource<T>(
  key: string,
  fetcher: () => Promise<T>,
  options: UseResourceOptions = {},
): UseResourceResult<T> {
  const { enabled = true, immediate = true, onError } = options;

  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<unknown | null>(null);
  const [loading, setLoading] = useState(false);

  const fetcherRef = useRef(fetcher);
  fetcherRef.current = fetcher;
  const onErrorRef = useRef(onError);
  onErrorRef.current = onError;
  const activeKeyRef = useRef(key);

  const refresh = useRef(async () => {
    const myKey = activeKeyRef.current;
    setLoading(true);
    try {
      const next = await fetcherRef.current();
      if (activeKeyRef.current !== myKey) return;
      setData(next);
      setError(null);
    } catch (err) {
      if (activeKeyRef.current !== myKey) return;
      setError(err);
      onErrorRef.current?.(err);
    } finally {
      if (activeKeyRef.current === myKey) setLoading(false);
    }
  }).current;

  useEffect(() => {
    activeKeyRef.current = key;
    if (!enabled) {
      setData(null);
      setError(null);
      setLoading(false);
      return;
    }
    if (immediate) void refresh();
    return undefined;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, enabled, immediate]);

  return { data, error, loading, refresh };
}
