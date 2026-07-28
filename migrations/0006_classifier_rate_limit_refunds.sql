-- Records a daily-quota refund without removing the event from the rolling
-- per-minute window. A provider 429 consumed a request attempt but did not
-- consume provider tokens, so it must continue to throttle retries while no
-- longer exhausting the daily token budget.
ALTER TABLE classifier_rate_limit_events ADD COLUMN daily_refunded_at TEXT;
