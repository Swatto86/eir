-- Preserve the exact scalar type alongside registry undo data. Existing rows remain
-- nullable: old present-value snapshots cannot be restored safely and are rejected.
ALTER TABLE registry_undo
ADD COLUMN prior_kind TEXT
CHECK (
    prior_kind IS NULL
    OR prior_kind IN ('String', 'ExpandString', 'DWord', 'QWord')
);
