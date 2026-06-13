CREATE TABLE IF NOT EXISTS decisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TEXT NOT NULL,
    signal_snapshot TEXT NOT NULL,
    claude_response TEXT NOT NULL,
    confidence REAL NOT NULL,
    executed INTEGER NOT NULL DEFAULT 0,
    execution_output TEXT
);

CREATE TABLE IF NOT EXISTS system_state_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TEXT NOT NULL,
    cpu_usage REAL,
    memory_usage REAL,
    disk_usage REAL,
    failed_services_count INTEGER,
    snapshot TEXT
);
