const PREFIX = 'orgasmic:';

function keyFor(key: string): string {
  return `${PREFIX}${key}`;
}

export function getJson<T>(key: string, fallback: T): T {
  try {
    const raw = window.localStorage.getItem(keyFor(key));
    if (!raw) return fallback;
    return JSON.parse(raw) as T;
  } catch {
    return fallback;
  }
}

export function setJson<T>(key: string, value: T): void {
  try {
    window.localStorage.setItem(keyFor(key), JSON.stringify(value));
  } catch {
    /* ignore */
  }
}

export function getString(key: string): string | null {
  try {
    return window.localStorage.getItem(keyFor(key));
  } catch {
    return null;
  }
}

export function setString(key: string, value: string): void {
  try {
    window.localStorage.setItem(keyFor(key), value);
  } catch {
    /* ignore */
  }
}
