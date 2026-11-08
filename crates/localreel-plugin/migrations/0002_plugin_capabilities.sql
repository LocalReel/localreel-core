-- Capability disclosure, grants and the audit fields needed to attribute calls.
--
-- Phase 1 stores the vocabulary and the grants. The broker that executes them
-- arrives later; the audit columns are added now so a grant and a future call
-- share one attribution format instead of two.

ALTER TABLE plugin_audit ADD COLUMN capability TEXT;
ALTER TABLE plugin_audit ADD COLUMN scope_digest TEXT;
ALTER TABLE plugin_audit ADD COLUMN result TEXT;
ALTER TABLE plugin_audit ADD COLUMN bytes_in INTEGER;
ALTER TABLE plugin_audit ADD COLUMN bytes_out INTEGER;
ALTER TABLE plugin_audit ADD COLUMN duration_ms INTEGER;
ALTER TABLE plugin_audit ADD COLUMN request_id TEXT;
ALTER TABLE plugin_audit ADD COLUMN job_id TEXT;
ALTER TABLE plugin_audit ADD COLUMN user_id INTEGER;

CREATE TABLE plugin_grants (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    plugin_id      TEXT NOT NULL,
    plugin_version TEXT NOT NULL,
    capability     TEXT NOT NULL,
    scope_json     TEXT NOT NULL,
    scope_digest   TEXT NOT NULL,
    tier           INTEGER NOT NULL,
    granted_by     INTEGER,
    granted_at     INTEGER NOT NULL,
    expires_at     INTEGER,
    revoked_at     INTEGER,
    UNIQUE(plugin_id, plugin_version, capability)
);

CREATE INDEX plugin_grants_plugin ON plugin_grants (plugin_id, plugin_version, capability);
CREATE INDEX plugin_grants_active ON plugin_grants (plugin_id, revoked_at, expires_at);
