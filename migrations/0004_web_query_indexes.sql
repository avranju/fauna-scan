-- Read-side indexes for the web image explorer and classification projection.

CREATE INDEX idx_images_capture_id
    ON images(capture_start_at DESC, id DESC);

CREATE INDEX idx_images_download_capture
    ON images(download_status, capture_start_at DESC, id DESC);

CREATE INDEX idx_images_processing_capture
    ON images(processing_status, capture_start_at DESC, id DESC);

CREATE INDEX idx_classifications_image_completed
    ON classifications(image_id, request_completed_at DESC, id DESC);

CREATE INDEX idx_classifications_flags_confidence
    ON classifications(contains_wildlife, is_interesting, confidence);
