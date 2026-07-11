-- Fauna Scan initial schema
-- Phase 3: SQLite schema and durable state primitives

-- ── Cameras ────────────────────────────────────────────────────────────────

CREATE TABLE cameras (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_number  INTEGER NOT NULL,
    primary_track_id TEXT    NOT NULL,
    picture_track_id TEXT    NOT NULL UNIQUE,
    name            TEXT,
    raw_discovery_identifier TEXT,
    enabled         INTEGER NOT NULL DEFAULT 1,
    first_seen_at   TEXT    NOT NULL,
    last_seen_at    TEXT    NOT NULL,
    created_at      TEXT    NOT NULL,
    updated_at      TEXT    NOT NULL
);

-- ── Images ────────────────────────────────────────────────────────────────

CREATE TABLE images (
    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
    image_key               TEXT    NOT NULL UNIQUE,
    camera_id               INTEGER NOT NULL REFERENCES cameras(id),
    track_id                TEXT    NOT NULL,
    capture_start_at        TEXT    NOT NULL,
    capture_end_at          TEXT,
    playback_uri            TEXT    NOT NULL,
    canonical_playback_uri  TEXT    NOT NULL,
    codec_type              TEXT,
    content_type            TEXT,
    nvr_reported_size       INTEGER,
    local_path              TEXT,

    download_status         TEXT    NOT NULL DEFAULT 'pending'
        CHECK (download_status IN ('pending', 'downloading', 'downloaded', 'retry_wait', 'unavailable', 'failed')),
    download_attempts       INTEGER NOT NULL DEFAULT 0,
    downloaded_at           TEXT,
    download_last_error     TEXT,
    download_next_attempt_at TEXT,
    download_lease_until    TEXT,

    processing_status       TEXT    NOT NULL DEFAULT 'new'
        CHECK (processing_status IN ('new', 'processing', 'done', 'retry_wait', 'failed', 'missing')),
    processing_attempts     INTEGER NOT NULL DEFAULT 0,
    processing_started_at   TEXT,
    processing_completed_at TEXT,
    processing_last_error   TEXT,
    processing_next_attempt_at TEXT,
    processing_lease_until  TEXT,

    discovered_at           TEXT    NOT NULL,
    created_at              TEXT    NOT NULL,
    updated_at              TEXT    NOT NULL
);

-- ── Classifications ───────────────────────────────────────────────────────

CREATE TABLE classifications (
    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
    image_id                INTEGER NOT NULL REFERENCES images(id),
    model                   TEXT    NOT NULL,
    prompt_version          TEXT    NOT NULL,
    contains_wildlife       INTEGER NOT NULL,
    is_interesting          INTEGER NOT NULL,
    summary                 TEXT,
    species_json            TEXT,
    confidence              REAL,
    classification_json     TEXT,
    raw_response            TEXT,
    request_started_at      TEXT    NOT NULL,
    request_completed_at    TEXT    NOT NULL,
    created_at              TEXT    NOT NULL,

    UNIQUE(image_id, model, prompt_version)
);

-- ── Search cursors ────────────────────────────────────────────────────────

CREATE TABLE search_cursors (
    camera_id                       INTEGER PRIMARY KEY REFERENCES cameras(id),
    next_search_at                  TEXT,
    last_completed_window_start     TEXT,
    last_completed_window_end       TEXT,
    last_poll_at                    TEXT,
    last_error                      TEXT,
    updated_at                      TEXT NOT NULL
);

-- ── Service metadata ──────────────────────────────────────────────────────

CREATE TABLE service_metadata (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

-- ── Indexes ───────────────────────────────────────────────────────────────

-- Index for claiming pending or due-retry downloads
CREATE INDEX idx_images_download_claim
    ON images(download_status, download_next_attempt_at)
    WHERE download_status IN ('pending', 'retry_wait');

-- Index for claiming new or due-retry processing work
CREATE INDEX idx_images_processing_claim
    ON images(processing_status, processing_next_attempt_at)
    WHERE processing_status IN ('new', 'retry_wait');

-- Index for lease recovery on downloads
CREATE INDEX idx_images_download_lease
    ON images(download_lease_until)
    WHERE download_status = 'downloading';

-- Index for lease recovery on processing
CREATE INDEX idx_images_processing_lease
    ON images(processing_lease_until)
    WHERE processing_status = 'processing';

-- Index for camera image ordering
CREATE INDEX idx_images_camera_capture
    ON images(camera_id, capture_start_at);
