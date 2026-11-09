-- Administrator actions worth remembering.
--
-- Append-only: nothing here is edited or deleted. The point is to answer "who
-- removed that source" months later, not to be a general event log.
CREATE TABLE IF NOT EXISTS admin_audit (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    at       INTEGER NOT NULL,
    user_id  INTEGER,
    username TEXT,
    action   TEXT NOT NULL,
    target   TEXT,
    detail   TEXT
);

CREATE INDEX IF NOT EXISTS idx_admin_audit_at ON admin_audit (at DESC);
