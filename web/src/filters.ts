import { DateTime } from 'luxon';

export const presets = [
  ['15m', 'Last 15 minutes'],
  ['1h', 'Last hour'],
  ['6h', 'Last 6 hours'],
  ['24h', 'Last 24 hours'],
  ['7d', 'Last 7 days'],
  ['30d', 'Last 30 days'],
  ['custom', 'Custom range'],
  ['all', 'All time'],
] as const;
export const globalKeys = ['from', 'to', 'camera', 'range', 'tz'];

export function defaultRange() {
  const now = DateTime.utc();
  return new URLSearchParams({
    from: now.minus({ hours: 24 }).toISO()!,
    to: now.toISO()!,
    range: '24h',
  });
}

export function scopeParams(p: URLSearchParams) {
  const q = new URLSearchParams();
  globalKeys.forEach((k) => p.getAll(k).forEach((v) => q.append(k, v)));
  return q;
}

export function apiParams(p: URLSearchParams) {
  const q = new URLSearchParams(p);
  ['range', 'tz', 'view'].forEach((k) => q.delete(k));
  return q.toString();
}

export function imageParams(p: URLSearchParams) {
  const q = new URLSearchParams(p);
  if (!q.has('download_status')) q.set('download_status', 'downloaded');
  if (q.get('download_status') === 'all') q.delete('download_status');
  return apiParams(q);
}

export function imageHref(
  p: URLSearchParams,
  changes: Record<string, string> = {},
) {
  const q = scopeParams(p);
  Object.entries(changes).forEach(([k, v]) => q.set(k, v));
  return `/images?${q}`;
}

export function localToUTC(value: string, zone: string) {
  const date = DateTime.fromISO(value, { zone });
  if (!date.isValid || date.toFormat("yyyy-MM-dd'T'HH:mm") !== value)
    throw new Error('Enter a valid local time in the selected timezone.');
  return date.toUTC().toISO()!;
}
