-- Optimistic concurrency guard for registry undo: only restore the prior value while
-- the live value still exactly matches what Eir wrote.
ALTER TABLE registry_undo
ADD COLUMN applied_kind TEXT
CHECK (
    applied_kind IS NULL
    OR applied_kind IN ('String', 'ExpandString', 'DWord', 'QWord')
);

ALTER TABLE registry_undo
ADD COLUMN applied_data TEXT;
