# Fauna Scan: Software Requirements Specification

## 1. Purpose

Fauna Scan is a Linux service that retrieves still images captured by a Hikvision Network Video Recorder, stores them locally, and submits newly downloaded images to a vision-capable large language model for wildlife classification.

The primary goal is to identify potentially interesting wildlife footage from images already selected and retained by the NVR’s internal detection algorithms.

The application shall:

1. Dynamically discover cameras connected to the NVR.
2. Search the NVR for captured images from a configured starting point.
3. Download images that have not previously been downloaded.
4. Persist all discovery, download, and processing state in SQLite.
5. Periodically poll the NVR for new images.
6. Submit newly downloaded images to a vision LLM.
7. Persist structured classification results.
8. Run continuously as a user-level systemd service on Linux.

---

## 2. Technology and Platform Requirements

### 2.1 Programming language

The application shall be implemented in Rust using the current stable Rust toolchain.

### 2.2 Operating environment

The application shall:

* Run on Linux.
* Be distributed as a single executable.
* Be suitable for execution as a user-level systemd service.
* Require no external database server.
* Store state in a local SQLite database.
* Use asynchronous I/O for HTTP operations, timers, signal handling, and database worker coordination.

### 2.3 Application name

The executable and project should be named:

```text
fauna-scan
```

The internal Rust package name may use:

```text
fauna_scan
```

---

## 3. High-Level Architecture

The executable shall contain the following logical modules:

```text
fauna-scan
├── configuration
├── database
├── nvr
│   ├── authentication
│   ├── camera discovery
│   ├── image search
│   └── image download
├── downloader
├── scanner
├── classifier
├── filesystem
├── logging
└── service lifecycle
```

The application shall run two long-lived background pipelines:

### Downloader pipeline

The downloader shall:

1. Discover NVR cameras.
2. Search historical image metadata.
3. Insert discovered images into SQLite.
4. Download pending images.
5. Poll periodically for newly created images.

### Scanner pipeline

The scanner shall:

1. Query SQLite for downloaded images whose processing status is `new`.
2. Submit them sequentially to the configured vision LLM.
3. Validate and persist classification results.
4. Mark successfully processed images as `done`.

The two pipelines shall communicate through SQLite rather than in-memory queues. This allows either pipeline to resume after a process restart without losing work.

---

## 4. Configuration

### 4.1 Configuration format

The primary configuration file shall use TOML.

The default configuration path shall be:

```text
${XDG_CONFIG_HOME:-$HOME/.config}/fauna-scan/config.toml
```

A different path shall be selectable using:

```text
fauna-scan --config /path/to/config.toml run
```

### 4.2 Example configuration

```toml
[general]
database_path = "/home/user/.local/state/fauna-scan/fauna-scan.sqlite3"
output_directory = "/home/user/Pictures/fauna-scan"
log_level = "info"
# Retention for no-wildlife images (days from capture time).
non_wildlife_image_retention_days = 4

[nvr]
scheme = "http"
host = "nvr.example.invalid"
port = 8080
username = "admin"
password_file = "/home/user/.config/fauna-scan/nvr-password"
start_at = "2026-07-11T00:00:00+05:30"
request_timeout_seconds = 30
connect_timeout_seconds = 10
allow_invalid_tls_certificates = false

[nvr.search]
window_minutes = 60
max_results = 50
poll_interval_seconds = 60
poll_overlap_seconds = 120
camera_refresh_interval_seconds = 3600
settlement_delay_seconds = 10

[nvr.download]
retry_limit = 10
retry_initial_delay_seconds = 5
retry_max_delay_seconds = 300
maximum_image_size_bytes = 25000000
verify_jpeg = true
rebase_playback_urls = true

[classifier]
enabled = true
base_url = "http://localhost:8081/v1"
endpoint = "/chat/completions"
model = "vision-model"
api_key_file = "/home/user/.config/fauna-scan/classifier-api-key"
username = ""
password_file = ""
request_timeout_seconds = 120
poll_interval_seconds = 10
retry_limit = 5
retry_initial_delay_seconds = 10
retry_max_delay_seconds = 300
processing_lease_seconds = 600
prompt_version = "wildlife-v2"

[classifier.generation]
temperature = 0.1
max_tokens = 1000
```

### 4.3 Secret handling

Passwords and API keys shall not be required to appear directly in the main configuration file.

The application shall support secrets supplied through:

* A separate file.
* An environment variable.
* A literal configuration value, for development only.

Supported configuration fields should include:

```toml
password = "literal-value"
password_file = "/path/to/file"
password_env = "FAUNA_SCAN_NVR_PASSWORD"
```

Only one source may be configured for a given secret.

If multiple sources are configured, configuration validation shall fail with a clear error.

Secret values shall:

* Never appear in logs.
* Never appear in error messages.
* Never be included in panic output.
* Be trimmed of a single trailing line ending when read from a file.

### 4.4 Configuration validation

The application shall validate all configuration before starting either pipeline.

Validation shall include:

* Valid NVR scheme.
* Non-empty NVR host.
* Port in the valid TCP port range.
* Valid RFC 3339 `start_at` timestamp.
* Positive search window.
* Positive polling intervals.
* Writable output directory.
* Writable database directory.
* Valid classifier URL when classification is enabled.
* Secret file existence and readability.
* `max_results` greater than zero.
* `maximum_image_size_bytes` greater than zero.
* `non_wildlife_image_retention_days` greater than zero and not exceeding
  ~100 years (36 525 days), which is the largest value that can be safely
  subtracted from a `DateTime<Utc>` without underflowing Chrono's bounds.

Invalid configuration shall cause a non-zero process exit before background tasks start.

---

## 5. NVR Authentication

The application shall support HTTP Digest authentication for Hikvision ISAPI requests.

Authentication shall apply to:

* Camera discovery requests.
* Image-search requests.
* Image-download requests.

The HTTP client shall:

* Reuse connections.
* Support Digest authentication challenge and response.
* Apply configured connection and request timeouts.
* Reject invalid TLS certificates by default.
* Support invalid certificates only when explicitly enabled.
* Never automatically send credentials to an unrelated host.

---

## 6. Camera Discovery

### 6.1 Dynamic discovery

The application shall dynamically discover available streaming channels using the Hikvision ISAPI API.

The expected discovery endpoint is:

```text
GET /ISAPI/Streaming/channels
```

The XML response shall be parsed without depending on a fixed XML namespace prefix.

### 6.2 Camera selection

The application shall identify the primary video stream for each camera.

A primary video stream generally has a track identifier ending in `01`, such as:

```text
101
201
301
401
```

The corresponding still-picture track shall end in `03`:

```text
101 -> 103
201 -> 203
301 -> 303
401 -> 403
```

The picture track ID shall be derived by retaining the channel portion and changing the stream suffix to `03`.

For numeric track IDs, the equivalent calculation is:

```text
picture_track_id = floor(primary_track_id / 100) * 100 + 3
```

The implementation shall not derive `103` from every track indiscriminately. It shall first identify the primary stream for each distinct camera channel.

### 6.3 Persisted camera information

Discovered cameras shall be stored in SQLite.

For each camera, store at least:

* Internal database ID.
* Channel number.
* Primary video track ID.
* Picture track ID.
* Camera name, when available.
* Enabled status.
* First discovery time.
* Most recent discovery time.
* Raw discovery identifier, when supplied by the NVR.

### 6.4 Discovery refresh

Camera discovery shall run:

* At application startup.
* Periodically according to `camera_refresh_interval_seconds`.

Newly discovered cameras shall automatically be included in subsequent searches.

Previously known cameras that disappear shall not be deleted from the database. They shall be marked inactive so historical records retain their camera association.

---

## 7. Image Search

### 7.1 Search endpoint

The application shall search for pictures using:

```text
POST /ISAPI/ContentMgmt/search
```

The request body shall use XML with content equivalent to:

```xml
<?xml version="1.0" encoding="utf-8"?>
<CMSearchDescription>
  <searchID>GENERATED-UUID-V4</searchID>
  <trackList>
    <trackID>103</trackID>
  </trackList>
  <timeSpanList>
    <timeSpan>
      <startTime>2026-07-11T02:00:00Z</startTime>
      <endTime>2026-07-11T03:00:00Z</endTime>
    </timeSpan>
  </timeSpanList>
  <contentTypeList>
    <contentType>metadata</contentType>
  </contentTypeList>
  <maxResults>50</maxResults>
  <searchResultPostion>0</searchResultPostion>
  <metadataList>
    <metadataDescriptor>//recordType.meta.std-cgi.com/allPic</metadataDescriptor>
  </metadataList>
</CMSearchDescription>
```

The misspelled element name:

```xml
<searchResultPostion>
```

shall be emitted exactly as required by the NVR API. It shall not be “corrected” to `searchResultPosition`.

### 7.2 Search identifiers

Every HTTP search request shall use a newly generated UUID version 4 value in `searchID`.

This includes subsequent pagination requests.

The implementation shall accept search IDs in responses with or without surrounding braces.

### 7.3 Time format

Search times shall be sent to the NVR as whole-second UTC RFC 3339 timestamps using the `Z` suffix. Fractional seconds are not accepted by the NVR:

```text
2026-07-11T02:00:00Z
```

The configured `start_at` value may contain an explicit local offset. It shall be converted to UTC internally.

All timestamps stored in SQLite shall be normalized to UTC.

### 7.4 Search windows

The application shall divide the requested time range into configurable windows.

The default window shall be 60 minutes.

For example:

```text
2026-07-11T00:00:00Z to 2026-07-11T01:00:00Z
2026-07-11T01:00:00Z to 2026-07-11T02:00:00Z
2026-07-11T02:00:00Z to 2026-07-11T03:00:00Z
```

Search windows shall be treated as half-open intervals where practical:

```text
[start, end)
```

The database deduplication rules shall remain the ultimate protection against overlap or ambiguous NVR boundary behavior.

A failed window shall not cause the search cursor to advance beyond that window.

### 7.5 Search response parsing

The parser shall extract at least:

* `responseStatus`
* `responseStatusStrg`
* `numOfMatches`
* Every `searchMatchItem`
* `trackID`
* `timeSpan/startTime`
* `timeSpan/endTime`
* `contentType`
* `codecType`
* `playbackURI`
* Metadata descriptors

The parser shall:

* Handle the default Hikvision XML namespace.
* Match XML elements by local name rather than requiring a particular namespace prefix.
* Decode XML entities such as `&amp;`.
* Ignore unknown elements.
* Return an actionable error for malformed required fields.
* Avoid failing because a future firmware version includes additional fields.

### 7.6 Result filtering

Only search results representing pictures shall be added to the download queue.

Expected values are:

```xml
<contentType>picture</contentType>
<codecType>jpeg</codecType>
```

Unexpected media types shall be logged at debug or warning level and skipped.

### 7.7 Pagination

The initial request for a search window shall use:

```xml
<searchResultPostion>0</searchResultPostion>
```

When:

```xml
<responseStatusStrg>MORE</responseStatusStrg>
```

is returned, the application shall request the next page.

The next position shall be calculated as:

```text
next_position = current_position + number_of_results_returned
```

If the NVR reports an unreliable result count, the configured `max_results` may be used as a fallback increment.

The implementation shall not merely increment the position by one.

Pagination shall continue until the response no longer reports `MORE`.

The implementation shall detect and abort a non-progressing pagination loop, including cases where:

* `MORE` is returned with zero results.
* The same position repeatedly produces the same page.
* The calculated position does not increase.
* The same page signature is observed repeatedly.

Such a condition shall be recorded as a retryable search error.

### 7.8 Search status handling

The application shall distinguish:

* Successful response with results.
* Successful response with no results.
* Successful response with more pages.
* NVR-declared failure.
* Invalid or incomplete response.
* HTTP or authentication failure.

An empty valid result shall not be treated as an application error.

---

## 8. Image Identity and Deduplication

### 8.1 Duplicate timestamps

Image identity shall not be based only on camera and timestamp.

The NVR can produce multiple images:

* At the same second.
* On the same track.
* With different names or playback URLs.

All genuinely distinct images shall be retained.

### 8.2 Stable image key

Each discovered image shall have a stable unique key derived from:

* NVR identity or configured NVR origin.
* Picture track ID.
* Capture start time.
* Canonical playback URI path and query string.

The preferred unique value is a SHA-256 digest of a canonical representation such as:

```text
<nvr-id>\n<track-id>\n<capture-time>\n<canonical-playback-path-and-query>
```

The database shall enforce uniqueness on this key.

The original playback URI shall also be stored.

### 8.3 Discovery idempotency

Inserting a previously discovered image shall:

* Not create a second row.
* Not reset its download state.
* Not reset its classification state.
* Optionally update non-state metadata if the NVR returned more complete information.

---

## 9. Playback URL Handling

The `playbackURI` returned by the NVR may include:

* An absolute URL.
* An NVR-local hostname.
* A hostname different from the configured connection name.
* XML-escaped query delimiters.

By default, the downloader shall:

1. Parse the returned URI.
2. Preserve its path and query string.
3. Replace its scheme, hostname, and port with the configured NVR origin.
4. Authenticate the resulting request using the configured NVR credentials.

For example:

```text
Returned:
http://nvr.example.invalid:8080/picture/Streaming/tracks/103/?starttime=...

Configured NVR:
http://192.168.1.50:8080

Downloaded from:
http://192.168.1.50:8080/picture/Streaming/tracks/103/?starttime=...
```

This behavior shall be controlled by:

```toml
rebase_playback_urls = true
```

When rebasing is disabled, the application shall refuse to send NVR credentials to a playback host that does not match the configured NVR host unless an explicit host allowlist permits it.

---

## 10. Database

### 10.1 Database engine

SQLite shall be used for persistent state.

The application shall:

* Enable foreign-key enforcement.
* Use WAL mode where supported.
* Set a reasonable busy timeout.
* Use transactions for state transitions.
* Apply schema migrations automatically.
* Never silently discard a database migration failure.

### 10.2 Camera table

The database shall contain a `cameras` table with fields equivalent to:

```text
id
channel_number
primary_track_id
picture_track_id
name
enabled
first_seen_at
last_seen_at
created_at
updated_at
```

`picture_track_id` shall be unique within an NVR instance.

### 10.3 Images table

The database shall contain an `images` table with fields equivalent to:

```text
id
image_key
camera_id
track_id
capture_start_at
capture_end_at
playback_uri
canonical_playback_uri
codec_type
content_type
nvr_reported_size
local_path

download_status
download_attempts
downloaded_at
download_last_error
download_next_attempt_at
download_lease_until

processing_status
processing_attempts
processing_started_at
processing_completed_at
processing_last_error
processing_next_attempt_at
processing_lease_until

discovered_at
created_at
updated_at
```

Recommended download states:

```text
pending
downloading
downloaded
retry_wait
unavailable
failed
```

Required processing states:

```text
new
processing
done
retry_wait
failed
missing
```

The externally significant successful processing states are `new` and `done`, as requested. Intermediate states exist to make restart and retry behavior reliable.

### 10.4 Classifications table

Classification results shall be stored in a separate `classifications` table so prompt or model changes can later produce additional classifications without destroying history.

Fields shall include:

```text
id
image_id
model
prompt_version
contains_wildlife
is_interesting
summary
species_json
bounding_boxes_json
confidence
classification_json
raw_response
request_started_at
request_completed_at
created_at
```

An image may have multiple historical classification rows, but only one active successful classification for a given:

```text
image_id + model + prompt_version
```

### 10.5 Search cursors

Search progress shall be persisted per camera.

A `search_cursors` table shall contain fields equivalent to:

```text
camera_id
next_search_at
last_completed_window_start
last_completed_window_end
last_poll_at
last_error
updated_at
```

The cursor shall advance only after:

* Every page in the window has been retrieved.
* Every valid result has been committed to SQLite.

The cursor may advance before image bytes are downloaded because pending downloads are independently persisted.

### 10.6 Service metadata

The database shall maintain:

* Schema version.
* Application version last used.
* NVR identity when available.
* Initial backfill completion status.
* Last successful camera discovery.
* Last successful downloader poll.
* Last successful scanner pass.

---

## 11. Image Downloading

### 11.1 Download queue

The downloader shall select records whose download status is:

```text
pending
```

or whose retry time has arrived.

Images already marked `downloaded` shall never be downloaded again automatically.

This rule applies even when the corresponding file has subsequently been deleted from disk.

### 11.2 State transition

Before beginning a download, the application shall atomically change:

```text
pending -> downloading
```

and assign a download lease expiry.

After successful completion:

```text
downloading -> downloaded
```

After a retryable failure:

```text
downloading -> retry_wait
```

After a permanent failure:

```text
downloading -> unavailable
```

or:

```text
downloading -> failed
```

### 11.3 Crash recovery

When the application starts, download records left in `downloading` with an expired lease shall return to a retryable state.

This prevents a process crash from permanently stranding an image.

### 11.4 File naming

Images shall be stored beneath the configured output directory.

The directory structure should be:

```text
<output-directory>/<camera>/<YYYY>/<MM>/<DD>/
```

A filename shall include:

* UTC capture timestamp with subsecond precision when available.
* Track ID.
* A short prefix of the stable image key.

Example:

```text
camera-01/2026/07/11/20260711T002932Z_track-103_a04b77e391f2.jpg
```

The stable key component prevents collisions between images captured at the same timestamp.

Camera names shall be sanitized before use as directory names.

### 11.5 Atomic file creation

Downloads shall be written to a temporary file in the destination directory:

```text
filename.jpg.part
```

After the complete response has been written, validated, and flushed, the temporary file shall be atomically renamed to its final name.

A database row shall not be marked `downloaded` until the final rename has succeeded.

Stale `.part` files may be removed during startup housekeeping.

### 11.6 Download validation

A successful download shall require:

* HTTP success status.
* Body size greater than zero.
* Body size not exceeding the configured maximum.
* JPEG signature validation when enabled.
* Successful write and rename.
* Optional agreement with the NVR-reported size when that value is available and reliable.

An incorrect or HTML response body shall not be saved as a successful JPEG merely because the NVR returned HTTP 200, a trick sufficiently common to deserve explicit prohibition.

### 11.7 Existing files

If the final destination already exists while the database row is not marked downloaded, the application shall verify the file.

When the existing file is a valid non-empty JPEG:

* It may be adopted.
* The database may be marked `downloaded`.

When it is invalid:

* It shall be renamed or removed.
* The image shall be downloaded normally.

### 11.8 Retry policy

Retryable failures include:

* Connection timeout.
* Temporary DNS error.
* HTTP 408.
* HTTP 429.
* HTTP 500-series response.
* Interrupted body transfer.
* Temporary SQLite busy error.

Permanent or long-lived failures include:

* HTTP 404 or 410 after a configurable number of confirmations.
* Invalid playback URI.
* Unsupported media type.
* Repeatedly invalid response body.
* Authentication failure requiring operator intervention.

Retries shall use exponential backoff capped by the configured maximum delay.

One failing image shall not block other images.

---

## 12. Historical Backfill and Continuous Polling

### 12.1 Initial backfill

On first run, the application shall search each discovered camera beginning at:

```text
nvr.start_at
```

and ending near the current time.

The effective end time shall account for:

```toml
settlement_delay_seconds
```

to avoid searching an interval while the NVR may still be finalizing metadata.

### 12.2 Continuous mode

After historical search windows have been processed, the application shall enter continuous polling mode.

At every polling interval it shall:

1. Refresh cameras when their refresh interval has elapsed.
2. Search from the persisted cursor toward the current time.
3. Include a configurable overlap with the previous successful range.
4. Insert newly discovered records.
5. Queue pending downloads.

### 12.3 Poll overlap

Each poll shall overlap the previously searched period by:

```toml
poll_overlap_seconds
```

The overlap protects against images that become visible in the NVR index shortly after their capture time.

Database uniqueness shall prevent repeated downloads.

### 12.4 Restart behavior

After restart, the application shall:

1. Open and migrate the database.
2. Recover expired leases.
3. Rediscover cameras.
4. Resume each camera from its persisted cursor.
5. Resume pending downloads.
6. Resume pending classifications.

It shall not repeat the entire backfill unless the database has been removed or an explicit administrative command resets the cursor.

---

## 13. Scanner

### 13.1 Scanner input

The scanner shall periodically select images satisfying all of the following:

```text
download_status = downloaded
processing_status = new
local_path is not null
```

It shall process images one at a time by default.

### 13.2 Claiming work

Before submitting an image to the classifier, the scanner shall atomically transition:

```text
new -> processing
```

and set a processing lease.

The claim operation shall prevent two scanner tasks or two service instances from processing the same image concurrently.

### 13.3 Missing files

When an image is marked downloaded but the local file no longer exists:

* The downloader shall not automatically redownload it.
* The scanner shall set processing status to `missing`.
* The condition shall be logged.
* The database shall retain the original image and download record.

An explicit administrative reset command may be added later, but automatic re-download is out of scope.

### 13.4 Scanner loop

The scanner loop shall:

1. Claim one eligible image.
2. Load and validate the local image.
3. Build the classifier request.
4. Submit the request.
5. Parse and validate the response.
6. Store the classification in a transaction.
7. Mark the image `done`.
8. Continue with the next image.
9. Sleep for the configured interval when no work is available.

### 13.5 Garbage collection of no-wildlife images

The scanner shall automatically garbage collect locally downloaded images
whose capture time exceeds a configurable retention duration and whose
completed classifications contain no wildlife.

Configuration:

```toml
[general]
non_wildlife_image_retention_days = 4
```

The default retention is 4 days.

Eligibility criteria:

* `processing_status = done`
* `local_path IS NOT NULL`
* `capture_start_at` is strictly older than `now - retention`
* At least one classification with `contains_wildlife = 0`
* No classification with `contains_wildlife = 1`

When an eligible image is collected:

* The local file is removed.
* `local_path` is cleared to `NULL` in the database.
* The image row, classification rows, download status, and timestamps
  are preserved.
* The `downloaded` status is unchanged — the file is not redownloaded.

Images with any wildlife-positive classification are never collected,
regardless of retention.

Collection runs as part of every scanner pass, including `scan --once`.
If shutdown is requested, collection is skipped for that pass.

Per-file filesystem failures (e.g. permission denied, path outside root)
are logged and reported but do not prevent later candidates from being
processed. Failed candidates remain eligible for retry on a later pass.

Database errors during candidate selection or path-clearing fail the
scanner pass, because durable reconciliation cannot be trusted.

Acceptance criteria:

1. Omitting `non_wildlife_image_retention_days` produces a validated 4-day retention.
2. An image older than the cutoff with only negative classifications is collected.
3. An image at exactly the cutoff or newer is not collected.
4. Any image with a positive wildlife classification is never collected.
5. Unclassified, in-progress, or failed images are not collected.
6. Collection removes the file and clears `local_path` while preserving
   the image row, classification rows, and download status.
7. Already-missing eligible files have `local_path` reconciled without
   failing the pass.
8. Paths outside the output directory are never deleted.
9. A filesystem failure does not prevent later candidates from being processed.
10. Continuous scanner passes and `scan --once` both execute collection.

### 13.6 Successful completion

An image shall be marked `done` only after the classification row has been committed successfully.

The classification insert and processing-state update shall occur in the same transaction.

### 13.7 Crash recovery

A row left in `processing` with an expired processing lease shall be returned to a retryable state on startup or during periodic maintenance.

---

## 14. Classifier API

### 14.1 Protocol

The initial implementation shall support an OpenAI-compatible vision chat-completions API.

The endpoint shall be configurable because local inference servers display considerable creativity when deciding exactly where `/v1` belongs.

The default request target shall be assembled from:

```text
classifier.base_url + classifier.endpoint
```

For example:

```text
http://localhost:8081/v1/chat/completions
```

### 14.2 Authentication

The classifier client shall support:

* Bearer API-key authentication.
* HTTP Basic authentication.
* No authentication.
* Both Basic authentication and an API key when explicitly configured.

Example headers:

```text
Authorization: Bearer <api-key>
```

or HTTP Basic authentication using the configured username and password.

Secrets shall never be logged.

### 14.3 Image submission

The image shall be submitted as an image content item supported by an OpenAI-compatible API.

The initial implementation may use an inline Base64 data URL:

```text
data:image/jpeg;base64,...
```

The request shall contain:

* Configured model.
* System instruction.
* User classification instruction.
* Image.
* Low temperature.
* Requested structured JSON output where the server supports it.

### 14.4 Default classification task

The classifier shall determine:

* Whether an animal is present.
* Whether the animal appears to be wildlife.
* Likely species or broad animal type.
* Whether the image is interesting enough to review.
* Confidence.
* A concise description of the scene.
* Any uncertainty or visibility problems.
* One image-relative bounding box for each visible animal, including domestic animals.

Humans, vehicles, vegetation movement, shadows, rain, insects near the lens, and camera artifacts should not be classified as wildlife unless an actual animal is visible.

Domestic animals may be identified but should normally have:

```text
contains_wildlife = false
```

unless the configured prompt defines otherwise.

### 14.5 Required structured response

The classifier shall be instructed to return a JSON object matching this logical schema:

```json
{
  "contains_animal": true,
  "contains_wildlife": true,
  "is_interesting": true,
  "species": [
    {
      "name": "Indian palm squirrel",
      "confidence": 0.82
    }
  ],
  "bounding_boxes": [
    { "x_min": 0.31, "y_min": 0.22, "x_max": 0.58, "y_max": 0.71 }
  ],
  "overall_confidence": 0.82,
  "summary": "A small squirrel is moving along the garden wall.",
  "uncertainties": []
}
```

Requirements:

* Boolean fields shall be actual JSON booleans.
* Confidence values shall be between `0.0` and `1.0`.
* `species` shall be an array.
* `bounding_boxes` shall be an array with one tight box per visible animal, or an empty array when `contains_animal` is false.
* Box coordinates use the full image as the frame, with the top-left corner at `(0, 0)` and the bottom-right corner at `(1, 1)`. Each coordinate shall be within `0.0` and `1.0`; `x_min < x_max` and `y_min < y_max`.
* Unknown species may use a broad label such as `bird`, `snake`, or `small mammal`.
* The response shall not claim a precise species when the image does not support one.
* Additional fields may be retained in `classification_json`.

### 14.6 Response parsing

The application shall support:

* A plain JSON response body.
* JSON contained in the assistant message content.
* JSON surrounded by a Markdown code fence.
* Structured-output fields returned by compatible servers.

The parser shall remove only well-understood wrapping. It shall not attempt to invent missing classification values from arbitrary prose.

The raw model response shall be stored for diagnosis.

### 14.7 Invalid classifier responses

An invalid response includes:

* Non-JSON output.
* Missing required fields.
* Invalid field types.
* Confidence outside the accepted range.
* Empty response.
* Truncated response.

Invalid responses shall be treated as retryable until the configured retry limit is reached.

After the retry limit, processing status shall become `failed`, and the final error and raw response shall be retained.

### 14.8 Prompt versioning

The classifier prompt shall have an explicit version identifier.

The prompt version shall be stored with every classification.

Changing the configured prompt version shall not automatically reprocess already completed images in the first implementation.

---

## 15. Concurrency

### 15.1 Search concurrency

Searches may run concurrently across cameras, but concurrency shall be bounded.

A conservative default should be used to avoid overloading the NVR.

### 15.2 Download concurrency

Image downloads may use bounded concurrency.

The default should be configurable and modest, such as two concurrent downloads.

### 15.3 Classification concurrency

Classification concurrency shall default to one.

The scanner shall therefore submit images one by one as required.

A future configuration option may permit greater concurrency, but sequential processing is the required initial behavior.

### 15.4 SQLite coordination

Database operations shall avoid holding write transactions while performing network I/O.

The expected pattern is:

1. Claim work in a short transaction.
2. Commit.
3. Perform HTTP or filesystem work.
4. Persist the result in another short transaction.

---

## 16. Logging and Observability

### 16.1 Logging format

The application shall log to standard output and standard error so systemd can collect logs in the journal.

Logs shall include:

* Timestamp.
* Severity.
* Module or operation.
* Camera and track identifiers when relevant.
* Image database ID or short image key when relevant.
* Retry attempt.
* Error category.

### 16.2 Log levels

Supported levels shall include:

```text
error
warn
info
debug
trace
```

The configured default shall be `info`.

### 16.3 Required informational events

At `info` level, log:

* Application startup and version.
* Database path.
* Output directory.
* Number of cameras discovered.
* Historical backfill range.
* Completion of a search window.
* Number of new images discovered.
* Successful downloads.
* Successful classifications.
* Entry into continuous polling.
* Graceful shutdown.

### 16.4 Sensitive information

Logs shall not include:

* NVR password.
* Classifier password.
* API key.
* Authorization header.
* Full Base64 image content.
* URLs containing credentials.

Playback query strings may be logged only at debug level and should be redacted when they contain potentially sensitive parameters.

### 16.5 Periodic summaries

The service should periodically log a compact summary containing:

* Cameras active.
* Images discovered.
* Images downloaded.
* Downloads pending.
* Images awaiting classification.
* Classifications completed.
* Retryable failures.
* Permanent failures.

---

## 17. Service Lifecycle

### 17.1 Startup

Startup shall proceed in this order:

1. Parse command-line arguments.
2. Load configuration.
3. Resolve secrets.
4. Validate configuration.
5. Create required directories.
6. Open SQLite.
7. Run migrations.
8. Recover expired work leases.
9. Initialize HTTP clients.
10. Test or perform camera discovery.
11. Start downloader and scanner tasks.

### 17.2 Shutdown

The application shall handle:

* `SIGTERM`
* `SIGINT`

On shutdown it shall:

* Stop claiming new work.
* Cancel or finish in-flight requests within a bounded period.
* Flush database operations.
* Leave incomplete claimed work recoverable through leases.
* Remove no successfully downloaded files.
* Exit cleanly.

### 17.3 Fatal task failure

If either primary pipeline terminates unexpectedly due to an internal error, the application shall:

* Log the failure.
* Signal the other pipeline to stop.
* Exit with a non-zero status.

The process shall not continue indefinitely with one silently dead subsystem.

---

## 18. Command-Line Interface

The executable shall support:

```text
fauna-scan [GLOBAL OPTIONS] <COMMAND>
```

Required commands:

### Run service

```text
fauna-scan run
```

Starts both downloader and scanner loops.

### Validate configuration

```text
fauna-scan check-config
```

Loads and validates configuration without contacting the NVR or classifier.

### Discover cameras

```text
fauna-scan discover
```

Contacts the NVR and prints discovered cameras and derived picture track IDs.

### One downloader pass

```text
fauna-scan download --once
```

Performs currently due discovery, search, and download work, then exits.

### One scanner pass

```text
fauna-scan scan --once
```

Processes eligible images until none remain, then exits.

### Database status

```text
fauna-scan status
```

Prints counts grouped by download and processing state.

Global options shall include:

```text
--config <PATH>
--log-level <LEVEL>
--version
--help
```

---

## 19. User-Level systemd Integration

The repository shall include an example service file:

```ini
[Unit]
Description=Fauna Scan Hikvision wildlife image classifier
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=%h/.local/bin/fauna-scan --config %h/.config/fauna-scan/config.toml run
Restart=on-failure
RestartSec=10
TimeoutStopSec=30

NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=%h/.local/state/fauna-scan
ReadWritePaths=%h/Pictures/fauna-scan

[Install]
WantedBy=default.target
```

The documentation shall explain installation using:

```bash
mkdir -p ~/.config/systemd/user
cp fauna-scan.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now fauna-scan.service
```

Logs shall be viewable using:

```bash
journalctl --user -u fauna-scan.service
```

The service file’s writable paths shall be adjusted to match configured database and output paths.

---

## 20. Filesystem and XDG Behavior

Default locations shall follow XDG conventions.

Configuration:

```text
${XDG_CONFIG_HOME:-$HOME/.config}/fauna-scan/
```

Persistent state:

```text
${XDG_STATE_HOME:-$HOME/.local/state}/fauna-scan/
```

Optional cache or temporary data:

```text
${XDG_CACHE_HOME:-$HOME/.cache}/fauna-scan/
```

The output image directory shall be explicitly configurable and shall not be assumed to be inside the state directory.

Directories shall be created when absent.

The application shall fail clearly when a required directory cannot be created or written.

---

## 21. Error Handling

Errors shall be categorized so retry decisions are explicit.

Recommended categories:

```text
Configuration
Authentication
Authorization
Network
Timeout
Protocol
XmlParsing
InvalidNvrResponse
PlaybackUnavailable
Filesystem
Database
ClassifierTransport
ClassifierResponse
Shutdown
Internal
```

Errors shall retain context, including:

* Operation.
* Camera.
* Track.
* Search window.
* Image identifier.
* HTTP status where applicable.

One malformed search result item shall preferably be skipped and recorded while valid sibling items are retained. A malformed entire response shall fail the current page.

The application shall not panic for expected runtime failures such as:

* NVR offline.
* Classifier offline.
* Invalid individual XML item.
* Missing downloaded file.
* Temporary database contention.
* HTTP timeout.

Panics are acceptable only for genuine internal invariant violations, and even those should be minimized.

---

## 22. Database and State Guarantees

The following invariants shall hold:

1. A successfully downloaded image is never automatically downloaded again.
2. Deleting a downloaded image from disk does not reset its download status.
3. An image is marked downloaded only after its final file exists.
4. An image is marked done only after its classification is committed.
5. Search cursors advance only after discovery records are committed.
6. Multiple images with the same timestamp may coexist.
7. Duplicate search results do not create duplicate database rows.
8. Process restart does not lose pending work.
9. Expired processing or download leases are recoverable.
10. Secrets never enter the database unless the user explicitly places them in a playback URL, which the application should reject or sanitize.

---

## 23. Testing Requirements

### 23.1 Unit tests

Unit tests shall cover:

* Camera track derivation.
* RFC 3339 time conversion.
* Search-window generation.
* XML request serialization.
* Preservation of `searchResultPostion`.
* UUID generation.
* XML parsing with the default Hikvision namespace.
* XML entity decoding.
* Response IDs surrounded by braces.
* Pagination position calculation.
* Pagination loop detection.
* Duplicate timestamps with distinct playback URIs.
* Stable image-key generation.
* Playback URL rebasing.
* Filename sanitization.
* JPEG signature validation.
* Classifier JSON parsing.
* Markdown-fenced classifier JSON.
* Invalid confidence values.
* Retry backoff calculation.
* State transition validation.

### 23.2 Integration tests

Integration tests shall use a mock HTTP server simulating:

* Digest authentication.
* Camera discovery.
* A single-page search.
* A paginated search with `MORE`.
* Empty search results.
* Malformed XML.
* Authentication failure.
* NVR timeout.
* Playback URL using a different hostname.
* JPEG download.
* Truncated JPEG.
* HTTP 500 followed by success.
* Classifier valid JSON response.
* Classifier malformed response.
* Classifier timeout.

### 23.3 Database tests

Database tests shall verify:

* Schema migration from an empty database.
* Duplicate image insertion.
* Atomic work claiming.
* Lease recovery.
* Restart after an interrupted download.
* Restart after an interrupted classification.
* No re-download after local file deletion.
* Classification transaction atomicity.

### 23.4 End-to-end test

An end-to-end test shall:

1. Start mock NVR and classifier services.
2. Discover at least two cameras.
3. Return more than 50 images to exercise pagination.
4. Include two images with the same timestamp.
5. Download all distinct images.
6. Classify each image.
7. Restart Fauna Scan.
8. Verify that no image is downloaded or classified again.
9. Add one new NVR image.
10. Verify that the polling loop downloads and classifies only that image.

---

## 24. Acceptance Criteria

The implementation shall be considered complete when all the following are demonstrated:

1. It builds using stable Rust on Linux.
2. It runs as a user-level systemd service.
3. It dynamically discovers Hikvision camera channels.
4. It correctly maps primary stream `101` to picture track `103`, `301` to `303`, and equivalent channels.
5. It searches each camera from the configured starting timestamp.
6. It handles more than 50 results through pagination.
7. It emits a different UUID v4 search ID for every search request.
8. It uses the exact XML tag `searchResultPostion`.
9. It stores discovered image metadata in SQLite.
10. It retains distinct images sharing a timestamp.
11. It downloads images atomically.
12. It does not redownload an image already marked downloaded.
13. It still does not redownload that image after its file is deleted.
14. It resumes pending work after restart.
15. It continuously polls the NVR after initial backfill.
16. It submits downloaded images sequentially to the classifier.
17. It stores structured classifier results.
18. It marks successfully classified images as `done`.
19. It retries temporary NVR and classifier failures.
20. It recovers work left in progress by a crash.
21. It shuts down cleanly on `SIGTERM`.
22. It logs useful operational information without exposing secrets.
23. Automated tests cover XML parsing, pagination, deduplication, retries, state transitions, and restart behavior.

---

## 25. Out of Scope for the Initial Version

The following are explicitly outside the initial implementation:

* Analysing full video clips. Bounded recording playback/download for image review is supported by the web interface.
* Real-time RTSP stream analysis.
* A graphical user interface.
* A web dashboard.
* Sending notifications.
* Automatically deleting NVR content.
* Re-downloading files deleted from local storage.
* Training or fine-tuning a vision model.
* Correlating multiple images into a single motion event.
* Reclassifying completed images when the model or prompt changes.
* Supporting NVR brands other than Hikvision.
* Managing the NVR’s recording or detection configuration.

The architecture should not unnecessarily prevent these capabilities from being added later.

---

## 26. Implementation Guidance

The codebase should favor:

* Explicit domain types for IDs, timestamps, and statuses.
* Typed configuration deserialization.
* Structured errors with contextual information.
* Streaming HTTP downloads rather than buffering entire images unnecessarily.
* Namespace-tolerant XML parsing.
* Parameterized SQL.
* Database migrations embedded in the executable.
* Short database transactions.
* Bounded concurrency.
* Cancellation-aware asynchronous tasks.
* Dependency injection or traits around NVR and classifier clients to enable testing.

The implementation should avoid:

* Storing operational state only in memory.
* Treating timestamps as unique image identifiers.
* Advancing cursors before committing discovered results.
* Marking work complete before filesystem or database operations succeed.
* Logging request headers containing credentials.
* Unbounded task spawning.
* Blindly trusting playback URI hosts.
* Assuming every successful HTTP response contains the expected XML or JPEG data.
