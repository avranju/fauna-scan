-- Passwords must be salted Argon2id PHC strings, never plaintext.
CREATE TABLE users (
    username TEXT PRIMARY KEY NOT NULL,
    password_hash TEXT NOT NULL
);

-- A NULL expiry means the session never expires on the server.
CREATE TABLE web_sessions (
    token_hash TEXT PRIMARY KEY NOT NULL,
    username TEXT NOT NULL REFERENCES users(username) ON DELETE CASCADE,
    credential_fingerprint TEXT NOT NULL,
    expires_at BIGINT
);
CREATE INDEX web_sessions_username ON web_sessions(username);
