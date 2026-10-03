# Fauna Scan

Fauna Scan is a Linux Rust service that discovers Hikvision cameras, searches
NVR picture metadata, downloads new JPEGs into a backend-neutral durable state
(SQLite or PostgreSQL), and classifies them through one or more
OpenAI-compatible vision API endpoints.

## Privacy and security

Fauna Scan processes surveillance data. It persists camera metadata, NVR
image URLs, downloaded JPEGs, and classification results in its configured
database and output directory. When classification is enabled, each image sent
for classification is transmitted to the configured OpenAI-compatible endpoint.

Classifications include `bounding_boxes`: one image-relative box per visible
animal (including domestic animals), with `x_min`, `y_min`, `x_max`, and `y_max`
in the range `0.0` to `1.0`. The origin is the image's top-left corner. An
image without animals has an empty array. Validated boxes are stored in
`classifications.bounding_boxes_json` and included in `classification_json`;
the image detail API returns them as `classifications[].bounding_boxes`.
Historical classifications return `null` for boxes; new classifications with
no animals return `[]`. Existing `prompt_version` settings
should be changed to `wildlife-v2` for new classifications using this contract;
completed images are not automatically classified again.

Request `/api/v1/images/IMAGE_ID/content?draw-bounding-box=true` to draw the
newest classification's boxes on the returned JPEG. The original JPEG is
returned when there are no boxes. Set `[web].bounding_box_color` to a `#RRGGBB`
color (default `#FFFF00`, yellow) and `[web].bounding_box_width_pixels` to a
stroke width from 1 to 64 (default 3). These settings only affect rendered
responses; stored images are unchanged. Use
`[web].max_concurrent_bounding_box_renders` to limit simultaneous annotated
JPEG renders (1 to 16, default 2). Requests above the limit wait before the
image file is read.
Only configure providers and storage locations you trust, and ensure that your
use of camera footage complies with applicable consent, privacy, and retention
requirements.

The web API has no built-in authentication. Keep its listener on loopback,
as in the default configuration, or put it behind an authenticated TLS reverse
proxy. Protect the configuration and secret files, database, and image output
directory from unauthorized access.

## Data backend

Fauna Scan stores all durable state behind a backend-neutral abstraction.
The default backend is SQLite; PostgreSQL is available for deployments that
require a shared database server.

### SQLite (default)

SQLite stores state in a single file. The default path is
`${XDG_STATE_HOME:-$HOME/.local/state}/fauna-scan/fauna-scan.sqlite3`.  No
external server is required.

### PostgreSQL

PostgreSQL stores state in a remote database server. Configure the backend
with `config.postgres.example.toml` as a starting point.

#### Secure URL configuration

PostgreSQL credentials must never appear in logs, diagnostics, or committed
configuration files. Choose exactly one secure source:

| Source        | TOML field     | Example                                    |
|---------------|----------------|--------------------------------------------|
| Environment   | `url_env`      | `url_env = "FAUNA_SCAN_POSTGRES_URL"`      |
| File          | `url_file`     | `url_file = "/run/secrets/postgres_url"`   |
| Literal       | `url`          | Not recommended for production             |

The connection URL must use the `postgres` or `postgresql` scheme and include
a host and database name (e.g. `postgres://user@host:5432/fauna_scan`).

#### Backend switching

Switching between SQLite and PostgreSQL does **not** transfer existing data.
Each backend maintains its own independent state. When you change the backend,
Fauna Scan starts fresh on the selected backend. Back up the PostgreSQL
database directly (e.g. with `pg_dump`) instead of copying SQLite files.

Database migrations run automatically on first connect for the selected backend.
Always back up your database before upgrading.

## Install and release verification

Build the frontend with Node.js 22.12+ (Node.js 24 is used in Docker), then
build with the current stable Rust toolchain:

```bash
npm ci --prefix web
npm run format:check --prefix web
npm run build --prefix web
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --no-fail-fast
cargo build
cargo build --release
install -Dm755 target/release/fauna-scan ~/.local/bin/fauna-scan
```

Copy `config.example.toml` to `${XDG_CONFIG_HOME:-$HOME/.config}/fauna-scan/config.toml`.
Persistent state defaults to `${XDG_STATE_HOME:-$HOME/.local/state}/fauna-scan/`;
cache is conventionally `${XDG_CACHE_HOME:-$HOME/.cache}/fauna-scan/`. The image
output directory is explicit and may be outside state.

Replace every `/home/user` placeholder with the real home and NVR/classifier
values. Create empty secret files securely, then populate them with a secure editor (do
not place the values in shell history):

```bash
mkdir -p ~/.config/fauna-scan ~/.local/state/fauna-scan ~/Pictures/fauna-scan
install -m 600 /dev/null ~/.config/fauna-scan/nvr-password
install -m 600 /dev/null ~/.config/fauna-scan/classifier-api-key
${EDITOR:-vi} ~/.config/fauna-scan/nvr-password
${EDITOR:-vi} ~/.config/fauna-scan/classifier-api-key
```

Verify that the files remain private with `stat -c '%a %n'
~/.config/fauna-scan/nvr-password ~/.config/fauna-scan/classifier-api-key` (the
mode should be `600`).

For each password or API key choose exactly one literal, file, or environment
source. File secrets have one trailing newline removed. Do not put credentials
in URLs. `check-config` validates paths, timestamps, TLS policy, lease values,
and secret readability without contacting external services:

```bash
fauna-scan --config ~/.config/fauna-scan/config.toml check-config
```

Use `scheme = "https"` and leave invalid TLS certificates disabled unless the
operator explicitly understands the risk. `start_at` accepts RFC 3339 and is
converted to UTC. Playback URLs are rebased to the configured NVR origin by
default; if rebasing is disabled, use the explicit playback host allowlist.

### Capture-time filtering

To consider only images captured during a daily time window, configure an
optional `[nvr.search.capture_time_window]` table:

```toml
[nvr.search.capture_time_window]
start_time = "19:00"
end_time = "07:00"
utc_offset = "+05:30"
```

Times use 24-hour `HH:MM` notation and are evaluated using the explicit fixed
UTC offset for the NVR's clock (`Z` is also accepted). The start is inclusive,
the end is exclusive, and an end earlier than the start crosses midnight. Thus
this example accepts captures from 7 PM through 6:59:59 AM and ignores all
other search results before they are persisted or downloaded. Omit the table
to retain the existing behavior of accepting images at all times.
Classifier Basic authentication uses `username` plus `password_file`; API-key
and Basic authentication may be combined. Configure classifier servers with
one or more `[[classifier.endpoints]]` tables. Each endpoint requires
`base_url` and `model`, and runs one concurrent worker. Set an endpoint's
`enabled = false` to remove it from request rotation without removing its
configuration. Endpoint settings do not inherit from other endpoints;
credentials, timeouts, prompts, and generation settings are resolved
independently. Images are claimed atomically and distributed among enabled
workers. An empty endpoint list disables classification. On HTTP 429, Fauna
Scan durably cools down the affected provider quota group (when `rate_limit`
is configured) or otherwise that endpoint/model, honoring a numeric
`Retry-After` header when supplied and using the configured retry backoff as a
fallback. Other classifier endpoints continue to process work during that
cooldown.

## Commands

```bash
fauna-scan --config PATH run
fauna-scan --config PATH web
fauna-scan --config PATH discover
fauna-scan --config PATH download --once
fauna-scan --config PATH scan --once
fauna-scan --config PATH status
fauna-scan --config PATH check-config
fauna-scan --log-level debug --config PATH run
fauna-scan --help
fauna-scan --version
```

Set `[web].enabled = true` to serve the interface and API with `run` at the
configured address (the example uses `http://127.0.0.1:8787`). Open that address
in a browser for Overview, Images, Scan activity, and About. The `web` command
serves the same interface against the durable database without starting workers.
The production HTML, CSS, and JavaScript are embedded in the Rust executable;
rebuild the frontend **before** rebuilding Rust whenever frontend source changes.
`just build` and `just release` do this automatically, as does the Docker build.
A Rust-only build without frontend assets still provides the API and explains
how to build the missing interface.

Images supports shareable UTC time/camera filters, gallery/table views, species,
confidence, model/prompt, lifecycle, text and local-file filters, and signed keyset
pagination. Image detail preserves classification provenance and older results,
provides zoom/pan controls and bounding-box overlays, and resolves NVR recordings
only when **Find recording** is selected. This prepares a bounded MP4 for the
embedded player and **Download video** using server-side FFmpeg and the existing
`[nvr]` username/password secret. Credentials are never returned to the browser
or included in FFmpeg logs. Use the raw NVR password in the configuration; the
adapter URL-encodes it when authenticating RTSP.

`[web].rtsp_port` defaults to 554, independently of the NVR ISAPI HTTP port.
The returned RTSP URL uses the requested pre/post-roll times rather than the
entire storage segment. Browser clips require FFmpeg with `libx264` and AAC
support (included in the Docker image). For native installations, install FFmpeg
and optionally set `[web].ffmpeg_path`. Set `clips_enabled = false` to disable
preparation. Availability is detected at web-server startup.

MP4 clips are transcoded to H.264/AAC, with video scaled to at most 1920 pixels
wide, so H.265 and other NVR codecs can play in browsers. Preparation is limited
to `max_concurrent_clip_encodes` jobs (default 1, allowed 1–4), two codec threads,
the configured maximum duration, a wall-clock timeout, and 64 MiB per clip.
At most eight prepared clips are retained in private temporary directories,
expire after 15 minutes, and support HTTP byte ranges for seeking. A fresh
preparation is required after expiry or server restart. A capture near a gap or
recording boundary may have a shorter clip if the NVR lacks the full interval.

`POST /api/v1/images/{id}/clip?pre_roll_seconds=10&post_roll_seconds=20`
prepares a clip and returns temporary `playback_url` and `download_url` values.
Playback and download share the same prepared MP4.

Live pages use SSE invalidations with polling fallback and retain their last
successful data during connection failures. Continuous downloader/scanner workers
persist heartbeats every five seconds. Databases without heartbeat metadata show
unknown pipeline health; expired leases and stale heartbeats never count as active.
Current pipeline state covers all dates/cameras, while summary cards and activity
queues use the selected capture interval.

For frontend development, run the backend in one terminal and Vite in another:

```bash
fauna-scan --config PATH web
npm ci --prefix web
npm run dev --prefix web
```

Vite proxies `/api` to `http://127.0.0.1:8787`; set `FAUNA_API_URL` to override that
address. `npm test --prefix web` builds production assets and runs Chromium
workflows, responsive checks, and Axe accessibility checks. It uses
`/usr/bin/chromium` when available, `CHROMIUM_PATH` when supplied, or Playwright's
installed Chromium otherwise (`cd web && npx playwright install chromium`).
`npm run format --prefix web` applies Prettier plus the project's required blank
lines after imports, between functions, and between interface blocks;
`npm run format:check --prefix web` checks both rules.

The default listener is loopback-only. Keep it on loopback or place it behind an
authenticated TLS reverse proxy; the service does not provide built-in user
authentication. Thumbnail rendering shares the configured render semaphore with
bounding-box overlays, limits decoding to 32 MP/128 MiB, and maintains a bounded
64 MiB / 512-entry memory cache. Local-file filtering checks actual safe filesystem
presence; sparsely matching pages may offer another batch to scan.

`run` supervises downloader and scanner pipelines. `download --once` performs
currently due discovery/search/download work; `scan --once` drains eligible
classifications. Configuration and migration errors stop startup. Temporary
network or classifier failures remain durable for retry, and expired leases are
recovered on restart. Completed downloads are not redownloaded, even if their
local file is later deleted.

### Image retention and garbage collection

Fauna Scan automatically garbage collects locally downloaded images that have
been classified as not containing wildlife. Only images whose classification
processing has completed and that contain no wildlife-positive classification
are eligible. Images with any wildlife-positive classification are preserved
indefinitely.

Retention is controlled by `general.non_wildlife_image_retention_days` in the
configuration file (default: `4`). An image is eligible for collection when
its `capture_start_at` is strictly older than the cutoff (current time minus
retention), its processing status is `done`, and it has at least one
`contains_wildlife = false` classification with no `contains_wildlife = true`
classification. Collection runs as part of every scanner pass, including
`scan --once`.

When an eligible image is collected, its local file is removed and the
`local_path` in the database is cleared to `NULL`. The image row, all
classification records, download status, and completion timestamps are
preserved in SQLite. Collected images remain available as
metadata/classification records through the API but no longer expose content
or thumbnail URLs. The `downloaded` status is unchanged, preserving the
guarantee that collected files are not redownloaded.

## Docker Compose deployment

The Compose deployment pulls `ghcr.io/avranju/fauna-scan:latest` from
the registry. Log in first if the registry requires authentication, then prepare
the container-specific configuration and secret files:

```bash
docker login ghcr.io
cp config.docker.example.toml config.toml
mkdir -p secrets
install -m 600 /dev/null secrets/nvr-password
install -m 600 /dev/null secrets/classifier-api-key
${EDITOR:-vi} config.toml
${EDITOR:-vi} secrets/nvr-password
${EDITOR:-vi} secrets/classifier-api-key
docker compose config
docker compose pull
docker compose up -d
docker compose logs -f fauna-scan
```

With the example Compose configuration, the API is available only on the
Docker host at `http://127.0.0.1:8787`.

The read-only container needs writable temporary storage for video clips.
The Compose example mounts a 1 GiB tmpfs at `/tmp`; include this mount in
custom deployments too. Prepared clips disappear when the container restarts.

Set `[database].path` (SQLite) or `[database].url_env` (PostgreSQL) and
`general.output_directory` to the container paths already used by
`config.docker.example.toml`. Compose persists those paths in the
`fauna-scan-state` and `fauna-scan-images` named volumes. Classifier and
NVR hostnames must be reachable from the container; `localhost` inside the
container refers to Fauna Scan itself.

To deploy a newly published image, run `docker compose pull` followed by
`docker compose up -d`. Compose stops the old container with a 30-second grace
period and reuses the persistent volumes.

## User systemd service

The repository's `fauna-scan.service` is a user-level example. Its
`ProtectHome=read-only` setting means the database and image directory must be
listed in `ReadWritePaths`; adjust those directives to exactly match the
configured paths before installation. For PostgreSQL deployments, no local
database path is needed, but the output directory and any secret files must
still be accessible. Install and manage it with:

```bash
mkdir -p ~/.config/systemd/user
cp fauna-scan.service ~/.config/systemd/user/
systemd-analyze --user verify ~/.config/systemd/user/fauna-scan.service
systemctl --user daemon-reload
systemctl --user enable --now fauna-scan.service
systemctl --user status fauna-scan.service
journalctl --user -u fauna-scan.service -f
systemctl --user restart fauna-scan.service
systemctl --user stop fauna-scan.service
```

The service uses `Restart=on-failure`, a 30-second stop timeout, and logs to
the user journal. A restart reopens/migrates the same SQLite database, resumes
cursors and pending work, and uses uniqueness plus durable statuses to avoid
duplicate downloads or classifications. Stop sends SIGTERM; the service
should log orderly downloader/scanner termination. Enable lingering or ensure a
user manager/login session is available if it must run without an interactive
login.

## Status, upgrades, and troubleshooting

Use `status` for grouped download and processing counts and inspect the journal
for discovery, completed windows, retries, downloads, classifications, and
shutdown details. If startup fails, run `check-config`, verify secret file
permissions and NVR reachability, and confirm the database/output parents are
writable. With hardening enabled, check that every database and output path is
covered by `ReadWritePaths`.

Before an upgrade, stop the service, install the new binary, and start it
again. Migrations run automatically for the selected backend and failures are
reported rather than ignored; retain a backup of the state database
(SQLite file or `pg_dump` for PostgreSQL). The acceptance evidence for this
release is in `docs/acceptance-checklist.md`.
