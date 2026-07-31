-- Fauna Scan Phase 10c: processing generation token for ownership verification.
--
-- Adds a monotonically increasing integer column that serves as a durable
-- claim generation/token.

ALTER TABLE images ADD COLUMN processing_generation BIGINT NOT NULL DEFAULT 0;
