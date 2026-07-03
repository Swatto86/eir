-- Persist the advisor's per-day escalation spend and count so its budget and
-- MAX_ESCALATIONS_PER_DAY caps survive a service restart (a settings change restarts
-- the service). Without this the in-memory counters reset to zero on every restart,
-- letting escalation resume after the day's budget was already spent.
CREATE TABLE IF NOT EXISTS advisor_daily_spend (
    day         TEXT    PRIMARY KEY,          -- YYYY-MM-DD (UTC)
    spent_usd   REAL    NOT NULL DEFAULT 0,   -- escalation AI spend for that day
    escalations INTEGER NOT NULL DEFAULT 0    -- number of escalations that day
);
