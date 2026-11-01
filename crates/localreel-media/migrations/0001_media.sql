-- First-party media schema.
--
-- The old LocalReel tables used `directories`/`videos`/`video_states`. The v2
-- names are `sources`/`media`/`media_state`; the legacy import tool maps the
-- old rows into these tables. All timestamps are Unix seconds.

CREATE TABLE sources (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    uri         TEXT NOT NULL UNIQUE,
    kind        TEXT NOT NULL DEFAULT 'local',
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE media (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    uid            TEXT NOT NULL UNIQUE,
    source_id      INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    uri            TEXT NOT NULL UNIQUE,
    canonical_path TEXT NOT NULL,
    filename       TEXT NOT NULL,
    title          TEXT,
    duration       REAL,
    width          INTEGER,
    height         INTEGER,
    file_size      INTEGER NOT NULL DEFAULT 0,
    modified_at    INTEGER NOT NULL DEFAULT 0,
    video_codec    TEXT,
    audio_codec    TEXT,
    bitrate        INTEGER,
    thumbnail_path TEXT,
    missing_at     INTEGER,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);

CREATE INDEX idx_media_source ON media(source_id);
CREATE INDEX idx_media_missing ON media(missing_at);
CREATE INDEX idx_media_created ON media(created_at DESC);

CREATE TABLE media_state (
    media_id   INTEGER PRIMARY KEY REFERENCES media(id) ON DELETE CASCADE,
    liked      INTEGER NOT NULL DEFAULT 0,
    favorite   INTEGER NOT NULL DEFAULT 0,
    position   REAL NOT NULL DEFAULT 0,
    duration   REAL NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);

CREATE INDEX idx_media_state_favorite ON media_state(favorite);
CREATE INDEX idx_media_state_updated ON media_state(updated_at);

CREATE TABLE scan_runs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id     INTEGER NOT NULL,
    source_uri    TEXT NOT NULL,
    started_at    INTEGER NOT NULL,
    finished_at   INTEGER,
    status        TEXT NOT NULL DEFAULT 'running',
    discovered    INTEGER NOT NULL DEFAULT 0,
    added         INTEGER NOT NULL DEFAULT 0,
    updated       INTEGER NOT NULL DEFAULT 0,
    unchanged     INTEGER NOT NULL DEFAULT 0,
    removed       INTEGER NOT NULL DEFAULT 0,
    failed        INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_scan_runs_source ON scan_runs(source_id, id DESC);

CREATE TABLE scan_issues (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id     INTEGER NOT NULL REFERENCES scan_runs(id) ON DELETE CASCADE,
    source_id  INTEGER NOT NULL,
    path       TEXT NOT NULL,
    message    TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_scan_issues_run ON scan_issues(run_id);
CREATE INDEX idx_scan_issues_source ON scan_issues(source_id);

-- Generated artifacts (thumbnails, transcodes, HLS packages). Keeping them in a
-- table means cache GC never has to guess from file names.
CREATE TABLE media_variants (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    media_id     INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    kind         TEXT NOT NULL,
    preset       TEXT NOT NULL,
    path         TEXT NOT NULL,
    size         INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER,
    UNIQUE(media_id, kind, preset, path)
);

CREATE INDEX idx_media_variants_media ON media_variants(media_id);

CREATE VIRTUAL TABLE media_fts USING fts5(
    title,
    filename,
    uri,
    content='media',
    content_rowid='id'
);

CREATE TRIGGER media_fts_ai AFTER INSERT ON media BEGIN
    INSERT INTO media_fts(rowid, title, filename, uri)
    VALUES (new.id, new.title, new.filename, new.uri);
END;

CREATE TRIGGER media_fts_ad AFTER DELETE ON media BEGIN
    INSERT INTO media_fts(media_fts, rowid, title, filename, uri)
    VALUES ('delete', old.id, old.title, old.filename, old.uri);
END;

CREATE TRIGGER media_fts_au AFTER UPDATE OF title, filename, uri ON media BEGIN
    INSERT INTO media_fts(media_fts, rowid, title, filename, uri)
    VALUES ('delete', old.id, old.title, old.filename, old.uri);
    INSERT INTO media_fts(rowid, title, filename, uri)
    VALUES (new.id, new.title, new.filename, new.uri);
END;
