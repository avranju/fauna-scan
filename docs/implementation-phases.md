# Fauna Scan Implementation Phases

This plan turns the requirements in `docs/spec.md` into independently verifiable implementation phases. The ordering establishes durable state and safety invariants before adding the long-running downloader and scanner pipelines.

## Guiding principles

- SQLite is the coordination boundary between downloading and classification.
- Network and filesystem work must happen outside database write transactions.
- Search, download, and processing work must be restart-safe before continuous loops are introduced.
- Every phase should add tests for its behavior; testing is not deferred to the end.
- Secrets, authentication headers, Base64 image data, and credential-bearing URLs must never be logged.
- Later phases may refine earlier interfaces, but each completed phase should build and have a passing test suite.

## Phase 1: Project foundation and application shell

### Scope

- Create the stable-Rust `fauna-scan` binary crate and initial module layout.
- Add the asynchronous runtime, structured logging, error/context framework, serialization support, time handling, and CLI parser.
- Define the required commands and global options:
  - `run`
  - `check-config`
  - `discover`
  - `download --once`
  - `scan --once`
  - `status`
  - `--config`, `--log-level`, `--version`, and `--help`
- Introduce explicit domain types for camera IDs, track IDs, image keys, timestamps, and state enums.
- Establish a top-level error taxonomy matching the specification.

### Deliverable

A binary that builds on stable Rust, exposes the complete CLI surface, initializes logging, and returns clear “not yet implemented” errors for commands whose internals are not available.

### Verification

- Formatting, lint, build, and basic CLI help/version tests pass.
- Invalid commands and arguments exit non-zero with useful messages.
- Domain state strings round-trip through serialization without ad hoc string handling.

## Phase 2: Configuration, XDG paths, and secret resolution

### Scope

- Implement typed TOML configuration for all sections in the specification.
- Resolve default XDG configuration and state paths.
- Support literal, file, and environment-variable secret sources with exactly one source permitted per secret.
- Trim one trailing line ending from file-backed secrets.
- Validate schemes, hosts, ports, timestamps, positive durations and limits, classifier URLs, and secret readability.
- Separate pure validation from directory creation so `check-config` does not contact either external service.
- Implement safe redaction for configuration debug output and errors.

### Deliverable

`fauna-scan check-config` fully loads, resolves, and validates configuration, reporting actionable errors without revealing secret values.

### Verification

- Tests cover default paths, explicit `--config`, each secret source, conflicting sources, missing secret files, timestamp parsing, numeric bounds, and classifier-disabled behavior.
- Tests assert that representative errors and debug output contain no configured secret.

## Phase 3: SQLite schema and durable state primitives

### Scope

- Add embedded migrations for cameras, images, classifications, search cursors, and service metadata.
- Enable foreign keys, WAL mode where supported, and a busy timeout.
- Implement typed repositories and parameterized queries.
- Enforce stable image-key uniqueness and classification uniqueness for image/model/prompt version.
- Implement short, atomic operations for:
  - Camera upsert and inactive marking.
  - Idempotent image discovery without resetting work state.
  - Search cursor advancement coupled to committed discoveries.
  - Download work claiming and completion/failure transitions.
  - Processing work claiming and classification completion in one transaction.
  - Expired lease recovery.
  - Status counts and service metadata updates.
- Encode valid download and processing transitions explicitly.

### Deliverable

A database layer that owns all state transitions and restart-safety invariants, independent of HTTP clients.

### Verification

- Database tests cover empty migration, duplicate insertion, multiple images at one timestamp, atomic claims, invalid transitions, lease recovery, classification transaction atomicity, and preservation of completed download state after file deletion.
- A failed migration is surfaced and never ignored.

## Phase 4: Shared HTTP infrastructure and Hikvision Digest authentication

### Scope

- Build reusable HTTP clients with connection pooling and configured connect/request timeouts.
- Implement Digest challenge/response authentication for Hikvision requests.
- Enforce TLS certificate verification by default, with only the explicit opt-out supported.
- Define origin comparison and credential-forwarding rules.
- Map transport, timeout, authentication, authorization, and protocol failures into structured error categories.
- Ensure request diagnostics redact authorization data and sensitive query parameters.

### Deliverable

An authenticated NVR transport usable by discovery, search, and download operations without risking credentials being sent to an unrelated origin.

### Verification

- Mock-server tests cover Digest authentication, bad credentials, timeouts, HTTP failures, TLS policy where practical, connection reuse, and cross-host credential refusal.

## Phase 5: Camera discovery

### Scope

- Implement `GET /ISAPI/Streaming/channels`.
- Parse XML by element local name, independent of namespace prefix.
- Group tracks by camera channel, select only primary streams ending in `01`, and derive picture streams ending in `03`.
- Preserve optional camera name and raw discovery identifier.
- Upsert discovered cameras and mark absent known cameras inactive without deleting history.
- Record discovery timestamps and service metadata.
- Implement the `discover` command with a concise human-readable result.

### Deliverable

`fauna-scan discover` authenticates to the NVR, prints correct camera/track mappings, and persists them when invoked as part of operational workflows.

### Verification

- Unit tests cover `101 -> 103`, `301 -> 303`, multiple streams per camera, malformed IDs, unknown XML fields, and default/prefixed namespaces.
- Integration tests cover successful discovery, empty discovery, malformed XML, and authentication failure.

## Phase 6: Image search, pagination, and discovery persistence

### Scope

- Generate half-open UTC search windows from configured `start_at` through a supplied effective end time.
- Serialize Hikvision search XML with a new UUID v4 for every HTTP request and the exact `searchResultPostion` spelling.
- Parse response status, counts, items, track and time spans, media types, playback URI, and metadata descriptors by local XML name.
- Accept response search IDs with or without braces and decode XML entities.
- Filter results to JPEG pictures while retaining valid siblings when one item is malformed.
- Implement pagination using returned result count, with `max_results` only as the documented fallback.
- Detect zero-progress, repeated-position, and repeated-page pagination loops.
- Compute a SHA-256 image key from NVR identity, picture track, UTC capture time, and canonical playback path/query.
- Commit a complete search window's discoveries and cursor advancement transactionally; never advance a failed window.

### Deliverable

A one-window search operation that can safely discover and deduplicate all picture records, including paginated responses and duplicate timestamps.

### Verification

- Unit tests cover UTC conversion, windows, exact XML spelling, per-request UUIDs, namespaces, entity decoding, braced IDs, filtering, page increments, loop detection, URI canonicalization, and stable keys.
- Mock-server tests cover one page, `MORE`, empty results, malformed whole responses, malformed sibling items, declared NVR failure, and timeout.
- Database tests prove cursor atomicity and idempotent replay.

## Phase 7: Image filesystem and download worker

### Scope

- Rebase playback URLs to the configured NVR origin by default while preserving path and query.
- When rebasing is disabled, enforce same-origin or explicit allowlist rules before attaching credentials.
- Generate sanitized camera/date paths and collision-resistant filenames.
- Stream bodies into same-directory `.part` files with a maximum-size guard.
- Validate non-empty JPEG signatures and reject HTML or invalid bodies even after HTTP 200.
- Flush and atomically rename before marking a row downloaded.
- Verify and adopt an existing valid final file; remove or quarantine an invalid one.
- Claim download work with leases, bounded concurrency, classified retry/permanent failures, exponential backoff, and retry limits.
- Clean stale `.part` files and recover expired download leases during startup housekeeping.

### Deliverable

A download worker that safely turns persisted pending image records into validated local JPEGs and durable `downloaded` state.

### Verification

- Unit tests cover rebasing, origin checks, filename sanitization, JPEG validation, size limits, and backoff.
- Integration tests cover normal download, truncated/HTML bodies, host mismatch, HTTP 500 then success, 404/410 policy, interrupted transfer, and existing-file adoption.
- Crash-point tests prove a row is never `downloaded` before the final file exists and that expired claims resume.

## Phase 8: Downloader orchestration, backfill, and polling

### Scope

- Combine discovery, search, cursor persistence, and downloads into a downloader pass.
- On first run, search each active camera from `start_at` toward now minus settlement delay.
- Persist progress per camera and enter continuous mode once historical windows are complete.
- Apply poll overlap without regressing the durable cursor or duplicating images.
- Refresh cameras on schedule and automatically include newly discovered cameras.
- Bound search concurrency across cameras and download concurrency independently.
- Implement `download --once` to perform all currently due discovery, search, and download work before exiting.
- Update downloader service metadata and emit required operational summaries.

### Deliverable

A restart-safe downloader pipeline supporting both finite one-pass execution and ongoing historical-backfill/continuous-poll behavior.

### Verification

- Integration tests cover multiple cameras, partial camera failure, settlement delay, failed-window cursor retention, overlap deduplication, new camera discovery, restart from cursor, and one bad download not blocking others.

## Phase 9: Classifier client and response validation

### Scope

- Build the configurable OpenAI-compatible chat-completions URL.
- Support no authentication, Bearer API key, Basic authentication, or explicitly configured combined authentication.
- Encode a JPEG as an inline Base64 data URL without logging it.
- Define a versioned system/user prompt and the required classification schema.
- Request structured JSON where compatible.
- Extract classification JSON from a plain body, assistant message content, Markdown fence, or supported structured-output field.
- Strictly validate required booleans, arrays, confidence ranges, and other field types without inventing missing values.
- Retain the normalized classification JSON and raw model response for diagnosis.

### Deliverable

A classifier client that returns either a validated typed result or a clearly categorized retryable/permanent error.

### Verification

- Unit tests cover request shape, all response wrappers, fenced JSON, missing fields, wrong types, out-of-range confidence, empty/truncated responses, and authentication-header redaction.
- Mock-server tests cover valid JSON, malformed output, timeout, and transient HTTP failure.

## Phase 10: Scanner pipeline

### Scope

- Atomically claim one downloaded/new image at a time and assign a processing lease.
- Check and validate the local file before classification.
- Mark absent files `missing` without changing download status or triggering re-download.
- Submit images sequentially, persist retry state with exponential backoff, and move exhausted attempts to `failed` while retaining diagnostic response data.
- Commit the successful classification row and `done` processing state in one transaction.
- Recover expired processing leases at startup and during maintenance.
- Implement `scan --once` to drain all currently eligible work and exit.
- Implement the periodic scanner loop and scanner service metadata.

### Deliverable

A sequential, restart-safe scanner that never marks an image done before its structured classification is committed.

### Verification

- Database and integration tests cover exclusive claiming, missing files, transient retries, invalid-response exhaustion, crash recovery, duplicate active classifications, and classification/state atomicity.
- A completed image is not automatically reclassified after restart or prompt/model configuration changes.

## Phase 11: Service lifecycle and operational commands

### Scope

- Implement the specified startup order: CLI, configuration, secrets, validation, directories, database, migrations, recovery, clients, discovery, then pipelines.
- Run downloader and scanner as supervised primary tasks sharing cancellation state only, not work queues.
- Treat unexpected termination of either primary pipeline as fatal: stop its sibling and exit non-zero.
- Handle `SIGINT` and `SIGTERM`, stop new claims, bound in-flight shutdown, and leave leases recoverable.
- Implement `status` counts grouped by download and processing state.
- Add periodic compact operational summaries and all required info-level lifecycle events.
- Audit logs and errors for secret, credential-bearing URL, and Base64 leakage.

### Deliverable

`fauna-scan run` operates both pipelines continuously, shuts down cleanly, and fails visibly if either subsystem dies.

### Verification

- Process-level tests cover startup failure before task launch, graceful signals, fatal child-task propagation, bounded shutdown, recoverable interrupted work, and useful `status` output.
- Log-capture tests check required events and sensitive-data exclusion.

## Phase 12: Packaging, systemd integration, and full acceptance

### Scope

- Add an example configuration and user-level `fauna-scan.service`.
- Document installation, XDG directories, secret files, writable-path adjustments, startup, status, and journal inspection.
- Build the full mock NVR/classifier end-to-end harness.
- Exercise two cameras, more than 50 results, duplicate timestamps, pagination, download, classification, restart idempotency, polling overlap, and one newly added image.
- Run the complete acceptance-criteria checklist and close any behavioral or documentation gaps.

### Deliverable

A single release-ready Linux executable, service example, operator documentation, and automated end-to-end proof of the required behavior.

### Verification

- Stable-Rust build, format, lint, unit, integration, database, and end-to-end suites all pass.
- The service starts under user systemd, survives a restart without duplicate work, processes a newly polled image, and exits cleanly on `SIGTERM`.
- Every item in section 24 of the specification has an automated test, documented manual check, or both.

## Cross-phase dependency map

| Capability | Depends on | First usable in |
| --- | --- | --- |
| Configuration validation | Application shell | Phase 2 |
| Durable claims and recovery | Database primitives | Phase 3 |
| Camera discovery | Configuration, HTTP, database | Phase 5 |
| Search and deduplication | Discovery, HTTP, database | Phase 6 |
| Atomic image download | Search records, filesystem, claims | Phase 7 |
| Continuous downloader | Discovery, search, download | Phase 8 |
| Validated classification | Configuration, HTTP | Phase 9 |
| Restart-safe scanner | Classifier, database claims | Phase 10 |
| Continuous service | Downloader and scanner pipelines | Phase 11 |
| Release acceptance | All runtime capabilities | Phase 12 |

## Suggested milestone boundaries

- **Milestone A — Durable core (Phases 1–3):** CLI, validated configuration, schema, and transactional state machinery.
- **Milestone B — NVR ingestion (Phases 4–8):** authenticated discovery, complete search, safe downloads, backfill, and polling.
- **Milestone C — Classification (Phases 9–10):** validated model integration and sequential restart-safe scanning.
- **Milestone D — Operations and release (Phases 11–12):** supervised lifecycle, systemd packaging, documentation, and end-to-end acceptance.

## Definition of done for every phase

A phase is complete only when:

1. Its public behavior and state transitions are implemented.
2. New failure paths return contextual errors rather than panic.
3. Relevant secrets and large payloads are absent from logs.
4. Unit and integration tests for the phase pass.
5. Formatting and lint checks pass.
6. Any new configuration, command, state, or operator behavior is documented.
