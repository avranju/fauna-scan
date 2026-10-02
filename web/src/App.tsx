import { useEffect, useState } from 'react';
import {
  Link,
  NavLink,
  Navigate,
  Route,
  Routes,
  useLocation,
  useSearchParams,
} from 'react-router-dom';
import { useQueryClient } from '@tanstack/react-query';
import {
  Activity as ActivityIcon,
  ArrowUpRight,
  Camera as CameraIcon,
  Images as ImagesIcon,
  Info,
  LayoutDashboard,
  Leaf,
  Radio,
  RefreshCw,
} from 'lucide-react';
import { DateTime } from 'luxon';
import { useApi, type Camera, type Config, type Health } from './api';
import { defaultRange, scopeParams } from './filters';
import { Badge, ErrorBox, Loading, Time, Toast, Zone } from './ui';
import { FilterBar } from './FilterBar';
import { Overview } from './Overview';
import { Images } from './Images';
import { Detail } from './Detail';
import { Activity } from './Activity';

function About() {
  const config = useApi<Config>('config');
  const health = useApi<Health>('health');
  return (
    <>
      <div className="page-heading">
        <div>
          <p className="eyebrow">YOUR LOCAL OBSERVATORY</p>
          <h1>About Fauna Scan</h1>
          <p className="muted">Wildlife discovery, close to home.</p>
        </div>
        <Leaf size={34} />
      </div>
      <section className="panel prose-panel">
        <h2>A read-only window into your cameras</h2>
        <p>
          Fauna Scan discovers NVR still images and uses vision models to
          identify wildlife. Images and processing state remain on your server.
          This interface explores captures and monitors work without changing
          your cameras or classifications.
        </p>
        {config.isError ? (
          <ErrorBox error={config.error} retry={() => config.refetch()} />
        ) : config.isPending ? (
          <Loading />
        ) : (
          <dl className="facts">
            <dt>NVR</dt>
            <dd>{config.data.nvr_identity || 'Not reported'}</dd>
            <dt>Version</dt>
            <dd>{config.data.version}</dd>
            <dt>Web started</dt>
            <dd>
              <Time value={health.data?.web_started_at} />
            </dd>
            <dt>Default recording interval</dt>
            <dd>
              {config.data.default_clip_pre_roll_seconds}s before /{' '}
              {config.data.default_clip_post_roll_seconds}s after
            </dd>
            <dt>Maximum recording interval</dt>
            <dd>{config.data.maximum_clip_duration_seconds}s</dd>
            <dt>NVR recording lookup</dt>
            <dd>
              {config.data.capabilities.nvr_recording_lookup
                ? 'Available on request'
                : 'Unavailable'}
            </dd>
            <dt>Browser clip playback</dt>
            <dd>
              {config.data.capabilities.browser_clip_playback
                ? 'Available'
                : 'No compatible adapter configured'}
            </dd>
            <dt>Clip download</dt>
            <dd>
              {config.data.capabilities.clip_download
                ? 'Available'
                : 'No bounded download adapter configured'}
            </dd>
          </dl>
        )}
        <h3>Time & privacy</h3>
        <p>
          Dates use the display timezone selected in the filters. Search
          boundaries are stored as UTC instants in shareable URLs. NVR
          credentials and local server paths are never sent to the browser.
        </p>
      </section>
    </>
  );
}

export function App() {
  const [params] = useSearchParams();
  const location = useLocation();
  const client = useQueryClient();
  const cameras = useApi<{ data: Camera[] }>('cameras');
  const health = useApi<Health>('health');
  const [disconnected, setDisconnected] = useState(false);
  const [toast, setToast] = useState('');
  const [clock, setClock] = useState(Date.now());
  const requestedZone =
    params.get('tz') || Intl.DateTimeFormat().resolvedOptions().timeZone;
  const zone = DateTime.now().setZone(requestedZone).isValid
    ? requestedZone
    : 'UTC';
  const filterPage = ['/', '/images', '/activity'].includes(location.pathname);
  const search = scopeParams(params).toString();
  useEffect(() => {
    let failures = 0;
    let connected = false;
    let coalesce: ReturnType<typeof setTimeout> | undefined;
    let source: EventSource | undefined;

    const invalidate = (resources?: string[]) => {
      if (document.visibilityState === 'visible')
        client.invalidateQueries({
          type: 'active',
          predicate: (query) =>
            !resources ||
            resources.includes(String(query.queryKey[0]).split('/')[0]),
        });
    };

    const connect = () => {
      source?.close();
      source = new EventSource('/api/v1/events');
      source.onopen = () => {
        connected = true;
      };
      source.onerror = () => {
        connected = false;
        if (++failures >= 2) setDisconnected(true);
      };
      source.addEventListener('invalidate', (event) => {
        failures = 0;
        setDisconnected(false);
        let resources: string[] | undefined;
        try {
          resources = JSON.parse((event as MessageEvent).data).resources;
        } catch {
          /* a full refresh is safe for an unknown event */
        }
        if (!coalesce)
          coalesce = setTimeout(() => {
            invalidate(resources);
            coalesce = undefined;
          }, 250);
      });
    };

    const visibility = () => {
      if (document.visibilityState === 'visible') {
        connect();
        invalidate();
      } else {
        source?.close();
        connected = false;
      }
    };

    if (document.visibilityState === 'visible') connect();
    const poll = setInterval(() => {
      if (!connected) invalidate();
    }, 5000);
    const tick = setInterval(() => setClock(Date.now()), 1000);
    document.addEventListener('visibilitychange', visibility);
    return () => {
      source?.close();
      clearInterval(poll);
      clearInterval(tick);
      clearTimeout(coalesce);
      document.removeEventListener('visibilitychange', visibility);
    };
  }, [client]);
  useEffect(() => {
    if (!toast) return;
    const timer = setTimeout(() => setToast(''), 5000);
    return () => clearTimeout(timer);
  }, [toast]);
  useEffect(() => {
    document.title = `${location.pathname === '/' ? 'Overview' : location.pathname === '/activity' ? 'Scan activity' : location.pathname === '/about' ? 'About' : 'Images'} · Fauna Scan`;
  }, [location.pathname]);
  const nav = [
    ['/', 'Overview', LayoutDashboard],
    ['/images', 'Images', ImagesIcon],
    ['/activity', 'Scan activity', ActivityIcon],
  ] as const;
  if (
    filterPage &&
    params.get('range') !== 'all' &&
    !params.has('from') &&
    !params.has('to')
  ) {
    const next = new URLSearchParams(params);
    const defaults = defaultRange();
    defaults.forEach((value, key) => next.set(key, value));
    return <Navigate to={`${location.pathname}?${next}`} replace />;
  }
  return (
    <Zone.Provider value={zone}>
      <Toast.Provider value={setToast}>
        <a className="skip-link" href="#main">
          Skip to content
        </a>
        <header className="app-header">
          <div className="header-inner">
            <Link to="/" className="brand">
              <span className="brand-mark">
                <Leaf size={23} />
              </span>
              <span>
                fauna<span className="brand-light">scan</span>
                <small>A LITTLE CLOSER TO NATURE</small>
              </span>
            </Link>
            <nav aria-label="Main navigation">
              {nav.map(([path, label, Icon]) => (
                <NavLink key={path} to={`${path}?${search}`} end={path === '/'}>
                  <Icon size={17} />
                  {label}
                </NavLink>
              ))}
            </nav>
            <div className="header-status">
              <span title="API connectivity is separate from pipeline health">
                <Radio size={14} />
                {health.isError
                  ? 'API unavailable'
                  : health.data
                    ? 'API connected'
                    : 'Connecting'}
              </span>
              <time dateTime={new Date(clock).toISOString()}>
                {DateTime.fromMillis(clock).setZone(zone).toFormat('HH:mm')}
              </time>
              <NavLink to="/about" aria-label="About Fauna Scan">
                <Info size={19} />
              </NavLink>
            </div>
          </div>
        </header>
        <main id="main" className="main">
          <div className="breadcrumb">
            <span>WORKSPACE</span>
            <span>/</span>
            <span>
              {location.pathname === '/'
                ? 'Overview'
                : location.pathname === '/activity'
                  ? 'Scan activity'
                  : location.pathname === '/about'
                    ? 'About'
                    : 'Images'}
            </span>
          </div>
          {disconnected && (
            <div className="notice" role="status">
              <RefreshCw size={16} />
              Live updates disconnected. Refreshing every 5 seconds while this
              page is visible. Last successful data stays on screen.
            </div>
          )}
          {health.isError && (
            <ErrorBox error={health.error} retry={() => health.refetch()} />
          )}
          {cameras.isError && filterPage && (
            <ErrorBox error={cameras.error} retry={() => cameras.refetch()} />
          )}
          {filterPage && <FilterBar cameras={cameras.data?.data || []} />}
          <Routes>
            <Route
              path="/"
              element={
                <Overview
                  cameras={cameras.data?.data || []}
                  health={health.data}
                />
              }
            />
            <Route path="/images" element={<Images />} />
            <Route path="/images/:imageId" element={<Detail />} />
            <Route
              path="/activity"
              element={
                <Activity
                  cameras={cameras.data?.data || []}
                  health={health.data}
                />
              }
            />
            <Route path="/about" element={<About />} />
            <Route
              path="*"
              element={
                <section className="empty">
                  <h1>Page not found</h1>
                  <Link className="button primary" to="/">
                    Return to overview
                  </Link>
                </section>
              }
            />
          </Routes>
          <footer>
            <span>
              <Leaf size={14} />
              Fauna Scan · Your local wildlife observatory
            </span>
            <Link to="/about">
              System information <ArrowUpRight size={13} />
            </Link>
          </footer>
        </main>
        <div className="toast" role="status" aria-live="polite">
          {toast}
        </div>
        <nav className="mobile-nav" aria-label="Mobile navigation">
          {nav.map(([path, label, Icon]) => (
            <NavLink key={path} to={`${path}?${search}`} end={path === '/'}>
              <Icon size={21} />
              {label}
            </NavLink>
          ))}
          <NavLink to="/about">
            <Info size={21} />
            About
          </NavLink>
        </nav>
      </Toast.Provider>
    </Zone.Provider>
  );
}
