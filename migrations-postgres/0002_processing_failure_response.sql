-- Fauna Scan Phase 10: processing failure diagnostic response column.
--
-- Adds a nullable TEXT column to store the latest available classifier
-- response body for failed processing attempts.

ALTER TABLE images ADD COLUMN processing_last_raw_response TEXT;
