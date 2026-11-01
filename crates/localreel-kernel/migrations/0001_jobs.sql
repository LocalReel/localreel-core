-- Persistent job queue used by scan/probe/thumbnail/transcode work.
--
-- The queue is deliberately generic: the kernel does not know what a
-- "transcode" is; the media crate registers handlers by `kind`.

CREATE TABLE sys_jobs (
    id                TEXT PRIMARY KEY,
    kind              TEXT NOT NULL,
    payload           TEXT NOT NULL,
    payload_version   INTEGER NOT NULL DEFAULT 1,
    dedupe_key        TEXT,
    status            TEXT NOT NULL,
    priority          INTEGER NOT NULL DEFAULT 0,
    progress          REAL NOT NULL DEFAULT 0,
    message           TEXT,
    result            TEXT,
    error             TEXT,
    attempts          INTEGER NOT NULL DEFAULT 0,
    max_attempts      INTEGER NOT NULL DEFAULT 3,
    available_at      INTEGER NOT NULL,
    lease_owner       TEXT,
    lease_expires_at  INTEGER,
    created_at        INTEGER NOT NULL,
    started_at        INTEGER,
    finished_at       INTEGER,
    cancel_requested  INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_sys_jobs_claim
    ON sys_jobs(status, available_at, priority DESC, created_at);

CREATE INDEX idx_sys_jobs_lease
    ON sys_jobs(status, lease_expires_at);

-- At most one queued/running job per (kind, dedupe_key). This makes enqueueing
-- idempotent for "probe this media" style work.
CREATE UNIQUE INDEX idx_sys_jobs_dedupe
    ON sys_jobs(kind, dedupe_key)
    WHERE status IN ('queued', 'running') AND dedupe_key IS NOT NULL;
