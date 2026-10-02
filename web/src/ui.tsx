import { createContext, useContext, useState, type ReactNode } from 'react';
import { DateTime } from 'luxon';
import { Link } from 'react-router-dom';
import {
  AlertCircle,
  Camera as CameraIcon,
  Check,
  Clock,
  Copy,
  ExternalLink,
  ImageOff,
  Leaf,
  LoaderCircle,
  MoreHorizontal,
  Star,
} from 'lucide-react';
import {
  api,
  nameOf,
  percent,
  speciesOf,
  type ImageDetail,
  type ImageRecord,
} from './api';

export const Zone = createContext(
  Intl.DateTimeFormat().resolvedOptions().timeZone,
);
export const Toast = createContext<(s: string) => void>(() => {});

export function Time({ value }: { value?: string | null }) {
  const zone = useContext(Zone);
  if (!value) return <span>Not recorded</span>;
  const d = DateTime.fromISO(value).setZone(zone);
  return (
    <time dateTime={value} title={d.toFormat('dd LLL yyyy, HH:mm:ss ZZZZ')}>
      {d.toFormat('dd LLL, HH:mm:ss')}
    </time>
  );
}

export function Badge({
  value,
  category,
}: {
  value: string;
  category?: string;
}) {
  const good = ['downloaded', 'done', 'healthy', 'ok'].includes(value);
  const bad = [
    'failed',
    'missing',
    'unavailable',
    'stale claim',
    'stopped',
  ].includes(value);
  const active = ['processing', 'downloading', 'active'].includes(value);
  const warn = ['retry_wait', 'retrying', 'degraded'].includes(value);
  const Icon = good
    ? Check
    : bad || warn
      ? AlertCircle
      : active
        ? LoaderCircle
        : Clock;
  return (
    <span
      className={`badge ${good ? 'good' : bad ? 'bad' : active ? 'active' : warn ? 'warn' : 'neutral'}`}
    >
      <Icon size={12} />
      {category ? `${category}: ` : ''}
      {value.replaceAll('_', ' ')}
    </span>
  );
}

export function Empty({
  title,
  children,
}: {
  title: string;
  children?: ReactNode;
}) {
  return (
    <div className="empty">
      <CameraIcon size={34} />
      <h3>{title}</h3>
      <div className="muted">{children}</div>
    </div>
  );
}

export function ErrorBox({
  error,
  retry,
}: {
  error: unknown;
  retry?: () => void;
}) {
  return (
    <div role="alert" className="error-box">
      <AlertCircle size={18} />
      <span>
        {error instanceof Error ? error.message : 'Unable to load data.'}
      </span>
      {retry && <button onClick={retry}>Try again</button>}
    </div>
  );
}

export function Loading() {
  return (
    <div className="loading" role="status">
      <LoaderCircle size={20} className="spin" />
      Loading…
    </div>
  );
}

export function Photo({
  src,
  alt,
  className = '',
}: {
  src?: string | null;
  alt: string;
  className?: string;
}) {
  const [failed, setFailed] = useState(false);
  return src && !failed ? (
    <img
      className={`photo ${className}`}
      src={src}
      alt={alt}
      loading="lazy"
      onError={() => setFailed(true)}
    />
  ) : (
    <div className={`photo placeholder ${className}`}>
      <ImageOff size={26} />
      <span>{failed ? 'Local image unavailable' : 'Image not downloaded'}</span>
    </div>
  );
}

export function CopyButton({
  text,
  label = 'Copy',
}: {
  text: string;
  label?: string;
}) {
  const toast = useContext(Toast);
  return (
    <button
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(text);
          toast('Copied to clipboard');
        } catch {
          toast('Copy unavailable. Select and copy the displayed value.');
        }
      }}
    >
      <Copy size={14} />
      {label}
    </button>
  );
}

export function SafeURL({
  url,
  label,
}: {
  url?: string | null;
  label: string;
}) {
  let safe: URL | null = null;
  try {
    const parsed = new URL(url || '');
    if (
      ['http:', 'https:', 'rtsp:', 'rtsps:'].includes(parsed.protocol) &&
      !parsed.username &&
      !parsed.password
    )
      safe = parsed;
  } catch {
    /* unavailable */
  }
  return (
    <div className="url-field">
      <h4>{label}</h4>
      {safe ? (
        <>
          <code>{safe.href}</code>
          <div className="actions">
            <CopyButton text={safe.href} />
            <a
              className="button"
              href={safe.href}
              target="_blank"
              rel="noopener noreferrer"
            >
              <ExternalLink size={14} />
              {safe.protocol.startsWith('rtsp')
                ? 'Open in video player'
                : 'Open'}
            </a>
          </div>
        </>
      ) : (
        <p className="muted">No validated URL is available.</p>
      )}
    </div>
  );
}

export function ImageCard({
  image,
  search,
}: {
  image: ImageRecord;
  search: string;
}) {
  const toast = useContext(Toast);
  const href = `/images/${image.id}?${search}`;
  const c = image.classification;
  return (
    <article className="image-card">
      <Link
        to={href}
        aria-label={`View image ${image.id} from ${nameOf(image.camera)}`}
      >
        <div className="image-wrap">
          <Photo
            src={image.thumbnail_url}
            alt={c?.summary || `Captured image from ${nameOf(image.camera)}`}
          />
          {c?.interesting && (
            <span className="interesting">
              <Star size={12} />
              Interesting
            </span>
          )}
        </div>
        <div className="card-body">
          <div className="row">
            <strong>{nameOf(image.camera)}</strong>
            <span className="muted text-xs">
              <Time value={image.captured_at} />
            </span>
          </div>
          <div className="species">
            <span>
              {c?.contains_wildlife && <Leaf size={14} />}{' '}
              {speciesOf(c) ||
                (c ? 'No species identified' : 'Awaiting classification')}
            </span>
            <span>{percent(c?.confidence)}</span>
          </div>
          <p className="summary">
            {c?.summary ||
              'Classification will appear here when processing is complete.'}
          </p>
          <div className="badges">
            <Badge category="Download" value={image.download_status} />
            <Badge category="Model" value={image.processing_status} />
          </div>
        </div>
      </Link>
      <details className="card-menu">
        <summary aria-label={`Actions for image ${image.id}`}>
          <MoreHorizontal size={17} />
        </summary>
        <div>
          <CopyButton
            text={`${location.origin}${href}`}
            label="Copy detail link"
          />
          <button
            onClick={async () => {
              try {
                const detail = await api<ImageDetail>(`images/${image.id}`);
                if (!detail.nvr.image_url)
                  throw new Error('No validated NVR image URL is available.');
                await navigator.clipboard.writeText(detail.nvr.image_url);
                toast('NVR image URL copied');
              } catch (e) {
                toast(e instanceof Error ? e.message : 'Copy unavailable');
              }
            }}
          >
            Copy NVR image URL
          </button>
        </div>
      </details>
    </article>
  );
}
