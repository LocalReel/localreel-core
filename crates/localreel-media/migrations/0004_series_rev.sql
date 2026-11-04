-- Derivation revision for the series columns.
--
-- The scanner recomputes series_key/series_title/episode when a row's revision
-- is older than the current parser revision. That lets parser improvements reach
-- existing libraries on the next scan without a manual rebuild.
ALTER TABLE media ADD COLUMN series_rev INTEGER NOT NULL DEFAULT 0;
