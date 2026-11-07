-- Audit trail for sidecar plugins.
--
-- The permission model is "least privilege + auditable" (ADR-014). The audit is
-- what makes the second half real: every start, every refused action and every
-- network request the host proxies on a plugin's behalf leaves a row, so an
-- operator can answer "what did this plugin actually do" without reading logs.

CREATE TABLE plugin_audit (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    plugin_id  TEXT NOT NULL,
    permission TEXT,
    action     TEXT NOT NULL,
    detail     TEXT,
    at         INTEGER NOT NULL
);

CREATE INDEX plugin_audit_plugin ON plugin_audit (plugin_id, id DESC);
CREATE INDEX plugin_audit_at ON plugin_audit (at);
