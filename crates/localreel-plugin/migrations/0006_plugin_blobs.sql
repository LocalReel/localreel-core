CREATE TABLE plugin_blobs (
    plugin_id  TEXT NOT NULL,
    name       TEXT NOT NULL,
    data       BLOB NOT NULL,
    size       INTEGER NOT NULL,
    digest     TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (plugin_id, name)
);
