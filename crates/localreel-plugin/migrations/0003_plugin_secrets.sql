CREATE TABLE plugin_secrets (
    plugin_id  TEXT NOT NULL,
    handle     TEXT NOT NULL,
    value      TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (plugin_id, handle)
);
