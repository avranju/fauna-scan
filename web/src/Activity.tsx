import { Link, useSearchParams } from 'react-router-dom';
import {
  useApi,
  type Activity as ActivityData,
  type Camera,
  type Health,
  type ImagePage,
} from './api';
import { apiParams, imageHref, scopeParams } from './filters';
import { Badge, Empty, ErrorBox, Loading, Time } from './ui';
import { CameraProgress, PipelineStrip } from './Overview';

export function Activity({
  cameras,
  health,
}: {
  cameras: Camera[];
  health?: Health;
}) {
  const [params] = useSearchParams();
  const scope = apiParams(scopeParams(params));
  const activity = useApi<ActivityData>('activity', scope);
  const failures = useApi<ImagePage>(
    'images',
    `${scope}&failure=permanent&limit=12`,
  );
  const completions = useApi<ImagePage>(
    'images',
    `${scope}&classified=true&sort=classified_desc&limit=12`,
  );
  return (
    <>
      <div className="page-heading">
        <div>
          <p className="eyebrow">BEHIND THE SCENES</p>
          <h1>Scan activity</h1>
          <p className="muted">
            Follow downloads, model work, and camera search progress.
          </p>
        </div>
        <Badge value="read only" />
      </div>
      <PipelineStrip health={health} activity={activity.data} />
      {activity.isError && (
        <ErrorBox error={activity.error} retry={() => activity.refetch()} />
      )}
      <section className="panel">
        <div className="section-heading">
          <h2>Current work</h2>
          <span className="muted">Durable claims · up to 100</span>
        </div>
        {activity.isPending ? (
          <Loading />
        ) : activity.data?.active.length ? (
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Image / camera</th>
                  <th>Operation</th>
                  <th>Attempt</th>
                  <th>Started / elapsed</th>
                  <th>Lease expires</th>
                </tr>
              </thead>
              <tbody>
                {activity.data.active.flatMap((a) =>
                  [
                    a.download_status === 'downloading'
                      ? {
                          type: 'Download',
                          attempts: a.download_attempts,
                          lease: a.download_lease_until,
                          start: null,
                        }
                      : null,
                    a.processing_status === 'processing'
                      ? {
                          type: 'Classification',
                          attempts: a.processing_attempts,
                          lease: a.processing_lease_until,
                          start: a.processing_started_at,
                        }
                      : null,
                  ]
                    .filter((p) => p !== null)
                    .map((p) => (
                      <tr key={`${a.id}-${p.type}`}>
                        <td>
                          <Link to={`/images/${a.id}?${params}`}>
                            {a.camera_name || `Camera ${a.channel}`} · #{a.id}
                          </Link>
                          <p className="muted text-xs">
                            <Time value={a.captured_at} />
                          </p>
                        </td>
                        <td>
                          {p.type}
                          <br />
                          <Badge
                            value={
                              !p.lease || Date.parse(p.lease) <= Date.now()
                                ? 'stale claim'
                                : (
                                      p.type === 'Download'
                                        ? health?.pipelines?.downloader
                                            .heartbeat_fresh
                                        : health?.pipelines?.classifier
                                            .heartbeat_fresh
                                    )
                                  ? 'active'
                                  : 'stale claim'
                            }
                          />
                        </td>
                        <td>{p.attempts}</td>
                        <td>
                          <Time value={p.start} />
                          {p.start && (
                            <p className="muted text-xs">
                              {Math.max(
                                0,
                                Math.floor(
                                  (Date.now() - Date.parse(p.start)) / 1000,
                                ),
                              )}{' '}
                              seconds elapsed
                            </p>
                          )}
                        </td>
                        <td>
                          <Time value={p.lease} />
                        </td>
                      </tr>
                    )),
                )}
              </tbody>
            </table>
          </div>
        ) : (
          <Empty title="No current work in this range">
            <p>
              Queue state and last successful polls are shown separately; an
              empty queue does not establish pipeline health.
            </p>
          </Empty>
        )}
      </section>
      <section className="panel">
        <div className="section-heading">
          <h2>Queues & current states</h2>
          <span className="muted">Selected capture interval</span>
        </div>
        <div className="queue-grid">
          {(activity.data?.counts || []).map((c) => (
            <Link
              key={`${c.category}-${c.status}`}
              to={imageHref(params, {
                download_status: c.category === 'download' ? c.status : 'all',
                ...(c.category === 'processing'
                  ? { processing_status: c.status }
                  : {}),
              })}
            >
              <Badge
                category={c.category === 'download' ? 'Download' : 'Model'}
                value={c.status}
              />
              <strong>{c.count.toLocaleString()}</strong>
            </Link>
          ))}
        </div>
      </section>
      <div className="grid gap-6 lg:grid-cols-2">
        {[
          { name: 'Recent classifications', result: completions },
          { name: 'Current permanent failures', result: failures },
        ].map(({ name, result }) => (
          <section className="panel" key={name}>
            <div className="section-heading">
              <h2>{name}</h2>
            </div>
            {result.isError ? (
              <ErrorBox error={result.error} retry={() => result.refetch()} />
            ) : !result.data?.data.length ? (
              <p className="muted p-5">No matching items in this range.</p>
            ) : (
              <div className="feed">
                {result.data.data.map((i) => (
                  <Link key={i.id} to={`/images/${i.id}?${params}`}>
                    <div>
                      <strong>
                        {i.camera.name || `Camera ${i.camera.channel}`} · #
                        {i.id}
                      </strong>
                      <p className="muted">
                        {i.classification?.summary ||
                          'Open image detail for the latest error and attempt information.'}
                      </p>
                    </div>
                    <Badge category="Model" value={i.processing_status} />
                    <Time
                      value={i.classification?.completed_at || i.captured_at}
                    />
                  </Link>
                ))}
              </div>
            )}
          </section>
        ))}
      </div>
      <CameraProgress cameras={cameras} />
      {activity.data && (
        <p className="as-of">
          As of <Time value={activity.data.generated_at} />
        </p>
      )}
    </>
  );
}
