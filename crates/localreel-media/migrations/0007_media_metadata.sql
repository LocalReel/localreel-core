-- Optional external metadata (poster, overview, episode title).
--
-- Entirely additive: a library with `provider = "none"` never writes here and
-- behaves exactly as before. One row per media item; a re-match replaces it.

CREATE TABLE media_metadata (
    media_id      INTEGER PRIMARY KEY REFERENCES media(id) ON DELETE CASCADE,
    provider      TEXT NOT NULL,          -- 'tmdb'
    provider_id   TEXT NOT NULL,          -- the provider's own id, for re-fetch
    title         TEXT,
    overview      TEXT,
    poster_path   TEXT,                   -- file name under <data>/cache/posters
    backdrop_path TEXT,
    release_date  TEXT,
    vote_average  REAL,
    confidence    REAL NOT NULL DEFAULT 0,
    fetched_at    INTEGER NOT NULL
);

CREATE INDEX media_metadata_fetched ON media_metadata (fetched_at);
