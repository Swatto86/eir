-- Last time each app was AI-checked for an available update, so the unmanaged
-- inventory can rotate fairly instead of re-checking the same front of the list.
CREATE TABLE IF NOT EXISTS update_checks (
    app_id     TEXT PRIMARY KEY,  -- stable, version-stripped app identity
    checked_at TEXT NOT NULL      -- RFC3339 UTC
);

CREATE INDEX IF NOT EXISTS idx_update_checks_at ON update_checks(checked_at);
