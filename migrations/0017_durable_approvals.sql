ALTER TABLE execution_log ADD COLUMN action_key TEXT;
CREATE INDEX IF NOT EXISTS idx_execution_log_action_key_time
    ON execution_log(action_key, executed_at);

ALTER TABLE pending_approvals ADD COLUMN action_key TEXT;
ALTER TABLE pending_approvals ADD COLUMN status TEXT NOT NULL DEFAULT 'pending'
    CHECK(status IN ('pending', 'approved', 'rejected'));
ALTER TABLE pending_approvals ADD COLUMN claimed_at TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_pending_approvals_active_action
    ON pending_approvals(action_key)
    WHERE action_key IS NOT NULL AND status IN ('pending', 'approved');
