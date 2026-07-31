-- Partial index to accelerate garbage-collection candidate selection.

ALTER TABLE images ADD COLUMN local_file_identity TEXT;

CREATE INDEX IF NOT EXISTS idx_images_garbage_collection_candidates
    ON images(capture_start_at, id)
    WHERE processing_status = 'done' AND local_path IS NOT NULL;

-- Canonical filesystem identity lets collection validate shared files.
CREATE INDEX IF NOT EXISTS idx_images_local_file_identity
    ON images(local_file_identity)
    WHERE local_file_identity IS NOT NULL;
