-- Series grouping for the feed.
--
-- A "series" is derived from the parent directory (the reliable signal in real
-- libraries), and the episode number from the filename. Movies and specials
-- keep a NULL/0 episode so the feed can tell them apart from episodes.
ALTER TABLE media ADD COLUMN series_key TEXT;
ALTER TABLE media ADD COLUMN series_title TEXT;
ALTER TABLE media ADD COLUMN episode INTEGER;
ALTER TABLE media ADD COLUMN is_extra INTEGER NOT NULL DEFAULT 0;

CREATE INDEX idx_media_series ON media(series_key, episode);
