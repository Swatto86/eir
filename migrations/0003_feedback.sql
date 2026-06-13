CREATE TABLE IF NOT EXISTS execution_feedback (
    id                       INTEGER PRIMARY KEY AUTOINCREMENT,
    execution_log_id         INTEGER REFERENCES execution_log(id),
    action                   TEXT    NOT NULL,
    succeeded                INTEGER NOT NULL,
    cpu_before               REAL,
    memory_before            REAL,
    failed_services_before   INTEGER,
    cpu_after                REAL,
    memory_after             REAL,
    failed_services_after    INTEGER,
    improvement_score        REAL,
    recorded_at              TEXT    NOT NULL
);
