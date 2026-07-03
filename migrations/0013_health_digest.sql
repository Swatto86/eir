-- Weekly plain-English health digests: a short retrospective the user can read,
-- generated from aggregated audit data by one bounded AI call. Only the latest row is
-- surfaced, but history is kept for context.
CREATE TABLE IF NOT EXISTS health_digest (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    text         TEXT NOT NULL,
    generated_at TEXT NOT NULL
);
