import { useContext, useRef, useState } from 'react';
import { useSearchParams } from 'react-router-dom';
import { DateTime } from 'luxon';
import {
  CalendarDays,
  Camera as CameraIcon,
  SlidersHorizontal,
  X,
} from 'lucide-react';
import { nameOf, type Camera } from './api';
import { defaultRange, localToUTC, presets } from './filters';
import { Zone } from './ui';

export function FilterBar({ cameras }: { cameras: Camera[] }) {
  const [params, setParams] = useSearchParams();
  const zone = useContext(Zone);
  const dialog = useRef<HTMLDialogElement>(null);
  const [draft, setDraft] = useState(params);
  const [search, setSearch] = useState('');
  const [error, setError] = useState('');
  const [datesEdited, setDatesEdited] = useState(false);
  const [from, setFrom] = useState('');
  const [to, setTo] = useState('');
  const [tz, setTz] = useState(zone);
  const [conversionZone, setConversionZone] = useState(zone);
  const range =
    params.get('range') ||
    (params.has('from') || params.has('to') ? 'custom' : 'all');
  const ids = params.getAll('camera').flatMap((v) => v.split(','));

  function open() {
    setDraft(new URLSearchParams(params));
    setTz(zone);
    setConversionZone(zone);
    setFrom(
      params.get('from')
        ? DateTime.fromISO(params.get('from')!)
            .setZone(zone)
            .toFormat("yyyy-MM-dd'T'HH:mm")
        : '',
    );
    setTo(
      params.get('to')
        ? DateTime.fromISO(params.get('to')!)
            .setZone(zone)
            .toFormat("yyyy-MM-dd'T'HH:mm")
        : '',
    );
    setError('');
    setDatesEdited(false);
    dialog.current?.showModal();
  }

  function apply() {
    const next = new URLSearchParams(draft);
    try {
      if (!DateTime.now().setZone(tz).isValid)
        throw new Error('Choose a valid IANA timezone.');
      const r = next.get('range') || range;
      if (r === 'custom' && (datesEdited || range !== 'custom')) {
        const start = localToUTC(from, tz),
          end = localToUTC(to, tz);
        if (start >= end) throw new Error('End must be later than start.');
        next.set('from', start);
        next.set('to', end);
      } else if (r === 'all') {
        next.delete('from');
        next.delete('to');
      } else if (r !== range && r !== 'custom') {
        const hours: Record<string, number> = {
          '15m': 0.25,
          '1h': 1,
          '6h': 6,
          '24h': 24,
          '7d': 168,
          '30d': 720,
        };
        const now = DateTime.utc();
        next.set('from', now.minus({ hours: hours[r || '24h'] }).toISO()!);
        next.set('to', now.toISO()!);
      }
      next.set('tz', tz);
      next.delete('cursor');
      setParams(next);
      dialog.current?.close();
    } catch (e) {
      setError((e as Error).message);
    }
  }

  const selected = draft.getAll('camera').flatMap((v) => v.split(','));

  function select(values: string[]) {
    const next = new URLSearchParams(draft);
    next.delete('camera');
    values.forEach((v) => next.append('camera', v));
    setDraft(next);
  }

  return (
    <>
      <div className="filter-bar">
        <button onClick={open}>
          <CalendarDays size={16} />
          {presets.find((p) => p[0] === range)?.[1] || 'Custom range'}
        </button>
        <span className="filter-divider" />
        <button onClick={open}>
          <CameraIcon size={16} />
          {ids.length
            ? `${ids.length} camera${ids.length > 1 ? 's' : ''}`
            : 'All cameras'}
        </button>
        <span className="timezone">{zone}</span>
        <button className="filter-edit" onClick={open}>
          <SlidersHorizontal size={15} />
          Filters
        </button>
        {(ids.length > 0 || range !== '24h') && (
          <button
            className="text-button"
            onClick={() => setParams(defaultRange())}
          >
            Reset
          </button>
        )}
      </div>
      {ids.length > 0 && (
        <div className="chips">
          {ids.map((id) => (
            <button
              key={id}
              onClick={() => {
                const next = new URLSearchParams(params);
                next.delete('camera');
                ids
                  .filter((i) => i !== id)
                  .forEach((i) => next.append('camera', i));
                setParams(next);
              }}
            >
              {nameOf(
                cameras.find((c) => String(c.id) === id) || {
                  name: `Camera ${id}`,
                },
              )}
              <X size={12} />
            </button>
          ))}
        </div>
      )}
      <dialog
        ref={dialog}
        className="filter-dialog"
        aria-labelledby="filter-title"
      >
        <form
          onSubmit={(e) => {
            e.preventDefault();
            apply();
          }}
        >
          <div className="row">
            <h2 id="filter-title">Time & cameras</h2>
            <button
              type="button"
              aria-label="Close filters"
              onClick={() => dialog.current?.close()}
            >
              <X size={20} />
            </button>
          </div>
          <label>
            Time range
            <select
              aria-label="Time range"
              value={draft.get('range') || range}
              onChange={(e) => {
                const next = new URLSearchParams(draft);
                next.set('range', e.target.value);
                setDraft(next);
              }}
            >
              {presets.map(([v, l]) => (
                <option key={v} value={v}>
                  {l}
                </option>
              ))}
            </select>
          </label>
          <label>
            Display timezone
            <input
              value={tz}
              onChange={(e) => {
                const next = e.target.value;
                if (DateTime.now().setZone(next).isValid) {
                  if (from)
                    setFrom(
                      DateTime.fromISO(from, { zone: conversionZone })
                        .setZone(next)
                        .toFormat("yyyy-MM-dd'T'HH:mm"),
                    );
                  if (to)
                    setTo(
                      DateTime.fromISO(to, { zone: conversionZone })
                        .setZone(next)
                        .toFormat("yyyy-MM-dd'T'HH:mm"),
                    );
                  setConversionZone(next);
                }
                setTz(next);
              }}
              list="timezones"
            />
            <datalist id="timezones">
              {[
                zone,
                'UTC',
                'Asia/Kolkata',
                'Europe/London',
                'America/New_York',
                'America/Los_Angeles',
              ].map((z, i) => (
                <option key={i} value={z} />
              ))}
            </datalist>
          </label>
          {(draft.get('range') || range) === 'custom' && (
            <div className="grid gap-4 sm:grid-cols-2">
              <label>
                From (inclusive)
                <input
                  type="datetime-local"
                  required
                  value={from}
                  onChange={(e) => {
                    setFrom(e.target.value);
                    setDatesEdited(true);
                  }}
                />
              </label>
              <label>
                To (exclusive)
                <input
                  type="datetime-local"
                  required
                  value={to}
                  onChange={(e) => {
                    setTo(e.target.value);
                    setDatesEdited(true);
                  }}
                />
              </label>
            </div>
          )}
          <fieldset>
            <legend>Cameras</legend>
            <input
              aria-label="Search cameras"
              placeholder="Search name or channel…"
              value={search}
              onChange={(e) => setSearch(e.target.value)}
            />
            <div className="actions">
              <button type="button" onClick={() => select([])}>
                All cameras
              </button>
              <button
                type="button"
                onClick={() =>
                  select(
                    cameras.filter((c) => c.enabled).map((c) => String(c.id)),
                  )
                }
              >
                Select active
              </button>
            </div>
            <div className="camera-options">
              {cameras
                .filter((c) =>
                  `${nameOf(c)} ${c.channel_number}`
                    .toLowerCase()
                    .includes(search.toLowerCase()),
                )
                .map((c) => (
                  <label className="check-label" key={c.id}>
                    <input
                      type="checkbox"
                      checked={selected.includes(String(c.id))}
                      onChange={(e) =>
                        select(
                          e.target.checked
                            ? [...selected, String(c.id)]
                            : selected.filter((i) => i !== String(c.id)),
                        )
                      }
                    />
                    {nameOf(c)}{' '}
                    <span className="muted">
                      Ch {c.channel_number}
                      {!c.enabled ? ' · inactive' : ''}
                    </span>
                  </label>
                ))}
            </div>
          </fieldset>
          {error && (
            <p role="alert" className="text-red-700">
              {error}
            </p>
          )}
          <div className="dialog-footer">
            <button
              type="button"
              onClick={() => {
                setParams(defaultRange());
                dialog.current?.close();
              }}
            >
              Reset filters
            </button>
            <button className="primary" type="submit">
              Apply filters
            </button>
          </div>
        </form>
      </dialog>
    </>
  );
}
