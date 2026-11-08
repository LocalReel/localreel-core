CREATE TABLE plugin_schedules (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    plugin_id      TEXT NOT NULL,
    plugin_version TEXT NOT NULL,
    interval_secs  INTEGER NOT NULL,
    payload        TEXT NOT NULL DEFAULT '{}',
    next_run_at    INTEGER NOT NULL,
    last_run_at    INTEGER,
    last_error     TEXT,
    enabled        INTEGER NOT NULL DEFAULT 1,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    UNIQUE(plugin_id, plugin_version)
);
