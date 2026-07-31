-- Durable reservations for provider classifier quotas.

CREATE TABLE classifier_rate_limit_events (
    id          TEXT PRIMARY KEY,
    quota_group TEXT NOT NULL,
    reserved_at TEXT NOT NULL,
    token_cost  BIGINT NOT NULL
);
CREATE INDEX idx_classifier_rate_limit_events_group_time
    ON classifier_rate_limit_events(quota_group, reserved_at);

CREATE TABLE classifier_rate_limit_daily_usage (
    quota_group TEXT NOT NULL,
    day         TEXT NOT NULL,
    requests    BIGINT NOT NULL,
    tokens      BIGINT NOT NULL,
    PRIMARY KEY (quota_group, day)
);
