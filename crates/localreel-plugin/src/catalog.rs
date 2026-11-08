//! Host-side staging for `catalog.write(draft)`.
//!
//! A collector submits normalized titles; the host validates them, stores the
//! draft and keeps the original JSON. Publishing into the final CMS tables is a
//! separate reviewed action, so a plugin never writes directly into the catalog
//! the user browses. `publish` mode is not implemented at this layer.

use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::time::now_secs;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

const MAX_ITEMS_PER_RUN: u64 = 2000;
const MAX_REMOTE_ID: usize = 128;
const MAX_NAME: usize = 512;
const MAX_SOURCE_TAG: usize = 64;
const MAX_EPISODE_NAME: usize = 256;
const MAX_URL: usize = 2048;
const MAX_LINES: usize = 16;
const MAX_EPISODES_PER_LINE: usize = 500;
const MAX_EPISODES_PER_RUN: usize = 20_000;

/// One title as accepted from a plugin. Keep this deliberately small: the
/// reviewed CMS schema adds posters, categories, downloads and state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogDraftTitle {
    pub remote_id: String,
    pub name: String,
    pub lines: Vec<CatalogDraftLine>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogDraftLine {
    pub source_tag: String,
    pub episodes: Vec<CatalogDraftEpisode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogDraftEpisode {
    pub index: usize,
    pub name: String,
    pub play_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CatalogDraft {
    pub plugin_id: String,
    pub plugin_version: String,
    pub remote_id: String,
    pub title: Value,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct CatalogDraftStore {
    db: Database,
}

impl CatalogDraftStore {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Validate every item first, then upsert in one transaction. A malformed
    /// item rejects the batch rather than leaving half a collection behind.
    pub fn upsert(&self, plugin_id: &str, plugin_version: &str, items: &[Value]) -> Result<usize> {
        if plugin_id.trim().is_empty() || plugin_version.trim().is_empty() {
            return Err(KernelError::InvalidInput(
                "plugin id and version are required".into(),
            ));
        }
        if items.len() as u64 > MAX_ITEMS_PER_RUN {
            return Err(KernelError::InvalidInput(format!(
                "catalog.write accepts at most {MAX_ITEMS_PER_RUN} items per run"
            )));
        }
        let mut validated = Vec::with_capacity(items.len());
        let mut total_episodes = 0usize;
        for item in items {
            let title: CatalogDraftTitle = serde_json::from_value(item.clone())
                .map_err(|error| KernelError::InvalidInput(format!("invalid title: {error}")))?;
            validate_title(&title)?;
            total_episodes += title
                .lines
                .iter()
                .map(|line| line.episodes.len())
                .sum::<usize>();
            if total_episodes > MAX_EPISODES_PER_RUN {
                return Err(KernelError::InvalidInput(format!(
                    "catalog.write accepts at most {MAX_EPISODES_PER_RUN} episodes per run"
                )));
            }
            validated.push((title, item));
        }

        let at = now_secs();
        self.db.with_write(|connection| {
            let mut written = 0usize;
            for (title, original) in &validated {
                let title_json = serde_json::to_string(original)
                    .map_err(|error| KernelError::InvalidInput(error.to_string()))?;
                let digest = digest_hex(title_json.as_bytes());
                connection.execute(
                    "INSERT INTO plugin_catalog_drafts (
                         plugin_id, plugin_version, remote_id, title_json,
                         title_digest, created_at, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                     ON CONFLICT(plugin_id, plugin_version, remote_id) DO UPDATE SET
                         title_json = excluded.title_json,
                         title_digest = excluded.title_digest,
                         updated_at = excluded.updated_at",
                    params![
                        plugin_id,
                        plugin_version,
                        title.remote_id,
                        title_json,
                        digest,
                        at
                    ],
                )?;
                written += 1;
            }
            Ok(written)
        })
    }

    pub fn list(&self, plugin_id: Option<&str>, limit: usize) -> Result<Vec<CatalogDraft>> {
        self.db.with_read(|connection| {
            let limit = limit.clamp(1, 500) as i64;
            let mut drafts = Vec::new();
            match plugin_id {
                Some(plugin_id) => {
                    let mut statement = connection.prepare(
                        "SELECT plugin_id, plugin_version, remote_id, title_json, updated_at
                         FROM plugin_catalog_drafts WHERE plugin_id = ?1
                         ORDER BY updated_at DESC LIMIT ?2",
                    )?;
                    let rows = statement.query_map(params![plugin_id, limit], draft_row)?;
                    for row in rows {
                        drafts.push(row?);
                    }
                }
                None => {
                    let mut statement = connection.prepare(
                        "SELECT plugin_id, plugin_version, remote_id, title_json, updated_at
                         FROM plugin_catalog_drafts ORDER BY updated_at DESC LIMIT ?1",
                    )?;
                    let rows = statement.query_map(params![limit], draft_row)?;
                    for row in rows {
                        drafts.push(row?);
                    }
                }
            }
            Ok(drafts)
        })
    }

    pub fn count(&self, plugin_id: Option<&str>) -> Result<u64> {
        self.db.with_read(|connection| {
            let count: i64 = match plugin_id {
                Some(plugin_id) => connection.query_row(
                    "SELECT COUNT(*) FROM plugin_catalog_drafts WHERE plugin_id = ?1",
                    params![plugin_id],
                    |row| row.get(0),
                )?,
                None => connection.query_row(
                    "SELECT COUNT(*) FROM plugin_catalog_drafts",
                    [],
                    |row| row.get(0),
                )?,
            };
            Ok(count.max(0) as u64)
        })
    }

    pub fn one(
        &self,
        plugin_id: &str,
        version: &str,
        remote_id: &str,
    ) -> Result<Option<CatalogDraft>> {
        self.db.with_read(|connection| {
            Ok(connection
                .query_row(
                    "SELECT plugin_id, plugin_version, remote_id, title_json, updated_at
                     FROM plugin_catalog_drafts
                     WHERE plugin_id = ?1 AND plugin_version = ?2 AND remote_id = ?3",
                    params![plugin_id, version, remote_id],
                    draft_row,
                )
                .optional()?)
        })
    }
}

fn draft_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CatalogDraft> {
    let title_json: String = row.get(3)?;
    let title = serde_json::from_str(&title_json).unwrap_or(Value::Null);
    Ok(CatalogDraft {
        plugin_id: row.get(0)?,
        plugin_version: row.get(1)?,
        remote_id: row.get(2)?,
        title,
        updated_at: row.get(4)?,
    })
}

fn validate_title(title: &CatalogDraftTitle) -> Result<()> {
    require_text(&title.remote_id, "remote_id", MAX_REMOTE_ID)?;
    require_text(&title.name, "name", MAX_NAME)?;
    if title.lines.is_empty() || title.lines.len() > MAX_LINES {
        return Err(KernelError::InvalidInput(format!(
            "title lines must contain 1..={MAX_LINES} entries"
        )));
    }
    for line in &title.lines {
        require_text(&line.source_tag, "source_tag", MAX_SOURCE_TAG)?;
        if line.episodes.is_empty() || line.episodes.len() > MAX_EPISODES_PER_LINE {
            return Err(KernelError::InvalidInput(format!(
                "line episodes must contain 1..={MAX_EPISODES_PER_LINE} entries"
            )));
        }
        for episode in &line.episodes {
            if episode.index == 0 {
                return Err(KernelError::InvalidInput(
                    "episode index must be 1 or greater".into(),
                ));
            }
            require_text(&episode.name, "episode name", MAX_EPISODE_NAME)?;
            if episode.play_url.len() > MAX_URL {
                return Err(KernelError::InvalidInput(format!(
                    "play_url must be at most {MAX_URL} bytes"
                )));
            }
            let url = Url::parse(&episode.play_url).map_err(|error| {
                KernelError::InvalidInput(format!("play_url is not a URL: {error}"))
            })?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(KernelError::InvalidInput(
                    "play_url must use http(s)".into(),
                ));
            }
            if !url.username().is_empty() || url.password().is_some() {
                return Err(KernelError::InvalidInput(
                    "play_url must not contain userinfo".into(),
                ));
            }
        }
    }
    Ok(())
}

fn require_text(value: &str, field: &str, max: usize) -> Result<()> {
    let value = value.trim();
    if value.is_empty() {
        return Err(KernelError::InvalidInput(format!(
            "{field} must not be empty"
        )));
    }
    if value.len() > max {
        return Err(KernelError::InvalidInput(format!(
            "{field} must be at most {max} bytes"
        )));
    }
    Ok(())
}

fn digest_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> (tempfile::TempDir, CatalogDraftStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("catalog.db").to_str().unwrap(), 1).unwrap();
        localreel_kernel::migrations::apply(&db, "plugin", crate::audit::migrations()).unwrap();
        (dir, CatalogDraftStore::new(db))
    }

    fn title(remote_id: &str) -> Value {
        json!({
            "remote_id": remote_id,
            "name": "示例影片",
            "lines": [{
                "source_tag": "量子",
                "episodes": [{
                    "index": 1,
                    "name": "正片",
                    "play_url": "https://cdn.example/1.m3u8"
                }]
            }]
        })
    }

    #[test]
    fn upserts_and_lists_drafts() {
        let (_dir, store) = store();
        assert_eq!(
            store
                .upsert("collector", "0.1.0", &[title("1"), title("2")])
                .unwrap(),
            2
        );
        assert_eq!(store.count(Some("collector")).unwrap(), 2);
        assert_eq!(
            store.upsert("collector", "0.1.0", &[title("1")]).unwrap(),
            1
        );
        assert_eq!(store.count(Some("collector")).unwrap(), 2);
        let drafts = store.list(Some("collector"), 10).unwrap();
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].title["name"], "示例影片");
    }

    #[test]
    fn rejects_bad_titles_atomically() {
        let (_dir, store) = store();
        let bad = json!({
            "remote_id": "3",
            "name": "",
            "lines": []
        });
        assert!(store
            .upsert("collector", "0.1.0", &[title("1"), bad])
            .is_err());
        assert_eq!(store.count(Some("collector")).unwrap(), 0);
    }

    #[test]
    fn rejects_non_http_and_oversized_urls() {
        let (_dir, store) = store();
        let mut value = title("4");
        value["lines"][0]["episodes"][0]["play_url"] = json!("ftp://example.com/x");
        assert!(store.upsert("collector", "0.1.0", &[value]).is_err());

        let mut value = title("5");
        value["lines"][0]["episodes"][0]["play_url"] =
            json!(format!("https://example.com/{}", "x".repeat(3000)));
        assert!(store.upsert("collector", "0.1.0", &[value]).is_err());
    }

    #[test]
    fn caps_the_batch_size() {
        let (_dir, store) = store();
        let items: Vec<Value> = (0..2001).map(|index| title(&index.to_string())).collect();
        assert!(store.upsert("collector", "0.1.0", &items).is_err());
    }
}
