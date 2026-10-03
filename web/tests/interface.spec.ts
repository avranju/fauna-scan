import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import { readFileSync } from 'node:fs';

const captured = '2026-10-01T10:00:00Z';
const classification = {
  id: 4,
  contains_wildlife: true,
  interesting: true,
  summary: 'A squirrel pauses on the garden wall.',
  species: [{ name: 'Indian palm squirrel', confidence: 0.92 }],
  confidence: 0.92,
  model: 'vision-model',
  prompt_version: 'wildlife-v2',
  completed_at: captured,
  request_started_at: '2026-10-01T09:59:58Z',
  request_completed_at: captured,
  structured: {
    uncertainties: [
      'Low light may obscure fine details.',
      'Species identification is uncertain.',
    ],
  },
  bounding_boxes: [{ x_min: 0.2, y_min: 0.2, x_max: 0.7, y_max: 0.8 }],
};
const image = {
  id: 1,
  captured_at: captured,
  capture_end_at: captured,
  camera: {
    id: 1,
    name: 'Back garden',
    channel: 3,
    primary_track_id: '301',
    picture_track_id: '303',
  },
  thumbnail_url: '/fixture.jpg',
  content_url: '/fixture.jpg',
  download_status: 'downloaded',
  processing_status: 'done',
  classification,
};
const state = {
  status: 'done',
  attempts: 1,
  last_error: null,
  next_attempt_at: null,
  lease_until: null,
  completed_at: captured,
};
const pipeline = {
  state: 'idle',
  active: 0,
  queue_depth: 0,
  oldest_queued_at: null,
  heartbeat_at: captured,
  last_success_at: captured,
  heartbeat_fresh: true,
  stale_after_seconds: 120,
  last_error: null,
};

async function fixture(page: Page) {
  await page.route('**/fixture.jpg*', (route) =>
    route.fulfill({
      contentType: 'image/svg+xml',
      body: '<svg xmlns="http://www.w3.org/2000/svg" width="800" height="600"><rect width="800" height="600" fill="#c2cdb0"/><path d="M0 380 Q300 150 800 350 V600 H0" fill="#718b63"/><circle cx="450" cy="330" r="45" fill="#82654a"/></svg>',
    }),
  );
  await page.route('**/api/v1/**', async (route) => {
    const url = new URL(route.request().url());
    const path = url.pathname.replace('/api/v1/', '');
    if (path === 'events')
      return route.fulfill({
        contentType: 'text/event-stream',
        body: ': fixture\n\n',
      });
    let body: unknown;
    if (path === 'config')
      body = {
        version: '0.1.0',
        default_clip_pre_roll_seconds: 10,
        default_clip_post_roll_seconds: 20,
        maximum_clip_duration_seconds: 120,
        capabilities: {
          nvr_recording_lookup: true,
          browser_clip_playback: true,
          clip_download: true,
        },
      };
    else if (path === 'health')
      body = {
        status: 'ok',
        version: '0.1.0',
        web_started_at: captured,
        generated_at: captured,
        active_downloads: 0,
        active_classifications: 0,
        pipelines: { downloader: pipeline, classifier: pipeline },
      };
    else if (path === 'cameras')
      body = {
        data: [
          {
            id: 1,
            name: 'Back garden',
            channel_number: 3,
            enabled: true,
            last_completed_window_end: captured,
            last_poll_at: captured,
            next_search_at: captured,
          },
          { id: 2, name: 'Driveway', channel_number: 2, enabled: false },
        ],
      };
    else if (path === 'overview')
      body = {
        camera_counts: [{ camera_id: 1, discovered: 142, classified: 112 }],
        counts: {
          discovered: 142,
          downloaded: 120,
          classified: 112,
          wildlife: 23,
          interesting: 8,
          retryable_failures: 2,
          permanent_failures: 1,
        },
        buckets: Array.from({ length: 12 }, (_, i) => ({
          start_at: `2026-10-01T${String(i).padStart(2, '0')}:00:00Z`,
          end_at: `2026-10-01T${String(i + 1).padStart(2, '0')}:00:00Z`,
          discovered: i + 2,
          downloaded: i,
          classified: Math.max(0, i - 2),
        })),
        generated_at: captured,
      };
    else if (path === 'activity')
      body = {
        counts: [
          { category: 'download', status: 'downloaded', count: 120 },
          { category: 'processing', status: 'retry_wait', count: 2 },
        ],
        active: [],
        generated_at: captured,
      };
    else if (path === 'images/facets')
      body = {
        download_status: { downloaded: 120, pending: 22 },
        processing_status: { done: 112, new: 8 },
      };
    else if (path === 'images')
      body = {
        data:
          url.searchParams.get('q') === 'no results'
            ? []
            : [{ ...image, id: url.searchParams.has('cursor') ? 2 : 1 }],
        page: {
          has_more:
            !url.searchParams.has('cursor') &&
            url.searchParams.get('q') !== 'no results',
          next_cursor:
            url.searchParams.has('cursor') ||
            url.searchParams.get('q') === 'no results'
              ? null
              : 'opaque-next',
        },
        generated_at: captured,
      };
    else if (/images\/\d+\/neighbors/.test(path))
      body = { previous: null, next: 2 };
    else if (/images\/\d+\/recording/.test(path))
      body = {
        status: 'found',
        requested_start_at: captured,
        requested_end_at: captured,
        recording_start_at: captured,
        recording_end_at: captured,
        nvr_playback_uri: 'rtsp://nvr.local/Streaming/tracks/301',
        capabilities: { browser_playback: true, download: true },
      };
    else if (/images\/\d+\/clip$/.test(path))
      body = {
        playback_url: '/api/v1/clips/fixture',
        download_url: '/api/v1/clips/fixture?download=true',
        expires_in_seconds: 900,
      };
    else if (path === 'clips/fixture')
      return route.fulfill({
        contentType: 'video/mp4',
        body: readFileSync(new URL('./fixtures/clip.mp4', import.meta.url)),
        headers: {
          'Content-Disposition': url.searchParams.has('download')
            ? 'attachment; filename="clip.mp4"'
            : 'inline',
        },
      });
    else if (/images\/\d+$/.test(path))
      body = {
        ...image,
        image_key: 'fixture-image',
        discovered_at: captured,
        download: { ...state, status: 'downloaded', downloaded_at: captured },
        processing: state,
        nvr: {
          image_url: 'http://nvr.local/picture/1',
          reported_image_url: null,
        },
        classifications: [
          classification,
          {
            ...classification,
            id: 3,
            model: 'older-model',
            summary: 'Earlier classification summary.',
            confidence: 0.7,
            bounding_boxes: null,
            structured: { uncertainties: ['Only one uncertainty.'] },
          },
        ],
      };
    else
      return route.fulfill({
        status: 404,
        json: { error: { message: 'Not found' } },
      });
    return route.fulfill({ json: body });
  });
}

test.beforeEach(async ({ page }) => fixture(page));

test('filters are shareable, validate ranges, and survive browser navigation', async ({
  page,
}) => {
  await page.goto('/images?range=all');
  await page.getByRole('button', { name: 'All time', exact: true }).click();
  await page.getByLabel('Time range', { exact: true }).selectOption('custom');
  await page.getByLabel('Display timezone').fill('UTC');
  await page.getByLabel('From (inclusive)').fill('2026-10-01T10:00');
  await page.getByLabel('To (exclusive)').fill('2026-10-01T09:00');
  await page
    .getByRole('button', { name: 'Apply filters', exact: true })
    .click();
  await expect(page.getByRole('alert')).toContainText('End must be later');
  await page.getByLabel('To (exclusive)').fill('2026-10-02T10:00');
  await page.getByRole('checkbox', { name: /Back garden/ }).check();
  await page.getByRole('checkbox', { name: /Driveway/ }).check();
  await page
    .getByRole('button', { name: 'Apply filters', exact: true })
    .click();
  await expect(page).toHaveURL(/camera=1&camera=2/);
  expect(new URL(page.url()).searchParams.get('from')).toBe(
    '2026-10-01T10:00:00.000Z',
  );
  await page.getByRole('button', { name: 'Custom range', exact: true }).click();
  await page.getByLabel('Display timezone').fill('Asia/Kolkata');
  await page
    .getByRole('button', { name: 'Apply filters', exact: true })
    .click();
  expect(new URL(page.url()).searchParams.get('from')).toBe(
    '2026-10-01T10:00:00.000Z',
  );
  expect(new URL(page.url()).searchParams.get('to')).toBe(
    '2026-10-02T10:00:00.000Z',
  );
  await page.getByRole('button', { name: 'Table view' }).click();
  await expect(page.getByRole('table')).toBeVisible();
  await page.goBack();
  await expect(
    page.getByRole('button', { name: 'Gallery view' }),
  ).toHaveAttribute('aria-pressed', 'true');
  await page.reload();
  await expect(
    page.getByRole('button', { name: '2 cameras', exact: true }),
  ).toBeVisible();
});

test('image filters and pagination use the server and preserve existing items', async ({
  page,
}) => {
  await page.goto('/images?range=all');
  await page.getByRole('button', { name: /Image filters/ }).click();
  await page.getByLabel('Species', { exact: true }).fill('squirrel');
  await page.getByLabel('Contains wildlife').selectOption('true');
  await page.getByLabel('Minimum confidence').selectOption('0.8');
  await page.getByRole('button', { name: 'Apply image filters' }).click();
  await expect(page).toHaveURL(/species=squirrel/);
  await page.getByRole('button', { name: 'Load more images' }).click();
  await expect(page.locator('.image-card')).toHaveCount(2);
  await page.getByLabel('Search summaries / cameras').fill('no results');
  await page.getByRole('button', { name: 'Apply image filters' }).click();
  await expect(
    page.getByRole('heading', { name: 'No images match this view' }),
  ).toBeVisible();
});

test('detail shows provenance, older results, zoom, and on-demand NVR recording', async ({
  page,
}) => {
  let recordingRequests = 0;
  page.on('request', (request) => {
    if (request.url().includes('/recording')) recordingRequests++;
  });
  await page.goto('/images/1');
  await expect(
    page.getByRole('heading', { name: 'Back garden' }),
  ).toBeVisible();
  expect(recordingRequests).toBe(0);
  await page.getByRole('button', { name: 'Zoom in', exact: true }).click();
  await page.getByLabel('Classification result').selectOption('3');
  await expect(page.locator('.full-summary')).toHaveText(
    'Earlier classification summary.',
  );
  await page.getByRole('tab', { name: 'Processing', exact: true }).click();
  await expect(
    page.getByRole('heading', { name: 'Current lifecycle' }),
  ).toBeVisible();
  await page.getByRole('tab', { name: 'NVR & files' }).click();
  await expect(
    page.getByRole('link', { name: 'Open Local Image', exact: true }),
  ).toHaveAttribute('href', '/fixture.jpg?draw-bounding-box=true');
  await expect(page.locator('code').first()).toHaveText(
    'http://nvr.local/picture/1',
  );
  expect(recordingRequests).toBe(0);
  await page.getByRole('button', { name: 'Find recording' }).click();
  await expect(
    page.getByRole('link', { name: 'Open in video player' }),
  ).toHaveAttribute('rel', 'noopener noreferrer');
  expect(recordingRequests).toBe(1);
  const video = page.locator('video');
  await expect(video).toBeVisible();
  await expect(
    page.getByRole('link', { name: 'Download video' }),
  ).toHaveAttribute('href', '/api/v1/clips/fixture?download=true');
  await expect
    .poll(() => video.evaluate((v: HTMLVideoElement) => v.readyState))
    .toBeGreaterThan(0);
  await video.evaluate((v: HTMLVideoElement) => v.play());
  await expect
    .poll(() => video.evaluate((v: HTMLVideoElement) => v.currentTime))
    .toBeGreaterThan(0);
  await page.getByLabel('Seconds before').fill('5');
  await expect(video).toHaveCount(0);
});

test('all screens are accessible and fit desktop/mobile viewports', async ({
  page,
}, testInfo) => {
  for (const route of [
    '/?range=all',
    '/images?range=all',
    '/images/1',
    '/activity?range=all',
    '/about',
  ]) {
    await page.goto(route);
    await expect(page.locator('h1')).toBeVisible();
    await expect(page.locator('.loading')).toHaveCount(0);
    const accessibility = await new AxeBuilder({ page })
      .withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa'])
      .analyze();
    expect(accessibility.violations).toEqual([]);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= window.innerWidth,
      ),
    ).toBe(true);
    await page.screenshot({
      path: testInfo.outputPath(
        `${route.split('?')[0].replaceAll('/', '_') || 'overview'}.png`,
      ),
      fullPage: true,
    });
  }
});

test('missing images keep metadata usable and API failures can be retried', async ({
  page,
}) => {
  await page.route('**/fixture.jpg*', (route) =>
    route.fulfill({ status: 404 }),
  );
  await page.goto('/images/1');
  await expect(
    page.getByRole('heading', { name: 'Local image unavailable' }),
  ).toBeVisible();
  await expect(
    page.getByRole('heading', { name: 'Wildlife spotted' }),
  ).toBeVisible();
  await page.route('**/api/v1/overview*', (route) =>
    route.fulfill({
      status: 503,
      json: { error: { message: 'Database temporarily unavailable' } },
    }),
  );
  await page.goto('/?range=all');
  await expect(page.getByRole('alert')).toContainText(
    'Database temporarily unavailable',
  );
  await expect(page.getByRole('button', { name: 'Try again' })).toBeVisible();
});

test('live disconnects keep images visible, poll, and recover', async ({
  page,
}) => {
  await page.addInitScript(() => {
    class OfflineEvents extends EventTarget {
      onopen: (() => void) | null = null;
      onerror: (() => void) | null = null;

      constructor() {
        super();
        Object.assign(window, { testLiveEvents: this });
        setTimeout(() => this.onerror?.(), 50);
        setTimeout(() => this.onerror?.(), 100);
      }

      close() {}
    }

    Object.assign(window, { EventSource: OfflineEvents });
  });
  let reads = 0;
  page.on('request', (request) => {
    if (new URL(request.url()).pathname === '/api/v1/images') reads++;
  });
  await page.goto('/images?range=all');
  await expect(page.locator('.image-card')).toHaveCount(1);
  await expect(page.getByText(/Live updates disconnected/)).toBeVisible();
  await expect.poll(() => reads, { timeout: 8000 }).toBeGreaterThan(1);
  await expect(page.locator('.image-card')).toHaveCount(1);
  await page.evaluate(() => {
    const source = (window as unknown as { testLiveEvents: EventTarget })
      .testLiveEvents;
    source.dispatchEvent(new MessageEvent('invalidate', { data: '{}' }));
  });
  await expect(page.getByText(/Live updates disconnected/)).toHaveCount(0);
});

test('every histogram bar shows its capture interval start', async ({
  page,
}) => {
  await page.goto('/?range=all&tz=UTC');
  await expect(page.locator('.histogram-label')).toHaveCount(12);
  await expect(page.locator('.histogram-label').first()).toContainText('00:00');
  await expect(page.locator('.histogram-label').last()).toContainText('11:00');
});

test('zoom controls stay clickable and bounding boxes default on', async ({
  page,
}) => {
  await page.goto('/images/1');
  const boxes = page.getByRole('checkbox', { name: /Draw latest/ });
  const photo = page.locator('.viewer img');
  await expect(boxes).toBeChecked();
  await expect(photo).toHaveAttribute('src', /draw-bounding-box=true/);
  for (let i = 0; i < 6; i++) {
    await page.getByRole('button', { name: 'Zoom in', exact: true }).click();
    await page.waitForTimeout(250);
  }
  for (const name of ['Zoom out', '100%', 'Fit', 'Reset image view']) {
    await page.getByRole('button', { name, exact: true }).click();
  }
  await boxes.uncheck();
  await expect(photo).not.toHaveAttribute('src', /draw-bounding-box/);
  await page.getByRole('link', { name: 'Next', exact: true }).click();
  await expect(boxes).toBeChecked();
});

test('uncertainties show a list for multiple entries and plain text for one', async ({
  page,
}) => {
  await page.goto('/images/1');
  await expect(page.locator('.uncertainties li')).toHaveText([
    'Low light may obscure fine details.',
    'Species identification is uncertain.',
  ]);
  await page.getByLabel('Classification result').selectOption('3');
  await expect(page.locator('.uncertainties')).toHaveCount(0);
  await expect(
    page.getByText('Only one uncertainty.', { exact: true }),
  ).toBeVisible();
});

test('copy falls back when Clipboard API is unavailable or denied', async ({
  page,
}) => {
  await page.addInitScript(() => {
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: undefined,
    });
    Object.assign(window, { copiedValues: [] });
    document.execCommand = (command: string) => {
      if (command !== 'copy') return false;
      const field = document.activeElement as HTMLTextAreaElement;
      (window as unknown as { copiedValues: string[] }).copiedValues.push(
        field.value.slice(field.selectionStart, field.selectionEnd),
      );
      return true;
    };
  });
  await page.goto('/images/1');
  const button = page.getByRole('button', { name: 'Copy detail link' });
  await button.click();
  await expect(
    page.getByText('Copied to clipboard', { exact: true }),
  ).toBeVisible();
  expect(
    await page.evaluate(
      () => (window as unknown as { copiedValues: string[] }).copiedValues,
    ),
  ).toEqual([page.url()]);
  await expect(button).toBeFocused();
  await page.evaluate(() =>
    Object.defineProperty(navigator, 'clipboard', {
      value: { writeText: () => Promise.reject(new Error('Denied')) },
    }),
  );
  await button.click();
  expect(
    await page.evaluate(
      () =>
        (window as unknown as { copiedValues: string[] }).copiedValues.length,
    ),
  ).toBe(2);
  await page.goto('/images');
  await page.getByLabel('Actions for image 1').click();
  await page
    .getByRole('button', { name: 'Copy NVR image URL', exact: true })
    .click();
  await expect(
    page.getByText('NVR image URL copied', { exact: true }),
  ).toBeVisible();
  expect(
    await page.evaluate(() =>
      (window as unknown as { copiedValues: string[] }).copiedValues.at(-1),
    ),
  ).toBe('http://nvr.local/picture/1');
});

test('HTTP copy fallback writes to the actual system clipboard', async ({
  page,
}, testInfo) => {
  test.skip(
    testInfo.project.name === 'mobile',
    'System clipboard is shared across browser contexts.',
  );
  await page.context().grantPermissions(['clipboard-read', 'clipboard-write']);
  await page.addInitScript(() => {
    Object.assign(window, { nativeClipboard: navigator.clipboard });
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: undefined,
    });
  });
  await page.goto('/images/1');
  await page
    .getByRole('button', { name: 'Copy detail link', exact: true })
    .click();
  expect(
    await page.evaluate(() =>
      (
        window as unknown as { nativeClipboard: Clipboard }
      ).nativeClipboard.readText(),
    ),
  ).toBe(page.url());
  await page.goto('/images');
  await page.getByLabel('Actions for image 1').click();
  await page
    .getByRole('button', { name: 'Copy NVR image URL', exact: true })
    .click();
  expect(
    await page.evaluate(() =>
      (
        window as unknown as { nativeClipboard: Clipboard }
      ).nativeClipboard.readText(),
    ),
  ).toBe('http://nvr.local/picture/1');
});
