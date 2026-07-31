-- Records a daily-quota refund without removing the event from the rolling
-- per-minute window.

ALTER TABLE classifier_rate_limit_events ADD COLUMN daily_refunded_at TEXT;
