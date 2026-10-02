import { useState } from 'react';
import { Link, useSearchParams } from 'react-router-dom';
import { useInfiniteQuery } from '@tanstack/react-query';
import { Grid2X2, List, SlidersHorizontal } from 'lucide-react';
import { api, nameOf, percent, speciesOf, useApi, type ImagePage } from './api';
import { defaultRange, imageParams } from './filters';
import { Badge, Empty, ErrorBox, ImageCard, Loading, Photo, Time } from './ui';

export function Images() {
  const [params, setParams] = useSearchParams();
  const [filters, setFilters] = useState(false);
  const table = params.get('view') === 'table';
  const query = imageParams(params);
  const facets = useApi<{
    download_status: Record<string, number>;
    processing_status: Record<string, number>;
  }>('images/facets', query, filters);
  const images = useInfiniteQuery({
    queryKey: ['images', query, table],
    queryFn: ({ pageParam, signal }) =>
      api<ImagePage>(
        `images?${query}&limit=${table ? 100 : 60}${pageParam ? `&cursor=${encodeURIComponent(pageParam)}` : ''}`,
        signal,
      ),
    initialPageParam: '',
    getNextPageParam: (last) => last.page.next_cursor || undefined,
  });
  const rows = images.data?.pages.flatMap((p) => p.data) || [];

  const update = (key: string, value: string) => {
    const q = new URLSearchParams(params);
    q.delete('cursor');
    if (value) q.set(key, value);
    else q.delete(key);
    setParams(q);
  };

  const count = [
    'download_status',
    'processing_status',
    'classified',
    'contains_wildlife',
    'interesting',
    'species',
    'confidence_min',
    'model',
    'prompt_version',
    'q',
    'time_field',
    'failure',
    'local_file',
  ].filter((k) => params.has(k)).length;
  const [hidden, setHidden] = useState<string[]>(() => {
    try {
      const stored: unknown = JSON.parse(
        localStorage.getItem('fauna-columns') || '[]',
      );
      return Array.isArray(stored)
        ? stored.filter((key): key is string => typeof key === 'string')
        : [];
    } catch {
      return [];
    }
  });

  function toggleColumn(key: string) {
    const next = hidden.includes(key)
      ? hidden.filter((k) => k !== key)
      : [...hidden, key];
    setHidden(next);
    try {
      localStorage.setItem('fauna-columns', JSON.stringify(next));
    } catch {
      /* optional preference */
    }
  }

  return (
    <>
      <div className="page-heading">
        <div>
          <p className="eyebrow">YOUR FIELD ARCHIVE</p>
          <h1>Images</h1>
          <p className="muted">Every capture has a story. Find yours.</p>
        </div>
        <span className="subtle-label">
          {rows.length.toLocaleString()}
          {images.hasNextPage ? '+' : ''} images
          {images.hasNextPage ? ' loaded' : ''}
        </span>
      </div>
      <div className="toolbar">
        <div className="segmented">
          <button
            aria-label="Gallery view"
            aria-pressed={!table}
            onClick={() => update('view', 'gallery')}
          >
            <Grid2X2 size={17} />
          </button>
          <button
            aria-label="Table view"
            aria-pressed={table}
            onClick={() => update('view', 'table')}
          >
            <List size={17} />
          </button>
        </div>
        <label className="inline-label">
          Sort
          <select
            value={params.get('sort') || 'captured_desc'}
            onChange={(e) => update('sort', e.target.value)}
          >
            <option value="captured_desc">Captured: newest</option>
            <option value="captured_asc">Captured: oldest</option>
            <option value="confidence_desc">Confidence: highest</option>
            <option value="classified_desc">Classified: newest</option>
            <option value="camera_asc">Camera, then newest</option>
          </select>
        </label>
        <button aria-expanded={filters} onClick={() => setFilters(!filters)}>
          <SlidersHorizontal size={16} />
          Image filters{count > 0 ? ` (${count})` : ''}
        </button>
        {table && (
          <details>
            <summary>Columns</summary>
            <div className="columns-menu">
              {['species', 'confidence', 'model', 'completed'].map((k) => (
                <label className="check-label" key={k}>
                  <input
                    type="checkbox"
                    checked={!hidden.includes(k)}
                    onChange={() => toggleColumn(k)}
                  />
                  {k}
                </label>
              ))}
            </div>
          </details>
        )}
      </div>
      {count > 0 && (
        <div className="chips">
          {[
            'processing_status',
            'classified',
            'contains_wildlife',
            'interesting',
            'species',
            'confidence_min',
            'model',
            'prompt_version',
            'q',
            'time_field',
            'failure',
            'local_file',
          ]
            .filter((k) => params.has(k))
            .map((k) => (
              <button key={k} onClick={() => update(k, '')}>
                {k.replaceAll('_', ' ')}: {params.get(k)} ×
              </button>
            ))}
        </div>
      )}
      {filters && (
        <form
          className="panel image-filters"
          onSubmit={(e) => {
            e.preventDefault();
            const q = new URLSearchParams(params);
            const f = new FormData(e.currentTarget);
            f.forEach((v, k) => {
              q.delete(k);
              if (v) q.set(k, String(v));
            });
            q.delete('cursor');
            setParams(q);
          }}
          key={params.toString()}
        >
          <label>
            Download state
            <select
              name="download_status"
              defaultValue={params.get('download_status') || 'downloaded'}
            >
              {[
                'all',
                'pending',
                'downloading',
                'downloaded',
                'retry_wait',
                'unavailable',
                'failed',
              ].map((s) => (
                <option value={s} key={s}>
                  {s.replaceAll('_', ' ')}
                  {facets.data?.download_status[s] != null
                    ? ` (${facets.data.download_status[s]})`
                    : ''}
                </option>
              ))}
            </select>
          </label>
          <label>
            Processing state
            <select
              name="processing_status"
              defaultValue={params.get('processing_status') || ''}
            >
              <option value="">Any</option>
              {[
                'new',
                'processing',
                'done',
                'retry_wait',
                'failed',
                'missing',
              ].map((s) => (
                <option key={s} value={s}>
                  {s}
                  {facets.data?.processing_status[s] != null
                    ? ` (${facets.data.processing_status[s]})`
                    : ''}
                </option>
              ))}
            </select>
          </label>
          {[
            ['classified', 'Classification'],
            ['contains_wildlife', 'Contains wildlife'],
            ['interesting', 'Interesting'],
          ].map(([k, l]) => (
            <label key={k}>
              {l}
              <select name={k} defaultValue={params.get(k) || ''}>
                <option value="">Any</option>
                <option value="true">Yes</option>
                <option value="false">No</option>
              </select>
            </label>
          ))}
          <label>
            Time field
            <select
              name="time_field"
              defaultValue={params.get('time_field') || 'captured'}
            >
              {['captured', 'discovered', 'downloaded', 'classified'].map(
                (s) => (
                  <option key={s}>{s}</option>
                ),
              )}
            </select>
          </label>
          <label>
            Local file
            <select
              name="local_file"
              defaultValue={params.get('local_file') || ''}
            >
              <option value="">Any</option>
              <option value="present">Present</option>
              <option value="missing">Missing</option>
            </select>
          </label>
          <label>
            Species
            <input
              name="species"
              defaultValue={params.get('species') || ''}
              placeholder="e.g. Indian palm squirrel"
            />
          </label>
          <label>
            Minimum confidence
            <select
              name="confidence_min"
              defaultValue={params.get('confidence_min') || ''}
            >
              <option value="">Any</option>
              {[0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100].map((n) => (
                <option key={n} value={n / 100}>
                  {n}%
                </option>
              ))}
            </select>
          </label>
          <label>
            Model
            <input name="model" defaultValue={params.get('model') || ''} />
          </label>
          <label>
            Prompt version
            <input
              name="prompt_version"
              defaultValue={params.get('prompt_version') || ''}
            />
          </label>
          <label>
            Search summaries / cameras
            <input
              name="q"
              defaultValue={params.get('q') || ''}
              placeholder="Search…"
            />
          </label>
          <div className="actions self-end">
            <button type="submit" className="primary">
              Apply image filters
            </button>
            <button type="button" onClick={() => setParams(defaultRange())}>
              Reset
            </button>
          </div>
        </form>
      )}
      {images.isError && (
        <ErrorBox error={images.error} retry={() => images.refetch()} />
      )}{' '}
      {images.isPending ? (
        <Loading />
      ) : rows.length === 0 ? (
        <Empty
          title={
            images.hasNextPage
              ? 'No matching images in this batch'
              : 'No images match this view'
          }
        >
          <p>
            Try another time range or include pending downloads. If discovery
            has not run yet, captures will appear here once it does.
          </p>
          <button
            onClick={() =>
              setParams(
                new URLSearchParams({ range: 'all', download_status: 'all' }),
              )
            }
          >
            Show all discovered images
          </button>
        </Empty>
      ) : table ? (
        <div className="table-wrap panel">
          <table>
            <thead>
              <tr>
                <th>Image</th>
                <th>Captured</th>
                <th>Camera</th>
                <th>Download</th>
                <th>Model state</th>
                <th>Wildlife</th>
                {!hidden.includes('species') && <th>Species</th>}
                {!hidden.includes('confidence') && <th>Confidence</th>}
                {!hidden.includes('model') && <th>Model</th>}
                {!hidden.includes('completed') && <th>Completed</th>}
              </tr>
            </thead>
            <tbody>
              {rows.map((i) => (
                <tr key={i.id}>
                  <td>
                    <Link to={`/images/${i.id}?${params}`}>
                      <Photo
                        src={i.thumbnail_url}
                        alt={`Open image ${i.id}`}
                        className="table-photo"
                      />
                    </Link>
                  </td>
                  <td>
                    <Link to={`/images/${i.id}?${params}`}>
                      <Time value={i.captured_at} />
                    </Link>
                  </td>
                  <td>{nameOf(i.camera)}</td>
                  <td>
                    <Badge category="Download" value={i.download_status} />
                  </td>
                  <td>
                    <Badge category="Model" value={i.processing_status} />
                  </td>
                  <td>
                    {i.classification
                      ? i.classification.contains_wildlife
                        ? 'Yes'
                        : 'No'
                      : '—'}
                  </td>
                  {!hidden.includes('species') && (
                    <td>{speciesOf(i.classification) || '—'}</td>
                  )}
                  {!hidden.includes('confidence') && (
                    <td>{percent(i.classification?.confidence)}</td>
                  )}
                  {!hidden.includes('model') && (
                    <td>{i.classification?.model || '—'}</td>
                  )}
                  {!hidden.includes('completed') && (
                    <td>
                      <Time value={i.classification?.completed_at} />
                    </td>
                  )}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <div className="gallery">
          {rows.map((i) => (
            <ImageCard key={i.id} image={i} search={params.toString()} />
          ))}
        </div>
      )}
      {images.hasNextPage && (
        <div className="load-more">
          <button
            disabled={images.isFetchingNextPage}
            onClick={() => images.fetchNextPage()}
          >
            {images.isFetchingNextPage ? 'Loading…' : 'Load more images'}
          </button>
        </div>
      )}
      {images.data && (
        <p className="as-of">
          As of <Time value={images.data.pages[0].generated_at} />
        </p>
      )}
    </>
  );
}
