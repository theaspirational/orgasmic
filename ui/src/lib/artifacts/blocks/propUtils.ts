import type { AttrValue } from '../types';

export function asString(value: AttrValue | undefined, fallback = ''): string {
  if (typeof value === 'string') return value;
  if (typeof value === 'number' || typeof value === 'boolean') return String(value);
  return fallback;
}

export function asOptionalString(value: AttrValue | undefined): string | undefined {
  return typeof value === 'string' && value.length > 0 ? value : undefined;
}

export function asNumber(value: AttrValue | undefined, fallback: number): number {
  if (typeof value === 'number') return value;
  if (typeof value === 'string' && value.trim() && !Number.isNaN(Number(value))) return Number(value);
  return fallback;
}

export function asBool(value: AttrValue | undefined, fallback = false): boolean {
  if (typeof value === 'boolean') return value;
  if (value === 'true') return true;
  if (value === 'false') return false;
  return fallback;
}

export function asArray(value: AttrValue | undefined): AttrValue[] {
  return Array.isArray(value) ? value : [];
}

export function asRecord(value: AttrValue | undefined): Record<string, AttrValue> {
  return value && typeof value === 'object' && !Array.isArray(value) ? value : {};
}
