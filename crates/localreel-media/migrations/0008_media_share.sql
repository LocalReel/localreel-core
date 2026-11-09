-- Share links: one row per link an account created for one item.
--
-- The token is the only credential, so it is random and unguessable; expiry and
-- revocation are checked on every read instead of deleting the row, so a link
-- that stops working can still be explained ("it expired on ...").
CREATE TABLE IF NOT EXISTS media_share (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    media_id       INTEGER NOT NULL,
    token          TEXT NOT NULL UNIQUE,
    created_by     INTEGER,
    created_at     INTEGER NOT NULL,
    expires_at     INTEGER,
    revoked_at     INTEGER,
    allow_download INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_media_share_media ON media_share (media_id);
