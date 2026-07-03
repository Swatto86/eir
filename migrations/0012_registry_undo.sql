-- Snapshots of a registry value's prior state, captured before a registry_reset
-- overwrites it, so the change can be reverted with one click from the activity feed.
CREATE TABLE IF NOT EXISTS registry_undo (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    execution_id  INTEGER NOT NULL,
    key_path      TEXT NOT NULL,
    value_name    TEXT NOT NULL,
    -- 1 if the value existed before (restore = set back to prior_data); 0 if we
    -- created it (restore = delete the value).
    prior_existed INTEGER NOT NULL,
    prior_data    TEXT,
    created_at    TEXT NOT NULL,
    undone        INTEGER NOT NULL DEFAULT 0
);
