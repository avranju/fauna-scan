-- Durable reservations for provider classifier quotas.  Reservations are made
-- before an HTTP request and are deliberately retained after process restart.
CREATE TABLE classifier_rate_limit_events (
    id          TEXT PRIMARY KEY,
    quota_group TEXT NOT NULL,
    reserved_at TEXT NOT NULL,
    token_cost  INTEGER NOT NULL
);
CREATE INDEX idx_classifier_rate_limit_events_group_time
    ON classifier_rate_limit_events(quota_group, reserved_at);

CREATE TABLE classifier_rate_limit_daily_usage (
    quota_group TEXT NOT NULL,
    day         TEXT NOT NULL,
    requests    INTEGER NOT NULL,
    tokens      INTEGER NOT NULL,
    PRIMARY KEY (quota_group, day)
);
