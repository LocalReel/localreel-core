CREATE TABLE plugin_catalog_drafts (
    plugin_id      TEXT NOT NULL,
    plugin_version TEXT NOT NULL,
    remote_id      TEXT NOT NULL,
    title_json     TEXT NOT NULL,
    title_digest   TEXT NOT NULL,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY (plugin_id, plugin_version, remote_id)
);
