-- Accounts, sessions and per-user playback state.
--
-- Until now LocalReel had one implicit user: playback position, likes and
-- favourites lived in a single `media_state` row per media item. Serving a
-- household (a NAS, a shared desktop) needs accounts and per-account state, so
-- the state table gains `user_id` and a composite primary key.
--
-- The owner account (id 1) is created here with an empty password. An empty
-- password means "not configured yet": the server refuses to log in and the UI
-- asks for one before anything else. The migration then hands every existing
-- state row to that owner, so an upgraded library keeps its watch history.

CREATE TABLE users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT NOT NULL DEFAULT '',
    role          TEXT NOT NULL DEFAULT 'user',
    enabled       INTEGER NOT NULL DEFAULT 1,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    last_login_at INTEGER
);

INSERT INTO users (id, username, password_hash, role, enabled, created_at, updated_at)
VALUES (1, 'admin', '', 'admin', 1, 0, 0);

CREATE TABLE sessions (
    id          TEXT PRIMARY KEY,
    csrf        TEXT NOT NULL,
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    user_agent  TEXT
);

CREATE INDEX idx_sessions_user ON sessions(user_id);
CREATE INDEX idx_sessions_expires ON sessions(expires_at);

CREATE TABLE media_state_per_user (
    media_id   INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    liked      INTEGER NOT NULL DEFAULT 0,
    favorite   INTEGER NOT NULL DEFAULT 0,
    position   REAL NOT NULL DEFAULT 0,
    duration   REAL NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (media_id, user_id)
);

INSERT INTO media_state_per_user (media_id, user_id, liked, favorite, position, duration, updated_at)
SELECT media_id, 1, liked, favorite, position, duration, updated_at FROM media_state;

DROP TABLE media_state;

ALTER TABLE media_state_per_user RENAME TO media_state;

CREATE INDEX idx_media_state_favorite ON media_state(favorite);
CREATE INDEX idx_media_state_updated ON media_state(updated_at);
CREATE INDEX idx_media_state_user ON media_state(user_id);
