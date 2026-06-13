CREATE INDEX IF NOT EXISTS idx_execution_log_action      ON execution_log (action);
CREATE INDEX IF NOT EXISTS idx_execution_log_executed_at ON execution_log (executed_at);
CREATE INDEX IF NOT EXISTS idx_feedback_cpu_after        ON execution_feedback (cpu_after);
