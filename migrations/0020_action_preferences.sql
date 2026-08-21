-- Durable per-action approval preferences. Keyed by FixAction::dedup_key so the same
-- semantic fix (e.g. kill the same process) can be ignored or always-approved across
-- analysis cycles even when AI-regenerated parameters differ. Both preferences are
-- user-set and reversible; see ARCHITECTURE.md "Action preferences".
CREATE TABLE IF NOT EXISTS action_preferences (
    action_key  TEXT PRIMARY KEY NOT NULL,
    preference  TEXT NOT NULL CHECK (preference IN ('ignore', 'always_approve')),
    summary     TEXT NOT NULL,
    target      TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL
);

-- RejectedSignal learning previously keyed only on format!("{action:?}"), which
-- fragments across regenerable parameters and almost never reaches quorum. Store the
-- stable dedup key alongside the display label so detectors can count semantic repeats.
ALTER TABLE approval_rejections ADD COLUMN action_key TEXT;
