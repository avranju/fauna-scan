import { useContext } from 'react';
import { Link, useSearchParams } from 'react-router-dom';
import {
  ArrowDownToLine,
  ArrowRight,
  Camera as CameraIcon,
  CheckCheck,
  Leaf,
  Star,
  TriangleAlert,
} from 'lucide-react';
import { DateTime } from 'luxon';
import {
  nameOf,
  useApi,
  type Activity,
  type Camera,
  type Health,
  type ImagePage,
  type Overview as OverviewData,
} from './api';
import { apiParams, imageHref, scopeParams } from './filters';
import { Badge, Empty, ErrorBox, ImageCard, Loading, Time, Zone } from './ui';

export function PipelineStrip({
  health,
}: {
  health?: Health;
  activity?: Activity;
}) {
  return (
    <div className="pipeline-grid">
      {(['downloader', 'classifier'] as const).map((key) => {
        const pipeline = health?.pipelines?.[key];
        return (
          <div className="pipeline" key={key}>
            <div className="row">
              <h3>{key === 'downloader' ? 'Downloader' : 'Classifier'}</h3>
              <Badge value={pipeline?.state || 'unknown'} />
            </div>
            <div className="pipeline-numbers">
              <strong>
                {pipeline?.active ?? '—'} <small>active</small>
              </strong>
              <span>{pipeline?.queue_depth ?? '—'} queued</span>
            </div>
            <p className="muted">
              Oldest queued:{' '}
              {pipeline?.oldest_queued_at ? (
                <span title={pipeline.oldest_queued_at}>
                  {Math.max(
                    0,
                    Math.floor(
                      (Date.now() - Date.parse(pipeline.oldest_queued_at)) /
                        60000,
                    ),
                  ).toLocaleString()}{' '}
                  min ago
                </span>
              ) : pipeline ? (
                'No queued item'
              ) : (
                'Unknown'
              )}
            </p>
            <p className="muted">
              Last successful {key === 'downloader' ? 'poll' : 'pass'}:{' '}
              <Time value={pipeline?.last_success_at} />
            </p>
            <p className="muted">
              Heartbeat: <Time value={pipeline?.heartbeat_at} />
            </p>
            {pipeline?.last_error && (
              <p className="text-amber-800">{pipeline.last_error}</p>
            )}
            {!pipeline?.heartbeat_at && (
              <p className="text-xs muted">
                No durable heartbeat yet. Start the pipeline to record liveness.
              </p>
            )}
            <p className="text-xs muted">
              Current state across all capture dates and cameras.
            </p>
          </div>
        );
      })}
    </div>
  );
}

export function CameraProgress({ cameras }: { cameras: Camera[] }) {
  const [params] = useSearchParams();
  const overview = useApi<OverviewData>(
    'overview',
    apiParams(scopeParams(params)),
  );
  const selected = params.getAll('camera').flatMap((v) => v.split(','));
  const rows = cameras
    .filter((c) => !selected.length || selected.includes(String(c.id)))
    .sort(
      (a, b) =>
        Number(Boolean(b.last_error)) - Number(Boolean(a.last_error)) ||
        (a.last_completed_window_end || '').localeCompare(
          b.last_completed_window_end || '',
        ) ||
        a.channel_number - b.channel_number,
    );
  return (
    <section className="panel">
      <div className="section-heading">
        <div>
          <h2>Camera progress</h2>
          <p className="muted">
            Current NVR search position; image totals use the selected capture
            range.
          </p>
        </div>
        <CameraIcon size={20} />
      </div>
      {overview.isError && (
        <p className="muted">
          Camera image totals are temporarily unavailable.{' '}
          <button className="text-button" onClick={() => overview.refetch()}>
            Retry camera totals
          </button>
        </p>
      )}
      {rows.length ? (
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Camera</th>
                <th>State</th>
                <th>Search completed through</th>
                <th>Search lag</th>
                <th>Last poll / next search</th>
                <th>Discovered / classified</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((c) => (
                <tr key={c.id}>
                  <td>
                    <Link
                      to={imageHref(params, {
                        camera: String(c.id),
                        download_status: 'all',
                      })}
                    >
                      <strong>{nameOf(c)}</strong>
                    </Link>
                    <p className="muted text-xs">Channel {c.channel_number}</p>
                    {c.last_error && (
                      <p className="text-red-700 text-xs">{c.last_error}</p>
                    )}
                  </td>
                  <td>
                    <Badge
                      value={
                        c.enabled
                          ? c.last_error
                            ? 'degraded'
                            : 'enabled'
                          : 'inactive'
                      }
                    />
                  </td>
                  <td>
                    <Time value={c.last_completed_window_end} />
                  </td>
                  <td>
                    {c.last_completed_window_end
                      ? `${Math.max(0, Math.floor((Date.now() - Date.parse(c.last_completed_window_end)) / 60000)).toLocaleString()} min`
                      : 'Unknown'}
                  </td>
                  <td>
                    <Time value={c.last_poll_at} />
                    <p className="muted text-xs">
                      Next: <Time value={c.next_search_at} />
                    </p>
                  </td>
                  <td>
                    {overview.data
                      ? `${overview.data.camera_counts?.find((count) => count.camera_id === c.id)?.discovered ?? 0} / ${overview.data.camera_counts?.find((count) => count.camera_id === c.id)?.classified ?? 0}`
                      : '—'}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <Empty title="No cameras discovered">
          <p>Cameras appear after the first successful NVR discovery.</p>
        </Empty>
      )}
    </section>
  );
}

export function Overview({
  cameras,
  health,
}: {
  cameras: Camera[];
  health?: Health;
}) {
  const [params] = useSearchParams();
  const scope = apiParams(scopeParams(params));
  const overview = useApi<OverviewData>('overview', scope);
  const activity = useApi<Activity>('activity', scope);
  const recent = useApi<ImagePage>(
    'images',
    `${scope}&interesting=true&download_status=downloaded&limit=8`,
  );
  const zone = useContext(Zone);
  const metrics = [
    ['discovered', 'Discovered', CameraIcon, { download_status: 'all' }],
    [
      'downloaded',
      'Downloaded',
      ArrowDownToLine,
      { download_status: 'downloaded' },
    ],
    [
      'classified',
      'Classified',
      CheckCheck,
      { classified: 'true', download_status: 'all' },
    ],
    [
      'wildlife',
      'Wildlife found',
      Leaf,
      { contains_wildlife: 'true', download_status: 'all' },
    ],
    [
      'interesting',
      'Interesting',
      Star,
      { interesting: 'true', download_status: 'all' },
    ],
    [
      'retryable_failures',
      'Retryable failures',
      TriangleAlert,
      { failure: 'retryable', download_status: 'all' },
    ],
    [
      'permanent_failures',
      'Permanent failures',
      TriangleAlert,
      { failure: 'permanent', download_status: 'all' },
    ],
  ] as const;
  const buckets = overview.data?.buckets || [];
  const maximum = Math.max(
    1,
    ...buckets.map((b) => b.discovered + b.downloaded + b.classified),
  );
  return (
    <>
      <div className="page-heading">
        <div>
          <p className="eyebrow">A WINDOW INTO THE WILD</p>
          <h1>Your wildlife, at a glance.</h1>
          <p className="muted">Discover what’s been passing through.</p>
        </div>
        <Link className="button primary" to={imageHref(params)}>
          Explore images <ArrowRight size={16} />
        </Link>
      </div>
      {overview.isError && (
        <ErrorBox error={overview.error} retry={() => overview.refetch()} />
      )}
      <div className="metrics">
        {metrics.map(([key, title, Icon, filters]) => (
          <Link
            className={`metric ${key === 'wildlife' || key === 'interesting' ? 'metric-accent' : ''}`}
            key={key}
            to={imageHref(params, filters)}
          >
            <div className="row">
              <span>{title}</span>
              <Icon size={17} />
            </div>
            <strong>
              {overview.isPending
                ? '—'
                : (overview.data?.counts[key]?.toLocaleString() ?? '—')}
            </strong>
            <small>
              Selected capture range · {params.getAll('camera').length || 'all'}{' '}
              cameras
            </small>
          </Link>
        ))}
      </div>
      <div className="section-heading">
        <h2>Live pipeline</h2>
        <Link to={`/activity?${scopeParams(params)}`}>
          View scan activity <ArrowRight size={14} />
        </Link>
      </div>
      <PipelineStrip health={health} activity={activity.data} />
      <section className="panel chart-panel">
        <div className="section-heading">
          <div>
            <h2>Capture activity</h2>
            <p className="muted">
              Images grouped by capture time. Current completion state, not an
              event log.
            </p>
          </div>
          <span className="chart-legend">
            Discovered · Downloaded · Classified
          </span>
        </div>
        {buckets.length ? (
          <>
            <div className="histogram" aria-label="Capture activity histogram">
              {buckets.map((b) => (
                <Link
                  key={b.start_at}
                  to={imageHref(params, {
                    from: b.start_at,
                    to: b.end_at,
                    range: 'custom',
                    download_status: 'all',
                  })}
                  aria-label={`${DateTime.fromISO(b.start_at).setZone(zone).toFormat('dd LLL HH:mm')}: ${b.discovered} discovered, ${b.downloaded} downloaded, ${b.classified} classified`}
                >
                  <svg
                    viewBox="0 0 20 100"
                    preserveAspectRatio="none"
                    aria-hidden="true"
                  >
                    <rect
                      x="2"
                      width="16"
                      y={100 - (b.discovered / maximum) * 100}
                      height={(b.discovered / maximum) * 100}
                      fill="#a7bdb2"
                    />
                    <rect
                      x="2"
                      width="16"
                      y={100 - ((b.discovered + b.downloaded) / maximum) * 100}
                      height={(b.downloaded / maximum) * 100}
                      fill="#659582"
                    />
                    <rect
                      x="2"
                      width="16"
                      y={
                        100 -
                        ((b.discovered + b.downloaded + b.classified) /
                          maximum) *
                          100
                      }
                      height={(b.classified / maximum) * 100}
                      fill="#214f3e"
                    />
                  </svg>
                </Link>
              ))}
            </div>
            <div className="row muted text-xs">
              <Time value={buckets[0].start_at} />
              <Time value={buckets[buckets.length - 1].end_at} />
            </div>
            <details>
              <summary>View chart data table</summary>
              <div className="table-wrap">
                <table>
                  <thead>
                    <tr>
                      <th>Capture interval start</th>
                      <th>Discovered</th>
                      <th>Downloaded</th>
                      <th>Classified</th>
                    </tr>
                  </thead>
                  <tbody>
                    {buckets.map((b) => (
                      <tr key={b.start_at}>
                        <td>
                          <Link
                            to={imageHref(params, {
                              from: b.start_at,
                              to: b.end_at,
                              range: 'custom',
                              download_status: 'all',
                            })}
                          >
                            <Time value={b.start_at} />
                          </Link>
                        </td>
                        <td>{b.discovered}</td>
                        <td>{b.downloaded}</td>
                        <td>{b.classified}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </details>
          </>
        ) : (
          <Empty title="No capture activity in this range" />
        )}
      </section>
      <div className="section-heading">
        <div>
          <h2>Worth a closer look</h2>
          <p className="muted">
            The latest interesting captures in this range.
          </p>
        </div>
        <Link to={imageHref(params, { interesting: 'true' })}>
          View all <ArrowRight size={14} />
        </Link>
      </div>
      {recent.isError ? (
        <ErrorBox error={recent.error} retry={() => recent.refetch()} />
      ) : recent.isPending ? (
        <Loading />
      ) : recent.data?.data.length ? (
        <div className="gallery">
          {recent.data.data.map((i) => (
            <ImageCard
              key={i.id}
              image={i}
              search={`${scopeParams(params)}&interesting=true&download_status=downloaded`}
            />
          ))}
        </div>
      ) : (
        <Empty title="No interesting images in this range">
          <Link className="button" to={imageHref(params)}>
            Show all downloaded images
          </Link>
        </Empty>
      )}
      <CameraProgress cameras={cameras} />
      {overview.data && (
        <p className="as-of">
          As of <Time value={overview.data.generated_at} />
        </p>
      )}
    </>
  );
}
