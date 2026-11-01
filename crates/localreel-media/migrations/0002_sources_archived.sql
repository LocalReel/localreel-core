-- Soft delete for sources. Archived sources keep their media/state but are
-- hidden from the default source list and cannot be scanned.
ALTER TABLE sources ADD COLUMN archived_at INTEGER;

CREATE INDEX idx_sources_archived ON sources(archived_at);
