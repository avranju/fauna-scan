-- Independent machine credentials. Only a SHA-256 fingerprint is persisted.
-- NULL expires_at explicitly means no expiry; revocation deletes the row.
CREATE TABLE api_tokens (
    name TEXT PRIMARY KEY NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    expires_at BIGINT
);
