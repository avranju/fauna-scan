-- Fauna Scan Phase 10c: processing generation token for ownership verification.
--
-- Adds a monotonically increasing integer column that serves as a durable
-- claim generation/token.  Each new claim increments the generation, and
-- lease renewal, failure, and completion operations verify that the
-- generation matches the one held by the claiming worker.
--
-- Recovery resets the generation to 0 so the next claim gets a fresh token.

ALTER TABLE images ADD COLUMN processing_generation INTEGER NOT NULL DEFAULT 0;
