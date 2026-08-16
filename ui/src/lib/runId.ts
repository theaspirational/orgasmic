const ULID_ALPHABET = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';

function runLaunchMillis(runId: string): number | null {
  const compact = /^run-([0-7][0-9A-HJKMNP-TV-Z]{25})$/i.exec(runId)?.[1]?.toUpperCase();
  if (compact) {
    let millis = 0;
    for (const char of compact.slice(0, 10)) {
      const value = ULID_ALPHABET.indexOf(char);
      if (value < 0) return null;
      millis = millis * 32 + value;
    }
    return millis;
  }

  const legacy =
    /^run-(\d{4})(\d{2})(\d{2})T(\d{2})(\d{2})(\d{2})-/.exec(runId);
  if (!legacy) return null;
  return Date.UTC(
    Number(legacy[1]),
    Number(legacy[2]) - 1,
    Number(legacy[3]),
    Number(legacy[4]),
    Number(legacy[5]),
    Number(legacy[6]),
  );
}

/** Compare compact and historical run ids by their embedded launch time. */
export function compareRunIdsByLaunch(a: string, b: string): number {
  const aMillis = runLaunchMillis(a);
  const bMillis = runLaunchMillis(b);
  if (aMillis !== null && bMillis !== null && aMillis !== bMillis) {
    return aMillis - bMillis;
  }
  return a.localeCompare(b);
}
