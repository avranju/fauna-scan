import { useContext, useEffect, useRef, useState } from 'react';
import { Link, useParams, useSearchParams } from 'react-router-dom';
import { useMutation } from '@tanstack/react-query';
import { imageParams } from './filters';
import { TransformComponent, TransformWrapper } from 'react-zoom-pan-pinch';
import {
  ArrowLeft,
  ArrowRight,
  Maximize,
  Minus,
  Plus,
  RotateCcw,
  Star,
} from 'lucide-react';
import {
  api,
  nameOf,
  percent,
  useApi,
  type Config,
  type Clip,
  type ImageDetail,
  type ImagePage,
  type Lifecycle,
  type Recording,
} from './api';
import { Badge, CopyButton, ErrorBox, Loading, SafeURL, Time } from './ui';

function LifecyclePanel({ label, state }: { label: string; state: Lifecycle }) {
  return (
    <section>
      <h3>{label}</h3>
      <Badge category={label} value={state.status} />
      <dl className="facts">
        <dt>Attempts</dt>
        <dd>{state.attempts}</dd>
        <dt>Started</dt>
        <dd>
          <Time value={state.started_at} />
        </dd>
        <dt>Completed</dt>
        <dd>
          <Time value={state.downloaded_at || state.completed_at} />
        </dd>
        <dt>Next attempt</dt>
        <dd>
          <Time value={state.next_attempt_at} />
        </dd>
        <dt>Lease expires</dt>
        <dd>
          <Time value={state.lease_until} />
        </dd>
      </dl>
      {state.last_error && <div className="error-box">{state.last_error}</div>}
    </section>
  );
}

function Uncertainties({ value }: { value: unknown }) {
  const items = Array.isArray(value)
    ? value.filter(
        (item): item is string =>
          typeof item === 'string' && item.trim().length > 0,
      )
    : typeof value === 'string' && value.trim()
      ? [value]
      : [];
  if (items.length <= 1)
    return <p>{items[0] || 'No uncertainties recorded.'}</p>;
  return (
    <ul className="uncertainties">
      {items.map((item, i) => (
        <li key={i}>{item}</li>
      ))}
    </ul>
  );
}

function RecordingPanel({ id, config }: { id: string; config: Config }) {
  const [pre, setPre] = useState(config.default_clip_pre_roll_seconds);
  const [post, setPost] = useState(config.default_clip_post_roll_seconds);
  const [playerFailed, setPlayerFailed] = useState(false);
  const clip = useMutation({
    mutationFn: () =>
      api<Clip>(
        `images/${id}/clip?pre_roll_seconds=${pre}&post_roll_seconds=${post}`,
        undefined,
        'POST',
      ),
    onSuccess: () => setPlayerFailed(false),
  });
  const recording = useMutation({
    mutationFn: () =>
      api<Recording>(
        `images/${id}/recording?pre_roll_seconds=${pre}&post_roll_seconds=${post}`,
      ),
    onSuccess: (data) => {
      if (data.status === 'found' && data.capabilities?.browser_playback)
        clip.mutate();
    },
  });
  const invalid =
    pre < 0 ||
    post < 0 ||
    !Number.isInteger(pre) ||
    !Number.isInteger(post) ||
    pre + post < 1 ||
    pre + post > config.maximum_clip_duration_seconds;
  return (
    <section className="recording">
      <h3>NVR video recording</h3>
      <p className="muted">
        Search the camera’s primary video track around this capture. The NVR is
        contacted only when you request a recording.
      </p>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (!invalid) {
            clip.reset();
            setPlayerFailed(false);
            recording.mutate();
          }
        }}
      >
        <div className="actions">
          <label>
            Seconds before
            <input
              type="number"
              disabled={recording.isPending || clip.isPending}
              min="0"
              max={config.maximum_clip_duration_seconds}
              value={pre}
              onChange={(e) => {
                setPre(Number(e.target.value));
                recording.reset();
                clip.reset();
                setPlayerFailed(false);
              }}
            />
          </label>
          <label>
            Seconds after
            <input
              type="number"
              disabled={recording.isPending || clip.isPending}
              min="0"
              max={config.maximum_clip_duration_seconds}
              value={post}
              onChange={(e) => {
                setPost(Number(e.target.value));
                recording.reset();
                clip.reset();
                setPlayerFailed(false);
              }}
            />
          </label>
          <button
            className="primary self-end"
            disabled={
              invalid ||
              recording.isPending ||
              clip.isPending ||
              !config.capabilities.nvr_recording_lookup
            }
          >
            {recording.isPending ? 'Searching NVR…' : 'Find recording'}
          </button>
        </div>
        <p className="muted text-xs">
          Maximum interval: {config.maximum_clip_duration_seconds} seconds.
        </p>
        {invalid && (
          <p role="alert" className="text-red-700">
            Choose a positive interval within the configured maximum.
          </p>
        )}
      </form>
      {recording.isError && (
        <ErrorBox error={recording.error} retry={() => recording.mutate()} />
      )}
      {recording.data?.status === 'not_found' && (
        <p role="status" className="notice">
          No recording covers this capture time. Adjust the interval and try
          again.
        </p>
      )}
      {recording.data?.status === 'found' && (
        <>
          <SafeURL
            label="NVR video playback URI"
            url={recording.data.nvr_playback_uri}
          />
          <dl className="facts">
            <dt>Requested clip</dt>
            <dd>
              <Time value={recording.data.requested_start_at} /> –{' '}
              <Time value={recording.data.requested_end_at} />
            </dd>
            <dt>NVR recording</dt>
            <dd>
              <Time value={recording.data.recording_start_at} /> –{' '}
              <Time value={recording.data.recording_end_at} />
            </dd>
          </dl>
        </>
      )}
      {clip.isPending && <p role="status">Preparing video clip…</p>}
      {clip.isError && (
        <ErrorBox error={clip.error} retry={() => clip.mutate()} />
      )}
      {clip.data && (
        <>
          <video
            key={clip.data.playback_url}
            controls
            playsInline
            preload="metadata"
            aria-label="NVR recording clip"
            src={clip.data.playback_url}
            onError={() => setPlayerFailed(true)}
          />
          {playerFailed && (
            <p role="alert">
              Video unavailable or expired. Prepare the clip again.
            </p>
          )}
          <p className="muted text-xs">
            Clips are available for 15 minutes. Prepare the clip again if it
            expires.
          </p>
        </>
      )}
      <div className="actions">
        <button
          disabled={
            recording.data?.status !== 'found' ||
            !config.capabilities.browser_clip_playback ||
            clip.isPending
          }
          onClick={() => clip.mutate()}
        >
          {clip.data ? 'Prepare clip again' : 'View clip'}
        </button>
        {clip.data ? (
          <a className="button" href={clip.data.download_url} download>
            Download video
          </a>
        ) : (
          <button disabled>Download video</button>
        )}
      </div>
      {!config.capabilities.browser_clip_playback && (
        <p className="muted text-xs">
          Browser playback and download require FFmpeg on the server. The NVR
          URL can be opened in an external player.
        </p>
      )}
    </section>
  );
}

export function Detail() {
  const { imageId = '' } = useParams();
  const [params] = useSearchParams();
  const image = useApi<ImageDetail>(`images/${imageId}`);
  const config = useApi<Config>('config');
  const [tab, setTab] = useState('classification');
  const [selected, setSelected] = useState<number | null>(null);
  const [failed, setFailed] = useState(false);
  const [dimensions, setDimensions] = useState('');
  const [boxes, setBoxes] = useState(true);
  const viewport = useRef<HTMLDivElement>(null);
  const natural = useRef({ width: 0, height: 0 });
  useEffect(() => {
    setSelected(null);
    setFailed(false);
    setDimensions('');
    setBoxes(true);
  }, [imageId]);
  const detail = image.data;
  const classification =
    detail?.classifications.find((c) => c.id === selected) ||
    detail?.classifications[0];
  const neighbours = useApi<{ previous: number | null; next: number | null }>(
    `images/${imageId}/neighbors`,
    params.size ? imageParams(params) : '',
  );
  const previous = neighbours.data?.previous;
  const next = neighbours.data?.next;
  if (image.isPending) return <Loading />;
  if (image.isError || !detail)
    return <ErrorBox error={image.error} retry={() => image.refetch()} />;
  const content =
    detail.content_url &&
    `${detail.content_url}${boxes && classification?.id === detail.classifications[0]?.id ? '?draw-bounding-box=true' : ''}`;
  return (
    <>
      <div className="detail-navigation">
        <Link to={`/images?${params}`}>
          <ArrowLeft size={15} />
          Back to images
        </Link>
        <div className="actions">
          <CopyButton text={location.href} label="Copy detail link" />
          {previous ? (
            <Link className="button" to={`/images/${previous}?${params}`}>
              <ArrowLeft size={14} />
              Previous
            </Link>
          ) : (
            <button disabled>Previous</button>
          )}
          {next ? (
            <Link className="button" to={`/images/${next}?${params}`}>
              Next
              <ArrowRight size={14} />
            </Link>
          ) : (
            <button disabled>Next</button>
          )}
        </div>
      </div>
      <div className="detail-grid">
        <section className="viewer panel" ref={viewport}>
          {content && !failed ? (
            <TransformWrapper
              key={imageId}
              initialScale={1}
              minScale={0.5}
              maxScale={12}
              centerOnInit
              wheel={{ disabled: true }}
            >
              {({
                zoomIn,
                zoomOut,
                resetTransform,
                setTransform,
                instance,
              }) => (
                <>
                  <div className="viewer-controls">
                    <button aria-label="Zoom in" onClick={() => zoomIn()}>
                      <Plus size={17} />
                    </button>
                    <button aria-label="Zoom out" onClick={() => zoomOut()}>
                      <Minus size={17} />
                    </button>
                    <button
                      onClick={() => {
                        const width = viewport.current?.clientWidth || 1;
                        setTransform(
                          0,
                          0,
                          Math.max(0.5, natural.current.width / width),
                        );
                      }}
                    >
                      100%
                    </button>
                    <button onClick={() => resetTransform()}>
                      <Maximize size={15} />
                      Fit
                    </button>
                    <button
                      aria-label="Reset image view"
                      onClick={() => resetTransform()}
                    >
                      <RotateCcw size={15} />
                    </button>
                  </div>
                  <div
                    className="viewer-stage"
                    tabIndex={0}
                    role="group"
                    aria-label="Image viewer. Use arrow keys to pan."
                    onKeyDown={(event) => {
                      const offsets: Record<string, [number, number]> = {
                        ArrowLeft: [40, 0],
                        ArrowRight: [-40, 0],
                        ArrowUp: [0, 40],
                        ArrowDown: [0, -40],
                      };
                      const offset = offsets[event.key];
                      if (offset) {
                        event.preventDefault();
                        setTransform(
                          instance.transformState.positionX + offset[0],
                          instance.transformState.positionY + offset[1],
                          instance.transformState.scale,
                        );
                      }
                    }}
                  >
                    <TransformComponent
                      wrapperClass="viewer-transform"
                      contentClass="viewer-content"
                    >
                      <img
                        src={content}
                        alt={
                          classification?.summary ||
                          `Captured image from ${nameOf(detail.camera)}`
                        }
                        onError={() => setFailed(true)}
                        onLoad={(e) => {
                          const img = e.currentTarget;
                          natural.current = {
                            width: img.naturalWidth,
                            height: img.naturalHeight,
                          };
                          setDimensions(
                            `${img.naturalWidth} × ${img.naturalHeight} pixels`,
                          );
                        }}
                      />
                    </TransformComponent>
                  </div>
                </>
              )}
            </TransformWrapper>
          ) : (
            <div className="viewer-unavailable">
              <h2>
                {detail.download.status === 'downloaded'
                  ? 'Local image unavailable'
                  : 'Image not downloaded'}
              </h2>
              <p>Classification, lifecycle, and NVR links remain available.</p>
              {content && (
                <button onClick={() => setFailed(false)}>Retry image</button>
              )}
            </div>
          )}
          <div className="viewer-footer">
            <span>{dimensions || `Image #${detail.id}`}</span>
            <span>Use zoom controls; drag or arrow keys to pan.</span>
          </div>
        </section>
        <aside className="panel detail-summary">
          <p className="eyebrow">CAMERA {detail.camera.channel}</p>
          <h1>{nameOf(detail.camera)}</h1>
          <p className="muted">
            <Time value={detail.captured_at} />
          </p>
          <div className="badges">
            <Badge category="Download" value={detail.download.status} />
            <Badge category="Model" value={detail.processing.status} />
          </div>
          {classification ? (
            <>
              <div className="result-title">
                {classification.interesting && <Star size={20} />}
                <h2>
                  {classification.contains_wildlife
                    ? 'Wildlife spotted'
                    : 'No wildlife detected'}
                </h2>
                <strong>{percent(classification.confidence)}</strong>
              </div>
              <p>{classification.summary || 'No summary was supplied.'}</p>
              <div className="species-list">
                {classification.species.map((s, i) => (
                  <div key={i}>
                    <span>{typeof s === 'string' ? s : s.name}</span>
                    <strong>
                      {typeof s === 'string' ? '—' : percent(s.confidence)}
                    </strong>
                  </div>
                ))}
              </div>
              <p className="muted text-xs">
                {classification.model} · {classification.prompt_version}
                <br />
                Completed <Time value={classification.request_completed_at} />
              </p>
            </>
          ) : (
            <>
              <h2>
                {detail.processing.status === 'new'
                  ? 'Waiting to be classified.'
                  : `Model: ${detail.processing.status.replaceAll('_', ' ')}`}
              </h2>
              <p className="muted">
                {detail.processing.last_error ||
                  'The latest processing information is available below.'}
              </p>
              <p>
                Next attempt: <Time value={detail.processing.next_attempt_at} />
              </p>
            </>
          )}
          <button className="primary" onClick={() => setTab('nvr')}>
            NVR image & video links
            <ArrowRight size={15} />
          </button>
        </aside>
      </div>
      <section className="panel detail-tabs">
        <div className="tabs" role="tablist" aria-label="Image information">
          {[
            ['classification', 'Classification'],
            ['processing', 'Processing'],
            ['nvr', 'NVR & files'],
            ['diagnostics', 'Diagnostics'],
          ].map(([key, title]) => (
            <button
              key={key}
              id={`tab-${key}`}
              role="tab"
              aria-controls={`panel-${key}`}
              aria-selected={tab === key}
              tabIndex={tab === key ? 0 : -1}
              onKeyDown={(e) => {
                const tabs = [
                  'classification',
                  'processing',
                  'nvr',
                  'diagnostics',
                ];
                if (e.key === 'ArrowRight' || e.key === 'ArrowLeft') {
                  e.preventDefault();
                  const newTab =
                    tabs[
                      (tabs.indexOf(tab) + (e.key === 'ArrowRight' ? 1 : 3)) % 4
                    ];
                  setTab(newTab);
                  document.getElementById(`tab-${newTab}`)?.focus();
                }
              }}
              onClick={() => setTab(key)}
            >
              {title}
            </button>
          ))}
        </div>
        <div
          className="tab-content"
          role="tabpanel"
          id={`panel-${tab}`}
          aria-labelledby={`tab-${tab}`}
          tabIndex={0}
        >
          {tab === 'classification' &&
            (classification ? (
              <>
                <label>
                  Classification result
                  <select
                    value={classification.id}
                    onChange={(e) => {
                      setSelected(Number(e.target.value));
                    }}
                  >
                    {detail.classifications.map((c, i) => (
                      <option key={c.id} value={c.id}>
                        {i === 0 ? 'Latest · ' : ''}
                        {c.model} · {c.prompt_version} ·{' '}
                        {c.request_completed_at}
                      </option>
                    ))}
                  </select>
                </label>
                <dl className="facts">
                  <dt>Model / prompt</dt>
                  <dd>
                    {classification.model} · {classification.prompt_version}
                  </dd>
                  <dt>Wildlife / interesting</dt>
                  <dd>
                    {classification.contains_wildlife ? 'Yes' : 'No'} /{' '}
                    {classification.interesting ? 'Yes' : 'No'}
                  </dd>
                  <dt>Overall confidence</dt>
                  <dd>{percent(classification.confidence)}</dd>
                  <dt>Request started</dt>
                  <dd>
                    <Time value={classification.request_started_at} />
                  </dd>
                  <dt>Request completed</dt>
                  <dd>
                    <Time value={classification.request_completed_at} />
                  </dd>
                  <dt>Request duration</dt>
                  <dd>
                    {classification.request_started_at &&
                    classification.request_completed_at
                      ? `${Math.max(0, (Date.parse(classification.request_completed_at) - Date.parse(classification.request_started_at)) / 1000)} seconds`
                      : 'Not recorded'}
                  </dd>
                </dl>
                <h3>Model summary</h3>
                <p className="full-summary">
                  {classification.summary || 'No summary provided.'}
                </p>
                <h3>Animal bounding boxes</h3>
                {classification.bounding_boxes == null ? (
                  <p className="muted">
                    Bounding boxes were not recorded for this classification.
                  </p>
                ) : classification.bounding_boxes.length === 0 ? (
                  <p className="muted">No animals were located.</p>
                ) : (
                  <>
                    <label className="check-label">
                      <input
                        type="checkbox"
                        checked={boxes}
                        disabled={
                          classification.id !== detail.classifications[0]?.id
                        }
                        onChange={(e) => setBoxes(e.target.checked)}
                      />
                      Draw latest classification’s boxes on image
                    </label>
                    <div className="table-wrap">
                      <table>
                        <thead>
                          <tr>
                            <th>Animal</th>
                            <th>Left</th>
                            <th>Top</th>
                            <th>Right</th>
                            <th>Bottom</th>
                          </tr>
                        </thead>
                        <tbody>
                          {classification.bounding_boxes.map((b, i) => (
                            <tr key={i}>
                              <td>{i + 1}</td>
                              <td>{percent(b.x_min)}</td>
                              <td>{percent(b.y_min)}</td>
                              <td>{percent(b.x_max)}</td>
                              <td>{percent(b.y_max)}</td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    </div>
                  </>
                )}
                {classification.structured?.uncertainties != null && (
                  <>
                    <h3>Model uncertainties</h3>
                    <Uncertainties
                      value={classification.structured.uncertainties}
                    />
                  </>
                )}
              </>
            ) : (
              <p>
                Waiting for a completed classification. Check Processing for the
                latest state.
              </p>
            ))}
          {tab === 'processing' && (
            <>
              <h2>Current lifecycle</h2>
              <p className="muted">
                Latest durable state; this is not a complete attempt history.
              </p>
              <p>
                Discovered <Time value={detail.discovered_at} />
              </p>
              <div className="grid gap-8 md:grid-cols-2">
                <LifecyclePanel label="Download" state={detail.download} />
                <LifecyclePanel label="Model" state={detail.processing} />
              </div>
            </>
          )}
          {tab === 'nvr' && (
            <>
              <SafeURL label="NVR still-image URL" url={detail.nvr.image_url} />
              {detail.nvr.reported_image_url !== detail.nvr.image_url &&
                detail.nvr.reported_image_url && (
                  <SafeURL
                    label="NVR-reported still URL"
                    url={detail.nvr.reported_image_url}
                  />
                )}
              <h3>Local file</h3>
              <p>
                {detail.file?.name || 'Local JPEG'} ·{' '}
                {dimensions || 'Dimensions unavailable'}
                {detail.file?.size_bytes
                  ? ` · ${(detail.file.size_bytes / 1024).toFixed(1)} KiB`
                  : ''}
              </p>
              {detail.content_url ? (
                <a
                  className="button"
                  href={detail.content_url}
                  target="_blank"
                  rel="noopener noreferrer"
                >
                  Open local JPEG
                </a>
              ) : (
                <p className="muted">A local file is not available.</p>
              )}
              {config.data ? (
                <RecordingPanel
                  key={imageId}
                  id={imageId}
                  config={config.data}
                />
              ) : config.isError ? (
                <ErrorBox error={config.error} retry={() => config.refetch()} />
              ) : (
                <Loading />
              )}
            </>
          )}
          {tab === 'diagnostics' && (
            <>
              <h2>Safe diagnostics</h2>
              <dl className="facts">
                <dt>Image ID</dt>
                <dd>{detail.id}</dd>
                <dt>Image key</dt>
                <dd>
                  <code>{detail.image_key}</code>
                </dd>
                <dt>Camera ID</dt>
                <dd>{detail.camera.id}</dd>
                <dt>Primary video track</dt>
                <dd>{detail.camera.primary_track_id}</dd>
                <dt>Picture track</dt>
                <dd>{detail.camera.picture_track_id}</dd>
              </dl>
              <p className="muted">
                Raw model responses are not exposed in the browser.
              </p>
            </>
          )}
        </div>
      </section>
    </>
  );
}
