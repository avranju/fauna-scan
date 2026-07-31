-- Durable provider/endpoint cooldowns following HTTP 429 responses.
-- The key is a configured provider quota group when available, otherwise a
-- stable endpoint/model identity.  Keeping this state in the database makes
-- a cooldown survive service restarts.
CREATE TABLE classifier_cooldowns (
    cooldown_group TEXT PRIMARY KEY,
    cooldown_until TEXT NOT NULL,
    updated_at     TEXT NOT NULL
);
