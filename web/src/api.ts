import { useQuery } from '@tanstack/react-query';

export interface Camera {
  id: number;
  name: string | null;
  channel_number: number;
  enabled: boolean;
  primary_track_id: string;
  picture_track_id: string;
  last_completed_window_end: string | null;
  last_poll_at: string | null;
  next_search_at: string | null;
  last_error: string | null;
}

export interface Classification {
  id: number;
  contains_wildlife: boolean;
  interesting: boolean;
  summary: string | null;
  species: ({ name: string; confidence?: number } | string)[];
  confidence: number | null;
  model: string;
  prompt_version: string;
  completed_at: string;
  request_started_at?: string;
  request_completed_at?: string;
  bounding_boxes?:
    { x_min: number; y_min: number; x_max: number; y_max: number }[] | null;
  structured?: Record<string, unknown>;
}

export interface ImageRecord {
  id: number;
  captured_at: string;
  capture_end_at: string | null;
  camera: {
    id: number;
    name: string | null;
    channel: number;
    primary_track_id?: string;
    picture_track_id?: string;
  };
  content_url: string | null;
  thumbnail_url: string | null;
  download_status: string;
  processing_status: string;
  classification: Classification | null;
}

export interface Lifecycle {
  status: string;
  attempts: number;
  last_error: string | null;
  next_attempt_at: string | null;
  lease_until: string | null;
  downloaded_at?: string;
  started_at?: string;
  completed_at?: string;
}

export interface ImageDetail extends Omit<
  ImageRecord,
  'classification' | 'thumbnail_url' | 'download_status' | 'processing_status'
> {
  file?: { present: boolean; name?: string; size_bytes?: number };
  image_key: string;
  discovered_at: string;
  download: Lifecycle;
  processing: Lifecycle;
  nvr: { image_url: string | null; reported_image_url: string | null };
  classifications: Classification[];
}

export interface ImagePage {
  data: ImageRecord[];
  page: { next_cursor: string | null; has_more: boolean };
  generated_at: string;
}

export interface Config {
  nvr_identity?: string;
  version: string;
  default_clip_pre_roll_seconds: number;
  default_clip_post_roll_seconds: number;
  maximum_clip_duration_seconds: number;
  capabilities: {
    nvr_recording_lookup: boolean;
    browser_clip_playback: boolean;
    clip_download: boolean;
  };
}

export interface Pipeline {
  state: string;
  active: number;
  queue_depth: number;
  oldest_queued_at: string | null;
  heartbeat_at: string | null;
  last_success_at: string | null;
  heartbeat_fresh: boolean;
  stale_after_seconds: number;
  last_error: string | null;
}

export interface Health {
  pipelines?: { downloader: Pipeline; classifier: Pipeline };
  status: string;
  version: string;
  web_started_at: string;
  last_camera_discovery: string | null;
  last_downloader_poll: string | null;
  last_scanner_pass: string | null;
  active_downloads: number;
  active_classifications: number;
  generated_at: string;
}

export interface Active {
  id: number;
  captured_at: string;
  camera_name: string | null;
  channel: number;
  download_status: string;
  download_attempts: number;
  download_lease_until: string | null;
  processing_status: string;
  processing_attempts: number;
  processing_started_at: string | null;
  processing_lease_until: string | null;
}

export interface Activity {
  counts: { category: string; status: string; count: number }[];
  active: Active[];
  generated_at: string;
}

export interface Overview {
  camera_counts?: {
    camera_id: number;
    discovered: number;
    classified: number;
  }[];
  counts: Record<string, number>;
  generated_at: string;
  buckets?: {
    start_at: string;
    end_at: string;
    discovered: number;
    downloaded: number;
    classified: number;
  }[];
}

export interface Recording {
  status: 'found' | 'not_found';
  requested_start_at: string;
  requested_end_at: string;
  recording_start_at?: string;
  recording_end_at?: string;
  nvr_playback_uri?: string;
  capabilities?: { browser_playback: boolean; download: boolean };
}

export interface Clip {
  playback_url: string;
  download_url: string;
  expires_in_seconds: number;
}

export class ApiError extends Error {
  constructor(
    message: string,
    public status: number,
  ) {
    super(message);
  }
}

export async function api<T>(
  path: string,
  signal?: AbortSignal,
  method = 'GET',
  body?: unknown,
): Promise<T> {
  const response = await fetch(`/api/v1/${path}`, {
    method,
    signal,
    credentials: 'same-origin',
    headers: {
      Accept: 'application/json',
      'X-Fauna-Scan-Request': '1',
      ...(body === undefined ? {} : { 'Content-Type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!response.ok) {
    const error = await response.json().catch(() => null);
    if (response.status === 401 && !path.startsWith('auth/')) {
      window.dispatchEvent(new Event('fauna-scan:unauthenticated'));
    }
    throw new ApiError(
      error?.error?.message ||
        `Request failed (${response.status}). Please try again.`,
      response.status,
    );
  }
  if (response.status === 204) return undefined as T;
  return response.json();
}

export function useApi<T>(resource: string, params = '', enabled = true) {
  return useQuery<T>({
    enabled,
    queryKey: [resource, params],
    queryFn: ({ signal }) =>
      api<T>(resource + (params ? `?${params}` : ''), signal),
  });
}

export const nameOf = (camera: {
  name: string | null;
  channel?: number;
  channel_number?: number;
}) => camera.name || `Camera ${camera.channel ?? camera.channel_number}`;

export const speciesOf = (c?: Classification | null) =>
  (c?.species || [])
    .map((s) => (typeof s === 'string' ? s : s.name))
    .join(', ');

export const percent = (n?: number | null) =>
  n == null ? '—' : `${Math.round(n * 100)}%`;
