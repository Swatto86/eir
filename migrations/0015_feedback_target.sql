-- Targeted-outcome effectiveness attribution (learning-loop deepening).
-- `target` is the specific fault a fix addressed (a service name for service actions;
-- '' when the fix has no metric-checkable target). `resolved` records whether that fault
-- actually cleared by the settle-windowed after-measurement (1 = cleared, 0 = not,
-- NULL = not applicable / not yet measured). `disk_before/after` close the disk gap in
-- the effectiveness score. All additive + defaulted, so an existing DB upgrades cleanly
-- (old rows read as target='' / resolved=NULL, i.e. "unmeasured" — never mis-penalised).
ALTER TABLE execution_feedback ADD COLUMN target TEXT NOT NULL DEFAULT '';
ALTER TABLE execution_feedback ADD COLUMN disk_before REAL;
ALTER TABLE execution_feedback ADD COLUMN disk_after REAL;
ALTER TABLE execution_feedback ADD COLUMN resolved INTEGER;
