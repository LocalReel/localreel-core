-- Audio (and, in a later release, subtitle) tracks of one media item.
--
-- Derived state, not user state: a re-probe replaces every row for that media.
-- Keeping it in a table (instead of re-probing on every request) is what lets
-- the player show a track menu without running ffprobe.
--
-- `kind` is already the discriminator subtitles will need, so adding them later
-- is a policy change rather than a migration.

CREATE TABLE media_tracks (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    media_id      INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    kind          TEXT NOT NULL,          -- 'audio' (subtitle is reserved)
    source        TEXT NOT NULL,          -- 'embedded' | 'sidecar'
    stream_index  INTEGER,                -- ffmpeg stream index; NULL for sidecar
    path          TEXT,                   -- sidecar path; NULL for embedded
    format        TEXT,                   -- codec name: 'aac', 'ac3', 'srt', ...
    language      TEXT,                   -- normalized: 'zh', 'en', 'ja', ...
    title         TEXT,                   -- container title or inferred name
    channels      INTEGER,                -- audio channel count
    is_default    INTEGER NOT NULL DEFAULT 0,
    is_forced     INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    UNIQUE (media_id, kind, source, stream_index, path)
);

CREATE INDEX media_tracks_media ON media_tracks (media_id, kind);
