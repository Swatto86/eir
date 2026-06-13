CREATE TABLE IF NOT EXISTS execution_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    decision_id INTEGER NOT NULL,
    action TEXT NOT NULL,
    success INTEGER NOT NULL,
    output TEXT,
    executed_at TEXT NOT NULL,
    FOREIGN KEY(decision_id) REFERENCES decisions(id)
);
