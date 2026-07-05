-- Small key/value store for transient service state that must survive a restart.
-- Used by Game Mode's power-plan boost: the pre-boost power-scheme GUID is stored
-- here when the plan is switched to High Performance, and restored + cleared on the
-- next clean exit or on service startup (so a crash mid-game still restores the plan).
CREATE TABLE IF NOT EXISTS app_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
