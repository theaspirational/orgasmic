import { useEffect, useState, useCallback } from 'react';

export function getQueryParam(key: string): string | null {
  return new URLSearchParams(window.location.search).get(key);
}

export function navigate(patch: Record<string, string | null | undefined>): void {
  const params = new URLSearchParams(window.location.search);
  for (const [key, value] of Object.entries(patch)) {
    if (value === null || value === undefined || value === '') params.delete(key);
    else params.set(key, value);
  }
  const query = params.toString();
  const next = `${window.location.pathname}${query ? `?${query}` : ''}${window.location.hash}`;
  const current = `${window.location.pathname}${window.location.search}${window.location.hash}`;
  if (current !== next) window.history.replaceState(null, '', next);
}

export function useQueryState(
  key: string,
  defaultValue: string | null = null,
): [string | null, (value: string | null) => void] {
  const [value, setValue] = useState<string | null>(() => getQueryParam(key) ?? defaultValue);

  const update = useCallback(
    (next: string | null) => {
      setValue(next);
      navigate({ [key]: next });
    },
    [key],
  );

  useEffect(() => {
    const onPop = () => setValue(getQueryParam(key) ?? defaultValue);
    window.addEventListener('popstate', onPop);
    return () => window.removeEventListener('popstate', onPop);
  }, [key, defaultValue]);

  return [value, update];
}
