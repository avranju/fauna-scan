# Fauna Scan Web Interface Design Specification

Status: implemented for milestones 1–3; browser media adapters remain capability-gated
Audience: product, design, and implementation
Last updated: 2026-10-01

## 1. Summary

Fauna Scan should provide a local-first, read-only web interface for exploring downloaded images, watching downloader and classifier work in progress, inspecting classification results, and following an image back to the corresponding still image and video recording on the NVR.

The interface should be part of the existing `fauna-scan` service and use the existing SQLite database as its source of truth. The recommended implementation is a small HTTP API plus an embedded browser application served by the Rust executable. NVR and classifier credentials must remain on the server; they must never be placed in HTML, JavaScript, browser storage, query strings, or generated NVR URLs.

The primary navigation is:

1. **Overview** — current health, queue state, recent throughput, and camera progress.
2. **Images** — a filterable gallery/table of discovered and downloaded images.
3. **Scan activity** — active downloads/classifications, retries, failures, and per-camera search progress.
4. **Image detail** — a shareable route for the image, model result, processing history, and NVR actions.

This document specifies the user experience, data semantics, API, backend additions, security model, and acceptance criteria. It extends the [original service requirements](spec.md), which explicitly treat a web dashboard and video handling as out of scope for the initial release.

## 2. Goals

The web interface must let an operator:

- See which images Fauna Scan has discovered and which image files are available locally.
- Filter images and operational data over any time interval and one or more cameras.
- Quickly narrow images by classification result, including wildlife, interestingness, species, confidence, and processing state.
- Inspect the model summary and structured classification for an individual image.
- See whether the service is alive, idle, backfilling, actively downloading, actively classifying, retrying, or degraded.
- See the current item and elapsed time for every active download or classification claim.
- Copy or open the full NVR still-image URL associated with an image.
- Obtain the NVR-provided video playback URI for a recording that contains the image timestamp.
- View or download the bounded video clip when the NVR and deployment provide a supported media adapter.
- Share a URL that reproduces the current image filters or opens a specific image detail page.

## 3. Non-goals for the first web release

The first release should not:

- Change camera, NVR recording, or detector configuration.
- Delete images or NVR recordings.
- Trigger reclassification, retry, or redownload operations from the browser.
- Edit model classifications or add human labels.
- Analyze video clips with the model.
- Correlate several still images into an event.
- Provide Internet-facing multi-tenant hosting.
- Attempt to play RTSP directly in browsers that do not support it.
- Invent an NVR recording URL from a track and timestamp when the NVR can return the authoritative recording URI.

Those capabilities can be added later without changing the core navigation or image API.

## 4. Existing system facts and constraints

The design uses these existing Fauna Scan concepts:

- `cameras` contains channel, primary-video track, picture track, name, enabled state, and discovery timestamps.
- `images` contains capture time, NVR `playback_uri`, rebased `canonical_playback_uri`, local path, download state, processing state, attempts, errors, retry times, and lease times.
- `classifications` contains model, prompt version, wildlife and interesting flags, summary, species JSON, bounding boxes JSON, confidence, normalized classification JSON, raw response, and request timestamps.
- `search_cursors` contains the per-camera completed search window, next search time, last poll time, and last error.
- `service_metadata` records milestones such as last successful camera discovery, downloader poll, and scanner pass.
- The downloader can process more than one image concurrently. Each configured classifier endpoint owns one worker, so more than one classification may be active.
- SQLite is the durable coordination mechanism. The UI must read durable state and must not depend on in-process worker objects to reconstruct state after restart.
- Timestamps are stored as RFC 3339 text and are normalized to UTC by the domain layer.
- The image `playback_uri` refers to the still-picture search result. It is not automatically a video recording URI.
- Browser access to an NVR URL may require Digest authentication or a native RTSP handler. A copied URL is still useful even when the browser cannot render it directly.

## 5. Product principles

### 5.1 Capture time is the default timeline

Wildlife exploration is about when an image was captured, not when a delayed backfill happened to download or classify it. All screens therefore use `capture_start_at` by default. Where useful, the operator can switch the time field to discovery, download, or classification completion time.

### 5.2 Current state and historical activity are different

Queue counts and active leases describe current state. Recent completion charts describe historical activity. The UI must label these separately and must not infer that a quiet queue means the service is healthy; heartbeat and cursor freshness determine health.

### 5.3 Failures remain visible

Failed, unavailable, missing, and retry-wait items must remain discoverable through filters. Errors should be shown in plain language with attempt count and next retry time, without exposing secrets or raw upstream response bodies.

### 5.4 A result never hides its provenance

Every classification view shows the model, prompt version, request completion time, and confidence alongside the summary. If multiple classification rows exist for an image, the UI presents the newest result first and makes older results available.

### 5.5 NVR links are explicit and credential-free

The UI differentiates the NVR still URL, the NVR video playback URI, and a Fauna Scan media endpoint. It must never imply that one is another, silently attach credentials, or expose a secret-bearing URL.

## 6. Information architecture and routes

| Route | Screen | Purpose |
| --- | --- | --- |
| `/` | Overview | Health, queues, throughput, and per-camera progress |
| `/images` | Images | Filterable image gallery or table |
| `/images/:imageId` | Image detail | Full image, classification, state, and NVR links |
| `/activity` | Scan activity | Live work, retries, failures, and cursors |
| `/about` | About/system | Version, NVR identity, timezone, and non-secret configuration |

Filters on `/`, `/images`, and `/activity` are encoded in the query string. For example:

```text
/images?from=2026-07-20T00%3A00%3A00Z&to=2026-07-21T00%3A00%3A00Z&camera=2&camera=4&wildlife=true&sort=captured_desc
```

Browser Back/Forward must restore the exact filter state. Copying the address must produce the same view for another authorized user, except for ephemeral pagination cursors.

## 7. Global application shell

Desktop layout:

```text
┌────────────────────────────────────────────────────────────────────────────┐
│ Fauna Scan       Overview  Images  Scan activity       ● Healthy   14:32  │
├────────────────────────────────────────────────────────────────────────────┤
│ Time: [Last 24 hours ▾] [20 Jul 00:00 — 21 Jul 00:00]  Cameras: [All ▾] │
│ Timezone: Asia/Kolkata                                      [Reset]        │
├────────────────────────────────────────────────────────────────────────────┤
│                                                                            │
│                         current screen                                     │
│                                                                            │
└────────────────────────────────────────────────────────────────────────────┘
```

The shell contains:

- Product name and primary navigation.
- A compact service-health indicator with tooltip details.
- A global time range and camera filter shared by Overview, Images, and Scan activity.
- The active display timezone.
- A visible reset action whenever a non-default filter is active.
- A reconnect banner when live updates are disconnected.

On screens below 768 px, primary navigation becomes a bottom navigation bar, filter controls open in a full-width sheet, and the image detail layout becomes one column.

## 8. Time and camera filtering

### 8.1 Time range behavior

Preset ranges are:

- Last 15 minutes
- Last hour
- Last 6 hours
- Last 24 hours (default)
- Last 7 days
- Last 30 days
- Custom
- All time

A custom range accepts local date and time for both boundaries. The UI converts them to RFC 3339 UTC values before sending them to the API. The API treats intervals as half-open: `from <= timestamp < to`. This avoids double-counting adjacent windows.

The UI must:

- Display the selected IANA timezone, defaulting to the browser timezone.
- Preserve the UTC instants if the display timezone changes.
- Reject an end earlier than or equal to the start.
- Show an explicit empty-range message instead of silently widening a range.
- Support keyboard entry as well as a date/time picker.
- Show exact absolute timestamps in tooltips when relative timestamps such as “3 min ago” are used.

### 8.2 Time field

The Images screen supports these time fields:

| UI label | Database meaning |
| --- | --- |
| Captured | `images.capture_start_at` |
| Discovered | `images.discovered_at` |
| Downloaded | `images.downloaded_at` |
| Classified | latest matching `classifications.request_completed_at` |

Captured is the default. Overview and camera cursor lag always use capture/search-window time and must not inherit another Images time-field selection.

### 8.3 Camera selector

The camera filter is a searchable multi-select. Each option shows camera name, channel, and enabled state, for example `Back garden · Ch 3` or `Driveway · Ch 2 · inactive`. “All cameras” includes inactive cameras with historical data. The filter uses immutable database camera IDs rather than names or track IDs.

Selected cameras appear as removable chips. A “Select active” shortcut selects all currently enabled cameras.

## 9. Overview screen

The Overview answers “Is it working?”, “Is it caught up?”, and “What has it found?”

### 9.1 Summary cards

For the selected capture interval and cameras, show:

- Images discovered
- Images downloaded
- Classifications completed
- Wildlife found
- Interesting images
- Retryable failures
- Permanent failures

Counts must be computed with the same filter predicates used by the image list. The card subtitle states the selected range, such as “Last 24 hours · 3 cameras.” Clicking a card navigates to Images with the equivalent filters.

### 9.2 Live pipeline strip

Show one status tile for Downloader and one for Classifier:

- State: active, idle, retrying, degraded, stopped, or unknown.
- Number of active workers/items.
- Queue depth.
- Oldest queued item age.
- Last successful poll/pass.
- Last heartbeat.
- Last sanitized error, if present.

State derivation is specified in section 16.

### 9.3 Activity chart

Display a stacked time histogram of discovered, downloaded, and classified images. Choose bucket size according to range:

| Range | Bucket |
| --- | --- |
| up to 6 hours | 5 minutes |
| over 6 hours through 48 hours | 1 hour |
| over 48 hours through 90 days | 1 day |
| over 90 days | 1 week |

The chart has a table equivalent for assistive technology. Clicking a bucket applies that exact time interval to Images.

### 9.4 Recent interesting images

Show the eight newest images where the selected classification has `is_interesting = true`. Each card contains the thumbnail, camera, capture time, leading species label, confidence, and a two-line summary. An empty state says “No interesting images in this range” and offers “Show all downloaded images.”

### 9.5 Camera progress

Each camera row shows:

- Name and channel.
- Active/inactive status.
- Last completed NVR search window end.
- Search lag: `now - last_completed_window_end`.
- Last poll time.
- Last search error.
- Images discovered and classified in the selected range.

Rows are ordered by worst health first and then channel number. Selecting a row filters the Images screen to that camera.

## 10. Images screen

### 10.1 Purpose and default view

The Images screen is the main exploration surface. It defaults to a responsive gallery ordered by capture time descending, with downloaded images and all processing states included.

The top bar contains:

- Result count, stated as exact or “more than N” when counting would be expensive.
- Gallery/table toggle.
- Sort selector.
- Filter button with active-filter count.
- Compact selected-filter chips.

### 10.2 Image filters

In addition to global time and cameras, support:

- Download state: pending, downloading, downloaded, retry wait, unavailable, failed.
- Processing state: new, processing, done, retry wait, failed, missing.
- Classification: any, classified, not classified.
- Contains wildlife: yes/no.
- Interesting: yes/no.
- Species: normalized, case-insensitive species label from `species_json`.
- Minimum overall confidence: 0–100%.
- Model and prompt version.
- Local file: present/missing.
- Text search over camera name and model summary. This is optional for the first milestone if SQLite FTS is not introduced.

Facet counts should reflect the global time/camera predicate and all other active filters, excluding the facet itself. This lets the user understand how many results a selection will produce.

### 10.3 Sorting

Supported sorts are:

- Captured: newest first (default)
- Captured: oldest first
- Confidence: highest first
- Classified: newest first
- Camera, then captured newest

Ties must always end with image ID in the matching direction so pagination is stable.

### 10.4 Gallery card

```text
┌────────────────────────────────┐
│                                │
│          thumbnail             │
│                                │
├────────────────────────────────┤
│ Back garden       21:14:06     │
│ INDIAN PALM SQUIRREL    82%    │
│ A small squirrel moves…       │
│ ● Downloaded   ● Classified  │
└────────────────────────────────┘
```

Each card shows:

- A 4:3 thumbnail using `object-fit: contain`; never crop evidence by default.
- Camera and capture time.
- Wildlife/interesting marker when applicable.
- Top species and confidence.
- At most two lines of summary.
- Compact download and processing states.
- A clear placeholder for pending, unavailable, failed, or locally missing content.

The entire card opens Image detail. Secondary actions available from a menu are “Copy detail link,” “Copy NVR image URL,” and, once resolved, “Copy NVR video URL.” The menu must be keyboard accessible and must not rely on hover.

### 10.5 Table view

The table is optimized for operations and includes thumbnail, captured time, camera, download state, processing state, wildlife, species, confidence, model, and completed time. Columns can be hidden but the choice need only persist in `localStorage`; it is not security-sensitive.

### 10.6 Pagination and loading

Use server-side keyset pagination, not offset pagination. Default page size is 60 gallery items or 100 table rows, with an API maximum of 200. “Load more” appends results and retains scroll position. A virtualized list may be added after measuring real datasets.

The list response includes an opaque `next_cursor`. The cursor encodes the sort tuple and a hash of normalized filters, and is authenticated by the server so it cannot be modified. A cursor used with different filters returns `400 invalid_cursor`.

Thumbnail loading is lazy. The browser should request generated thumbnails rather than full-resolution images in the gallery.

### 10.7 Empty and partial states

Distinct empty states are required:

- No images exist yet: explain that downloader discovery has not produced data.
- No images match: show active filters and a Reset filters action.
- Metadata exists but download is pending: retain the card and show current state.
- The database says downloaded but the local file is absent: show “Local file missing,” not a broken image icon.
- Thumbnail generation failed: allow opening the detail record and retrying the request on reload.

## 11. Image detail screen

The route `/images/:imageId` is the canonical shareable record view.

Desktop layout:

```text
┌───────────────────────────────────────┬──────────────────────────────────┐
│                                       │ Back garden                      │
│                                       │ 21 Jul 2026, 01:14:06 IST        │
│            full local image           │                                  │
│                                       │ Interesting wildlife · 82%       │
│                                       │ Indian palm squirrel             │
│                                       │ “A small squirrel moves …”      │
│                                       │                                  │
│                                       │ [Open NVR image] [Video clip ▾] │
├───────────────────────────────────────┴──────────────────────────────────┤
│ Classification | Processing | NVR & files | Diagnostics                  │
└──────────────────────────────────────────────────────────────────────────┘
```

### 11.1 Image viewer

- Display the full local JPEG without cropping.
- Support zoom, pan, fit-to-screen, 100%, and reset.
- Show native dimensions and file size when known.
- Provide previous/next controls that follow the originating list's filters and sort. If opened directly, previous/next use capture-descending order for the same camera and default 24-hour range is not imposed.
- If the local file is unavailable, keep all metadata and NVR actions usable.

### 11.2 Classification summary

The primary result panel shows:

- Contains wildlife.
- Interesting.
- Species list with per-species confidence when present.
- Overall confidence.
- Full model summary without truncation.
- Uncertainties or other normalized structured fields when present.
- Image-relative animal bounding boxes when present; historical classifications have a null value.
- Model, prompt version, request start/end, and request duration.

If several classification records exist, a selector ordered newest first changes the displayed result. The default is the newest completed classification, not the row with the highest confidence. Never merge fields from different classifications.

If processing is not complete, replace the result with the current state:

- New: “Waiting to be classified.”
- Processing: worker start time, elapsed duration, attempt, and lease expiration.
- Retry wait: sanitized error and next attempt time.
- Failed: sanitized final error and attempt count.
- Missing: explain that the expected local image was not found.

### 11.3 Processing tab

Show discovery, download, and processing as a chronological timeline. Include status, attempts, timestamps, next retry, and lease state. Because the current schema stores only latest state rather than every transition, the first release labels this “Current lifecycle” rather than “Audit history.” A true attempt history requires the optional `work_attempts` addition in section 17.

### 11.4 NVR and files tab

Show these as separate labeled values:

1. **NVR still-image URL** — `canonical_playback_uri`, rendered in full in a selectable, wrapping code field with Copy and Open actions.
2. **NVR-reported URL** — original `playback_uri`, shown only when it differs from the canonical URL; this is useful for diagnostics but should not be the default action.
3. **NVR video playback URI** — the URI returned by an on-demand video-recording search around this capture time.
4. **Local file** — safe display name, byte size, and content endpoint. Do not expose the absolute server filesystem path to ordinary users.

The NVR video section initially shows “Find recording.” Activating it searches the primary-video track associated with the camera over the requested clip interval. Default bounds are 10 seconds before through 20 seconds after `capture_start_at`; configurable deployment defaults and a maximum duration are described in section 15.

When a recording is found, show:

- Requested clip start/end.
- Recording start/end returned by the NVR.
- Full credential-free NVR playback URI.
- Copy NVR video URL.
- Open NVR video URL. For RTSP, label this “Open in video player.”
- View clip, only when a browser-compatible Fauna Scan media adapter is available.
- Download clip, only when a bounded authenticated download adapter is available.

When no recording covers the timestamp, say so explicitly and allow expanding the search interval within the configured maximum. Do not fall back to an unrelated nearby recording without user confirmation.

### 11.5 Diagnostics tab

Show immutable identifiers, track IDs, content/codec type, reported size, status-generation values, and sanitized errors. Raw classifier responses and raw classification JSON are hidden by default and require a separate diagnostics permission if authentication is enabled. Secrets, Authorization headers, and NVR response bodies must never be displayed.

## 12. Scan activity screen

“Scan” in the UI means all Fauna Scan pipeline work, while labels distinguish NVR search, image download, and model classification.

### 12.1 Live work

Show sections for active downloads and active classifications. Each row includes:

- Thumbnail or placeholder.
- Camera and capture time.
- Current operation.
- Attempt number.
- Start time and elapsed duration.
- Lease expiry.
- Worker endpoint model for classification, when durably known.

An active row is based on durable `downloading` or `processing` state. If its lease is expired or the responsible pipeline heartbeat is stale, mark it “stale claim” rather than active.

### 12.2 Queues

Show grouped queue counts and oldest item age:

- Pending download.
- Download retry wait.
- Awaiting classification (`new`).
- Classification retry wait.
- Permanent download failures/unavailable.
- Permanent processing failures/missing.

Selecting a queue opens Images with its exact state filters.

### 12.3 Recent completions and failures

Show a merged reverse-chronological feed derived from `downloaded_at`, `processing_completed_at`, and current failed/retry records. Feed entries link to Image detail. Since the existing schema does not retain old transient errors after success, the first release must not call this a complete event log.

### 12.4 Per-camera search activity

Show camera cursor progress as specified on Overview, plus `next_search_at`. A camera that is backfilling should show its completed window and distance remaining to the settlement boundary. Progress is determinate only when a stable backfill start and target are known; otherwise use the label “Backfilling through <timestamp>” rather than a misleading percentage.

### 12.5 Update behavior

Live state updates through Server-Sent Events (SSE). The server emits invalidation events rather than full records; the client refetches the affected summary or list. Coalesce events over 250 ms to prevent a busy downloader from causing request storms.

If SSE is unavailable, poll visible activity every 5 seconds and inactive tabs every 30 seconds. Pause routine polling while the document is hidden. Show the “Live updates disconnected” banner after two failed reconnect attempts, but keep the last successful data visible with an “as of” timestamp.

## 13. Visual language and accessibility

### 13.1 State colors and labels

Color is supplemental; every state also has text and an icon.

| Meaning | Suggested treatment |
| --- | --- |
| Complete/healthy | green, check icon |
| Active | blue, animated only when reduced-motion is not requested |
| Waiting/idle | neutral gray, clock or pause icon |
| Retry/degraded | amber, retry or warning icon |
| Permanent failure/missing | red, error icon |
| Wildlife/interesting | violet or teal, animal/star icon |

Download and processing badges always include their category, for example “Download: retrying” and “Model: failed,” so identical colors do not create ambiguity.

### 13.2 Accessibility requirements

- Meet WCAG 2.2 AA for color contrast, keyboard navigation, focus visibility, form labels, and status announcements.
- All functionality must be available without a pointer.
- Image alt text should be the model summary only when a completed summary exists; otherwise use a factual label such as “Captured image from Back garden at 21:14.” Do not present uncertain species guesses as definitive alt text.
- New live items should not steal focus. Queue/status changes use a polite ARIA live region with rate limiting.
- Respect `prefers-reduced-motion` and `prefers-color-scheme`.
- Charts require a data-table alternative.
- Dates include a machine-readable `<time datetime="...">` value.

### 13.3 Responsive behavior

- Gallery: 1 column below 480 px, 2 below 768 px, 3–5 on larger screens.
- Tables may horizontally scroll but pin the capture time and camera columns.
- Image detail becomes a single column with actions immediately below the image.
- Filter sheets use native scrolling and retain Apply and Reset controls at the bottom.

## 14. Recommended application architecture

```text
Browser
  │ HTTPS or trusted-LAN HTTP
  ▼
Fauna Scan web module (Axum)
  ├── embedded static UI assets
  ├── JSON read API
  ├── SSE invalidation stream
  ├── safe local image/thumbnail server
  └── NVR clip-link/media adapter
        │
        ├── SQLite read/write repositories
        ├── configured output directory
        └── existing authenticated NVR transport

Existing downloader and scanner pipelines
  └── SQLite durable state + lightweight UI invalidations
```

Recommended backend stack:

- `axum` and `tower-http` for HTTP routing, limits, tracing, and static response support.
- Existing `serde` types for JSON DTOs, with API-specific response types that do not expose database internals.
- Embedded, content-hashed production frontend assets so the release remains a single executable.
- A TypeScript browser application built with a small component framework such as Preact and a query-cache library. The framework is an implementation choice; the API and behavior in this document are normative.

The web server becomes a third supervised task in `fauna-scan run`. Unexpected web-server exit should be treated as a supervised component failure. A new `fauna-scan web` command may serve the UI without starting downloader/scanner workers for maintenance or development.

SQLite remains authoritative. In-process notifications improve latency only; missing a notification must never make the UI incorrect after a refetch or restart.

## 15. NVR link and video-clip design

### 15.1 Still-image link

The detail API returns both stored still URLs, but `canonical_playback_uri` is the primary NVR image link because it follows the service's configured rebasing and host-authorization policy. Before returning it, the server must parse it and reapply the same scheme, credential, host, allowlist, and fragment validation used by the downloader.

The URL must not contain a username, password, session token added by Fauna Scan, or other service secret. “Open” is an ordinary new-window navigation with `noopener,noreferrer`. A direct NVR request may prompt for NVR credentials; Fauna Scan should not attempt to populate that prompt.

### 15.2 Recording lookup

A still-picture URI is not sufficient proof of a video recording URI. To resolve a clip:

1. Load the image and camera.
2. Map the picture track to the persisted `primary_track_id`.
3. Compute requested bounds from capture time and pre/post-roll.
4. Perform a Hikvision Content Management recording search for video on that primary track.
5. Parse all paginated results with the same strict and defensive approach used by image search.
6. Select recordings that overlap the capture timestamp, preferring a recording that fully contains the requested interval, then the record with the greatest overlap.
7. Return the exact NVR-provided playback URI after applying origin/allowlist validation. Do not synthesize a URI if search returns none.
8. Cache successful and “not found” results briefly to protect the NVR, keyed by NVR identity, track, requested interval, and firmware-relevant options.

Hikvision documentation and web SDK examples show recording searches returning a `playbackURI`, commonly an RTSP URI, with recording start and end times. Firmware capabilities differ, so the implementation must be tested against the target NVR and treat playback and download methods as capability-driven. See the official [Hikvision Web SDK 3.2 guide](https://open.hikvision.com/fileserver/resourcedocsonline/Web3.2_%E6%8E%A7%E4%BB%B6%E5%BC%80%E5%8F%91%E5%8C%85%E7%BC%96%E7%A8%8B%E6%8C%87%E5%8D%97_20201102163345.pdf).

### 15.3 Clip playback and download adapters

Expose capabilities separately:

```json
{
  "nvr_playback_uri": true,
  "browser_playback": false,
  "bounded_download": true,
  "media_type": "video/mp4"
}
```

Adapters, in preference order, are:

1. A documented NVR HTTP download/export method that accepts the searched recording URI and bounds.
2. An optional server-side media adapter that reads the NVR RTSP playback URI using server-held credentials and produces a bounded MP4 stream/download.
3. NVR URI only: Copy and Open in an external player remain available, while browser View/Download are disabled with an explanation.

The media adapter must enforce:

- Default pre-roll 10 seconds and post-roll 20 seconds.
- Configurable maximum clip duration, default 120 seconds.
- Maximum two concurrent clip operations, independently configurable.
- Connection, first-byte, idle, and total-duration timeouts.
- Cancellation when the browser disconnects.
- No persistent clip cache by default. If enabled, use a bounded cache with expiry and operator-visible storage limits.
- A fixed allowlist of executable path and arguments if an external tool such as FFmpeg is used; never interpolate a URI into a shell command.
- Revalidation of the playback URI's scheme and host immediately before credentials are sent.
- `Content-Disposition: attachment` with a sanitized filename for downloads.

Example filename:

```text
fauna-scan-back-garden-20260720T154406Z.mp4
```

### 15.4 Clip configuration

Proposed non-secret configuration:

```toml
[web]
enabled = true
listen_address = "127.0.0.1:8787"
public_base_url = "http://127.0.0.1:8787"
trust_proxy_headers = false

[web.clips]
default_pre_roll_seconds = 10
default_post_roll_seconds = 20
maximum_duration_seconds = 120
maximum_concurrent_streams = 2
cache_ttl_seconds = 300
media_adapter = "nvr-uri-only" # or a separately configured supported adapter
```

`listen_address` should default to loopback. Binding to a non-loopback address requires authentication or an explicit insecure-LAN acknowledgement during configuration validation.

## 16. Service and pipeline health semantics

### 16.1 Required heartbeat additions

Existing milestone metadata cannot distinguish a stopped process from an idle one. Add durable heartbeat values for:

- `service_instance_id`
- `service_started_at`
- `service_heartbeat_at`
- `downloader_heartbeat_at`
- `scanner_heartbeat_at`
- `downloader_state`
- `scanner_state`
- latest sanitized pipeline error and its timestamp

A normalized `pipeline_status` table is preferable to many metadata keys:

```sql
CREATE TABLE pipeline_status (
    pipeline            TEXT PRIMARY KEY
        CHECK (pipeline IN ('service', 'downloader', 'scanner')),
    instance_id         TEXT NOT NULL,
    state               TEXT NOT NULL,
    started_at          TEXT,
    heartbeat_at        TEXT NOT NULL,
    last_success_at     TEXT,
    last_error          TEXT,
    last_error_at       TEXT,
    updated_at          TEXT NOT NULL
);
```

Heartbeats should update no more frequently than every 5 seconds to avoid excessive SQLite writes and should be written by each pipeline rather than inferred by the web server.

### 16.2 Derived UI state

With heartbeat interval `H` and the relevant configured poll interval `P`:

- **Active**: heartbeat age `<= max(3H, 15s)` and at least one valid active lease.
- **Retrying**: fresh heartbeat, no active work, and one or more due/future retry items.
- **Idle**: fresh heartbeat, no active work, and no due queue item.
- **Degraded**: fresh heartbeat plus a recent pipeline/cursor error or permanent failures in the selected operational window.
- **Stopped**: service shutdown was recorded cleanly, or heartbeat is stale beyond `max(3H, 2P)`.
- **Unknown**: heartbeat data does not exist, as with a database created by an older version.

Backfill/caught-up is a separate label:

- A camera is caught up when `last_completed_window_end` is at or later than `now - settlement_delay - poll_overlap`, allowing a small tolerance.
- A camera is backfilling when its cursor is valid but older than that threshold and searches are progressing.
- A camera is search-stalled when the cursor has not advanced over multiple expected polls and a fresh downloader heartbeat proves the process is alive.

These thresholds must be returned by the API or derived from server-provided configuration, not hard-coded independently in JavaScript.

## 17. Data model and repository changes

### 17.1 Required query indexes

Add indexes after confirming plans with representative data:

```sql
CREATE INDEX idx_images_capture_id
    ON images(capture_start_at DESC, id DESC);

CREATE INDEX idx_images_camera_capture_id
    ON images(camera_id, capture_start_at DESC, id DESC);

CREATE INDEX idx_images_download_capture
    ON images(download_status, capture_start_at DESC, id DESC);

CREATE INDEX idx_images_processing_capture
    ON images(processing_status, capture_start_at DESC, id DESC);

CREATE INDEX idx_classifications_image_completed
    ON classifications(image_id, request_completed_at DESC, id DESC);

CREATE INDEX idx_classifications_flags_confidence
    ON classifications(contains_wildlife, is_interesting, confidence);
```

Do not add every possible compound index initially. Capture query plans for common filters, use SQLite `ANALYZE`, and add indexes based on measured datasets.

### 17.2 Classification projection

The API repository should select a single classification row per image using a window function ordered by `request_completed_at DESC, id DESC`, unless the request explicitly asks for all classifications. This prevents duplicate image cards if future model/prompt runs create more rows.

Species filtering over JSON text may initially use SQLite JSON1 if guaranteed in the build. For large datasets or inconsistent historical JSON, introduce a normalized table:

```sql
CREATE TABLE classification_species (
    classification_id   INTEGER NOT NULL REFERENCES classifications(id),
    normalized_name     TEXT NOT NULL,
    display_name        TEXT NOT NULL,
    confidence          REAL,
    PRIMARY KEY (classification_id, normalized_name)
);
```

### 17.3 Clip-link cache

An optional bounded cache avoids repeating video searches:

```sql
CREATE TABLE nvr_recording_links (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    image_id            INTEGER NOT NULL REFERENCES images(id),
    requested_start_at  TEXT NOT NULL,
    requested_end_at    TEXT NOT NULL,
    recording_start_at  TEXT,
    recording_end_at    TEXT,
    playback_uri        TEXT,
    result_status       TEXT NOT NULL CHECK (result_status IN ('found', 'not_found')),
    resolved_at         TEXT NOT NULL,
    expires_at          TEXT NOT NULL,
    UNIQUE(image_id, requested_start_at, requested_end_at)
);
```

Expired rows can be replaced lazily. Playback URIs may contain sensitive infrastructure details even without credentials, so do not log them and do not include them in list responses.

### 17.4 Optional attempt history

If a complete operational audit is desired, add a `work_attempts` table populated transactionally during claims and transitions. Until that exists, the UI must only claim to show current state and recent timestamp-derived activity.

## 18. HTTP API specification

All JSON endpoints are under `/api/v1`. Timestamps are RFC 3339 UTC strings. IDs are serialized as JSON integers or opaque strings consistently; the recommendation is decimal strings to avoid future JavaScript integer limits. Boolean query values are `true` or `false`. Unknown query parameters return `400` to catch misspellings.

### 18.1 Endpoints

| Method and path | Purpose |
| --- | --- |
| `GET /api/v1/config` | Public, non-secret UI configuration, timezone, limits, and capabilities |
| `GET /api/v1/health` | Service and pipeline health summary |
| `GET /api/v1/cameras` | Cameras and cursor health |
| `GET /api/v1/overview` | Filtered counters, activity buckets, and recent interesting images |
| `GET /api/v1/images` | Filtered, sorted, cursor-paginated image projection |
| `GET /api/v1/images/:id` | Complete safe image detail and classifications |
| `GET /api/v1/images/:id/content` | Full local JPEG; `?draw-bounding-box=true` draws the newest classification's boxes |
| `GET /api/v1/images/:id/thumbnail` | Generated/cached thumbnail |
| `GET /api/v1/images/:id/recording` | Resolve or read cached NVR recording descriptor |
| `GET /api/v1/images/:id/clip` | Browser-compatible bounded clip stream, if supported |
| `GET /api/v1/images/:id/clip/download` | Bounded attachment, if supported |
| `GET /api/v1/activity` | Active work, queue state, failures, and cursors |
| `GET /api/v1/events` | SSE invalidation stream |

Recording resolution is logically read-only even though it contacts the NVR and may populate a cache. `GET` is acceptable if the operation is idempotent. Use `Cache-Control: no-store` for the descriptor because it contains internal URLs.

### 18.2 `GET /api/v1/images`

Parameters:

```text
from, to, time_field
camera (repeatable)
download_status (repeatable)
processing_status (repeatable)
classified, contains_wildlife, interesting
species (repeatable)
confidence_min
model, prompt_version
local_file
q
sort
limit
cursor
```

Example response:

```json
{
  "data": [
    {
      "id": "4812",
      "captured_at": "2026-07-20T15:44:06Z",
      "capture_end_at": "2026-07-20T15:44:07Z",
      "camera": { "id": "3", "name": "Back garden", "channel": 3 },
      "thumbnail_url": "/api/v1/images/4812/thumbnail?v=20260720T154420Z",
      "download": { "status": "downloaded", "attempts": 1 },
      "processing": { "status": "done", "attempts": 1 },
      "classification": {
        "id": "4771",
        "contains_wildlife": true,
        "interesting": true,
        "summary": "A small squirrel moves along the garden wall.",
        "species": [{ "name": "Indian palm squirrel", "confidence": 0.82 }],
        "confidence": 0.82,
        "model": "vision-model",
        "prompt_version": "wildlife-v1",
        "completed_at": "2026-07-20T15:44:31Z"
      }
    }
  ],
  "page": { "next_cursor": "opaque-value", "has_more": true },
  "meta": { "generated_at": "2026-07-21T09:02:11Z" }
}
```

List responses must not include local paths, NVR URLs, raw model responses, classification JSON, errors containing upstream bodies, or leases not needed for display.

### 18.3 `GET /api/v1/images/:id`

Return:

- Image and camera fields.
- Safe local content URLs and file-presence metadata.
- Download and processing state, timestamps, sanitized errors, retry, and lease information.
- All completed classification projections, newest first.
- Canonical and originally reported credential-free NVR still URLs.
- Recording capability and cached recording descriptor, but do not automatically contact the NVR merely by opening the page.
- Neighbor links when the request supplies a valid list-context token.

### 18.4 `GET /api/v1/images/:id/recording`

Parameters:

```text
pre_roll_seconds=10
post_roll_seconds=20
refresh=false
```

Example successful response:

```json
{
  "status": "found",
  "requested_start_at": "2026-07-20T15:43:56Z",
  "requested_end_at": "2026-07-20T15:44:26Z",
  "recording_start_at": "2026-07-20T15:40:00Z",
  "recording_end_at": "2026-07-20T15:50:00Z",
  "nvr_playback_uri": "rtsp://nvr.example/Streaming/tracks/301/?...",
  "capabilities": {
    "open_external": true,
    "browser_playback": false,
    "download": false
  },
  "resolved_at": "2026-07-21T09:03:00Z"
}
```

`not_found` is a normal `200` result. NVR unavailable, authentication failure, malformed response, and unsupported capability are distinct errors.

### 18.5 Activity SSE

SSE messages contain no secrets or full entities:

```text
event: invalidate
id: 98431
data: {"resources":["health","activity","images"],"at":"2026-07-21T09:04:01Z"}
```

Send a comment heartbeat every 15 seconds. Support `Last-Event-ID` within an in-memory bounded event buffer, but emit a full invalidation after restart or buffer loss. Limit open connections per authenticated principal/IP.

### 18.6 Error format

```json
{
  "error": {
    "code": "recording_not_supported",
    "message": "This NVR did not advertise a supported recording lookup method.",
    "request_id": "01J..."
  }
}
```

Messages are safe for users. Internal causes and upstream bodies go only to appropriately redacted server diagnostics. Expected status codes are 400 invalid input, 401 unauthenticated, 403 forbidden, 404 missing record/file, 409 stale/incompatible state, 422 valid but unsupported request, 429 limit exceeded, 502 invalid upstream response, and 503 NVR/service unavailable.

## 19. Local image and thumbnail serving

The web server must never accept a filesystem path from the browser. For every content request it must:

1. Load the image by ID.
2. Require `download_status = downloaded` and a local path.
3. Canonicalize the configured output directory and file path.
4. Verify the file is a regular file contained within the output directory; reject symlink escapes.
5. Verify or safely sniff JPEG content before serving.
6. Set `X-Content-Type-Options: nosniff` and a fixed `Content-Type: image/jpeg`.

Full content can use `ETag` based on image ID plus file metadata and may use private revalidation caching. Thumbnail cache keys include image key, source metadata, dimensions, and thumbnail encoder version. Generate thumbnails off the async runtime's core threads with a strict pixel/dimension limit to prevent decompression bombs.

## 20. Security and privacy

### 20.1 Deployment posture

The default server binds to `127.0.0.1`. Recommended remote access is through a trusted reverse proxy with TLS and authentication, or through an SSH/VPN tunnel.

Fauna Scan requires built-in authentication on all inner pages and API/media routes. `/login`, its static assets, and `POST /api/v1/auth/login` are public. Credentials live in the `users` table on both backends and are provisioned out of band or through `users list`, `users add USERNAME PASSWORD`, and `users remove USERNAME`. Passwords are salted Argon2id PHC strings. Every authenticated user has access to all pages.

Durable opaque sessions live in `web_sessions`, with only token digests stored. HTTP-only SameSite cookies are renewed on authenticated requests. `web.session_expiry_seconds = 0` is the default and means no server expiry; positive values give new sessions a fixed lifetime. Browser cookie retention limits still apply. `web.secure_cookie = true` must be enabled for HTTPS deployments. User deletion, password-hash changes, and sign-out revoke access. Login throttling, bounded password hashing, and a custom same-origin header on unsafe methods provide abuse and CSRF protection. Reverse-proxy identity headers are not used for built-in authentication.

### 20.2 Authorization roles

Prepare for two roles even if the first release has one user:

- **Viewer**: dashboard, local images, normalized classifications, and credential-free NVR links.
- **Diagnostics**: viewer capabilities plus raw normalized JSON and carefully sanitized operational diagnostics.

Clip streaming may be a separate permission because it consumes NVR bandwidth and server CPU.

### 20.3 Response protections

Set at least:

```text
Content-Security-Policy: default-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'
Referrer-Policy: no-referrer
X-Content-Type-Options: nosniff
Permissions-Policy: camera=(), microphone=(), geolocation=()
```

Use content-hashed external scripts/styles so the CSP does not require `unsafe-inline`. Escape all model-provided text as untrusted text, never HTML. Do not fetch NVR images from the browser for thumbnails; use the already downloaded local file.

### 20.4 Redaction

Never return or log:

- NVR or classifier passwords/API keys.
- Authorization, cookie, or proxy-auth headers.
- URLs containing user information.
- Absolute local file paths in ordinary API responses.
- Raw classifier response bodies in list or activity endpoints.
- NVR playback query values in routine access logs.

Request logs should use route templates such as `/api/v1/images/:id/recording`, not raw query strings.

## 21. Performance and reliability targets

For a database containing 1 million images on the intended host:

- Overview response p95 under 1 second for a 24-hour range.
- First page of Images p95 under 500 ms excluding thumbnail transfer.
- Image detail p95 under 250 ms excluding the optional NVR recording lookup.
- Cached thumbnail response p95 under 100 ms.
- UI usable first render under 2 seconds on a local 100 Mbps network.
- Recording lookup default timeout follows NVR request configuration and shows progress after 1 second.

API queries have bounded page sizes and request timeouts. Expensive exact totals may be replaced with capped/estimated counts if measurement shows they block normal downloader/scanner database work. The web connection pool must use the existing SQLite busy timeout and keep read transactions short.

If the web module fails, downloader and scanner durability is not compromised. Whether supervision restarts the whole process or only the web task is an implementation policy, but a web request must never hold an image processing lease or mutate core work status.

## 22. Configuration validation

Validate before startup:

- Listen address parses and port is nonzero.
- Non-loopback binding has an approved authentication posture.
- `public_base_url` contains no credentials or fragment and matches proxy policy.
- Clip pre/post-roll are nonnegative and their sum does not exceed maximum duration.
- Concurrent stream count and all timeouts are positive and bounded.
- Any external media adapter executable is an absolute regular-file path and uses a supported adapter definition.
- Thumbnail dimensions and cache limits are within safe bounds.
- Trusted proxy CIDRs are valid and `trust_proxy_headers` is false by default.

`check-config` should print enabled/disabled web and clip capabilities without printing secret values.

## 23. Testing strategy

### 23.1 Repository and API tests

- Half-open time filtering at exact boundaries.
- Multi-camera filtering including inactive cameras.
- Every download and processing state.
- Latest-classification projection with multiple model/prompt rows.
- Species/confidence filtering.
- Stable keyset pagination with identical timestamps and concurrent inserts.
- Cursor tamper and filter-mismatch rejection.
- Correct aggregation buckets across timezone and daylight-saving transitions.
- API DTOs omit local paths, secrets, and raw response data.

### 23.2 Filesystem/media tests

- Missing local file, wrong type, invalid JPEG, symlink escape, and replaced-file race.
- Thumbnail dimension and decompression limits.
- Range/cancellation behavior for media adapters.
- Clip duration and concurrency enforcement.
- Sanitized attachment filenames.

### 23.3 NVR tests

Use a mock NVR for recording search pagination, no-match, overlapping records, malformed XML, Digest authentication, redirects, cross-origin playback URIs, allowlisted hosts, timeout, retry, and credential redaction. Add an opt-in hardware acceptance test against the target NVR because firmware behavior and video download support vary.

### 23.4 Browser tests

- Filter changes update the URL and Back/Forward restores state.
- Keyboard-only gallery, filter sheet, menus, and image viewer.
- Responsive layouts at representative widths.
- Empty, loading, reconnecting, stale, and error states.
- SSE invalidation and polling fallback.
- Axe or equivalent automated accessibility checks plus manual screen-reader review.
- NVR links use safe new-window attributes and contain no injected credentials.

### 23.5 Load and coexistence tests

Seed at least 1 million image rows and representative classifications. Measure UI queries while downloader and multiple classifier workers transition rows. Confirm web reads do not cause lease loss, busy-timeout cascades, or material processing slowdown.

## 24. Delivery plan

### Milestone 1: Read-only image explorer

- Web server, embedded shell, loopback-only default.
- Cameras, Images gallery/table, Image detail.
- Global time/camera filters and cursor pagination.
- Safe local image and thumbnail serving.
- Classification summary and lifecycle state.
- Full canonical NVR still-image link.

### Milestone 2: Operational monitoring

- Pipeline heartbeat persistence and health rules.
- Overview metrics/chart and camera cursor progress.
- Scan activity queues, active claims, failures, and SSE invalidation.
- Performance indexes and large-dataset testing.

### Milestone 3: NVR video link

- Capability check and primary-track video recording search.
- Safe recording selection, link validation, and cache.
- Full NVR playback URI with Copy/Open actions.
- Hardware acceptance tests against the deployment NVR.

### Milestone 4: Browser playback/download, if required

- Implement one validated HTTP or media adapter.
- Bounded view/download routes, concurrency controls, and cancellation.
- Optional bounded clip cache.

### Milestone 5: Hardened remote access

- Reverse-proxy deployment documentation or built-in authentication.
- TLS/auth configuration tests, roles, rate limiting, and security review.

Milestones 1–3 satisfy exploration, monitoring, classification inspection, arbitrary time/camera filtering, and availability of both still and NVR recording links. Milestone 4 is required only if “view or download” must happen inside an ordinary browser rather than through the NVR's/native player's playback URI.

## 25. Acceptance criteria

The feature is accepted when:

1. An operator can select any valid start/end time and any combination of cameras and see only matching images.
2. Filter state is represented in the URL and survives reload and browser navigation.
3. A downloaded image is displayed from local storage without exposing its filesystem path.
4. Pending, active, retrying, failed, unavailable, and missing images remain visible and clearly labeled.
5. The Images screen can filter by download/processing state, wildlife, interestingness, species, and confidence.
6. Image detail displays the full model summary, structured species/confidence, model, prompt version, and request time.
7. Active classifications and downloads show elapsed time and are not reported active after their lease/heartbeat becomes stale.
8. Overview distinguishes healthy-idle, active, retrying, degraded, stopped, and unknown pipelines.
9. Per-camera search cursor and lag are visible, including last error.
10. Every image detail page shows the full validated canonical NVR still-image URL with Copy and Open actions.
11. “Find recording” searches the camera's primary video track around the image timestamp and returns the exact validated NVR playback URI when one exists.
12. The UI clearly distinguishes NVR still URL, NVR video URI, and Fauna Scan clip endpoint.
13. A supported media adapter allows a bounded clip to be viewed or downloaded; otherwise those actions are visibly unavailable while Copy/Open NVR URI remains functional.
14. No NVR/classifier credential, Authorization header, absolute local path, or raw diagnostic body appears in normal API responses, browser storage, HTML, or logs.
15. Keyset pagination does not duplicate or skip stable rows with identical capture times.
16. The UI remains usable when SSE disconnects, the NVR is unavailable, a local file is missing, or the database contains only metadata.
17. Automated accessibility checks pass and all core workflows work by keyboard.
18. Web reads and live updates do not materially disrupt downloader or classifier leases under the target load.

## 26. Open implementation decisions

These decisions require deployment-specific confirmation before Milestones 3–4:

- Exact Hikvision recording-search request and download/export capability supported by the target NVR firmware.
- Whether the NVR playback URI is reachable from operator devices or only from the Fauna Scan host.
- Whether users expect browser-native clip playback/download or whether opening the NVR URI in a native player is sufficient.
- Reverse-proxy authentication versus built-in authentication.
- Expected image count, retention duration, and peak download/classification rate for final performance sizing.
- Whether raw model JSON should be available to a diagnostics role.

None of these decisions blocks Milestone 1. Recording URL generation must remain capability-driven so later answers do not require changing the core image-detail contract.

## 27. Implementation notes

The browser application lives in `web/` and uses React, TypeScript, Vite, Tailwind,
TanStack Query, Luxon, Lucide, and react-zoom-pan-pinch. Run `npm ci` and
`npm run build` there before building Rust. `build.rs` embeds the content-hashed
assets, and Axum serves application routes with the same response protections as
the API. Docker and `just build`/`just release` build both parts.

The existing API's integer IDs and flat download/processing list fields remain
compatible. Repeated camera, species, and lifecycle filters are accepted alongside
comma-separated values. Explorer queries, sorts, latest-classification predicates,
and capture-bucket aggregation work with both SQLite and PostgreSQL. Additional
read endpoints provide `/api/v1/images/facets` and
`/api/v1/images/:id/neighbors`. Facet counts currently cover download and processing
states; counts are omitted with a filesystem-presence filter to avoid implying
that database metadata establishes file presence. Species labels use exact,
case-insensitive trimmed matching.

Pipeline heartbeats use durable `service_metadata` entries rather than a new
`pipeline_status` table. They are emitted by each continuously running pipeline
and distinguish unknown, idle, active, retrying, degraded, and stopped state.
SSE emits a complete invalidation every five seconds, including after reconnect
or process restart, and has a global connection cap of 32. This deliberately
requires no replay buffer because every message requests a current durable-state
refresh. The browser coalesces invalidations, falls back to polling, and pauses
background traffic when hidden.

Recording resolution remains on demand through the existing authenticated NVR
transport. Search results describe full storage segments; playback URLs are
rebuilt using the camera primary track, requested pre/post-roll interval, configured
NVR host, and `[web].rtsp_port` (554 by default).

The FFmpeg adapter prepares H.264/AAC MP4 clips using the resolved NVR credentials
on the server. `POST /api/v1/images/:id/clip` accepts the same pre/post-roll query
parameters as recording lookup and returns temporary playback/download URLs.
`GET /api/v1/clips/:token` supports byte ranges; `?download=true` adds attachment
disposition. This replaces the planned per-image GET clip/download routes above.
Preparation has a concurrency semaphore, two codec threads, duration and
wall-clock limits, and a 64 MiB file cap. Eight clips at most are cached in private
temporary directories for 15 minutes; expired clips require preparation again.
No credential-bearing input URI or FFmpeg stderr is logged or returned to clients.

Real NVR firmware validation and the million-row performance target require
deployment-specific acceptance testing.
