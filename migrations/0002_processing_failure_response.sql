-- Fauna Scan Phase 10: processing failure diagnostic response column.
--
-- Adds a nullable TEXT column to store the latest available classifier
-- response body for failed processing attempts.  This column is used
-- exclusively for diagnostic purposes and must never be logged.
--
-- Existing rows are unaffected; the column is NULL until a failure
-- transition populates it.

ALTER TABLE images ADD COLUMN processing_last_raw_response TEXT;
