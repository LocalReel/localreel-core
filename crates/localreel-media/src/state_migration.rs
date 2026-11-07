use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

use crate::model::MediaState;
use crate::service::MediaService;

#[derive(Debug, Clone, Serialize)]
pub struct StateMigrationDetail {
    pub legacy_filename: String,
    pub target_filename: Option<String>,
    pub episode: Option<i64>,
    pub status: String,
    pub before_position: f64,
    pub after_position: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateMigrationReport {
    pub dry_run: bool,
    pub source_id: Option<i64>,
    pub legacy_states: usize,
    pub matched: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub ambiguous: usize,
    pub unmatched: usize,
    pub conflicts: usize,
    pub details: Vec<StateMigrationDetail>,
}

#[derive(Debug)]
struct LegacyStateRow {
    filename: String,
    liked: bool,
    favorite: bool,
    position: f64,
    duration: f64,
}

#[derive(Debug, Clone)]
struct TargetMedia {
    id: i64,
    filename: String,
    episode: i64,
}

/// Copy legacy `video_states` onto current media by episode number.
///
/// This is intentionally a one-way merge: existing current progress wins on
/// position (max), flags are OR-ed, and the target's duration is used when it
/// is known. Ambiguous episode matches are skipped, never guessed.
pub fn migrate_legacy_state(
    db: &Database,
    legacy_path: &Path,
    source_id: Option<i64>,
    dry_run: bool,
) -> Result<StateMigrationReport> {
    if !legacy_path.is_file() {
        return Err(KernelError::InvalidInput(format!(
            "legacy database does not exist: {}",
            legacy_path.display()
        )));
    }

    let legacy = Connection::open(legacy_path)?;
    legacy.busy_timeout(Duration::from_secs(5))?;
    legacy.execute_batch("PRAGMA query_only = ON;")?;
    if !table_exists(&legacy, "videos")? {
        return Err(KernelError::InvalidInput(
            "legacy database is missing the 'videos' table".into(),
        ));
    }
    let legacy_states = read_legacy_states(&legacy)?;

    let targets = load_targets(db, source_id)?;
    let mut by_episode: HashMap<i64, Vec<TargetMedia>> = HashMap::new();
    for target in targets {
        by_episode.entry(target.episode).or_default().push(target);
    }

    let service = MediaService::new(db.clone());
    let mut report = StateMigrationReport {
        dry_run,
        source_id,
        legacy_states: legacy_states.len(),
        matched: 0,
        updated: 0,
        unchanged: 0,
        ambiguous: 0,
        unmatched: 0,
        conflicts: 0,
        details: Vec::new(),
    };

    for legacy_state in legacy_states {
        let episode = episode_key(&legacy_state.filename);
        let candidates = episode.and_then(|episode| by_episode.get(&episode));
        let target = match candidates {
            Some(candidates) if candidates.len() == 1 => &candidates[0],
            Some(candidates) if candidates.len() > 1 => {
                report.ambiguous += 1;
                push_detail(
                    &mut report,
                    &legacy_state,
                    None,
                    episode,
                    "ambiguous",
                    0.0,
                    0.0,
                );
                continue;
            }
            _ => {
                report.unmatched += 1;
                push_detail(
                    &mut report,
                    &legacy_state,
                    None,
                    episode,
                    "unmatched",
                    0.0,
                    0.0,
                );
                continue;
            }
        };

        report.matched += 1;
        let mut current = service.get_state(crate::account::OWNER_ID, target.id)?;
        let before_state = current.clone();
        let before_position = current.position;
        let after = merge_state(&mut current, &legacy_state, target.id);
        let changed = state_changed(&before_state, &current);
        if changed {
            report.updated += 1;
        } else {
            report.unchanged += 1;
        }
        if before_position > 0.0
            && legacy_state.position > 0.0
            && (before_position - legacy_state.position).abs() > 1.0
        {
            report.conflicts += 1;
        }

        if changed && !dry_run {
            service.save_state(&current)?;
        }
        push_detail(
            &mut report,
            &legacy_state,
            Some(&target.filename),
            episode,
            if changed { "updated" } else { "unchanged" },
            before_position,
            after,
        );
    }

    Ok(report)
}

fn merge_state(current: &mut MediaState, legacy: &LegacyStateRow, media_id: i64) -> f64 {
    current.media_id = media_id;
    current.liked |= legacy.liked;
    current.favorite |= legacy.favorite;
    let duration = if current.duration > 0.0 {
        current.duration
    } else if legacy.duration > 0.0 {
        legacy.duration
    } else {
        0.0
    };
    current.duration = duration;
    let mut position = current.position.max(legacy.position);
    if duration > 0.0 {
        position = position.min((duration - 1.0).max(0.0));
    }
    current.position = position;
    current.position
}

fn state_changed(before: &MediaState, after: &MediaState) -> bool {
    before.liked != after.liked
        || before.favorite != after.favorite
        || (before.position - after.position).abs() > f64::EPSILON
        || (before.duration - after.duration).abs() > f64::EPSILON
}

fn push_detail(
    report: &mut StateMigrationReport,
    legacy: &LegacyStateRow,
    target_filename: Option<&str>,
    episode: Option<i64>,
    status: &str,
    before_position: f64,
    after_position: f64,
) {
    if report.details.len() >= 100 {
        return;
    }
    report.details.push(StateMigrationDetail {
        legacy_filename: legacy.filename.clone(),
        target_filename: target_filename.map(str::to_string),
        episode,
        status: status.to_string(),
        before_position,
        after_position,
    });
}

fn load_targets(db: &Database, source_id: Option<i64>) -> Result<Vec<TargetMedia>> {
    db.with_read(|connection| {
        let (sql, params): (&str, Vec<i64>) = match source_id {
            Some(source_id) => (
                "SELECT id, filename FROM media WHERE source_id = ?1 ORDER BY id",
                vec![source_id],
            ),
            None => ("SELECT id, filename FROM media ORDER BY id", Vec::new()),
        };
        let mut statement = connection.prepare(sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut targets = Vec::new();
        for row in rows {
            let (id, filename) = row?;
            if let Some(episode) = episode_key(&filename) {
                targets.push(TargetMedia {
                    id,
                    filename,
                    episode,
                });
            }
        }
        Ok(targets)
    })
}

fn read_legacy_states(connection: &Connection) -> Result<Vec<LegacyStateRow>> {
    if !table_exists(connection, "video_states")? {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare(
        "SELECT v.filename, s.liked, s.favorite, s.position, s.duration
         FROM video_states s
         JOIN videos v ON v.id = s.video_id
         ORDER BY v.id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(LegacyStateRow {
            filename: row.get(0)?,
            liked: row.get::<_, i64>(1)? != 0,
            favorite: row.get::<_, i64>(2)? != 0,
            position: row.get(3)?,
            duration: row.get(4)?,
        })
    })?;
    let mut states = Vec::new();
    for row in rows {
        states.push(row?);
    }
    Ok(states)
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Pull an episode number out of common release names.
///
/// Supported: `S01E02`, `s1e2`, `[02]`, `第02话`, ` - 02`, `E02`.
pub fn episode_key(filename: &str) -> Option<i64> {
    let name = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename)
        .to_ascii_lowercase();
    let bytes = name.as_bytes();

    // SxxEyy / s1e2
    let mut index = 0;
    while index + 3 < bytes.len() {
        if bytes[index] == b's' {
            let mut cursor = index + 1;
            let start = cursor;
            while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                cursor += 1;
            }
            if cursor > start
                && cursor < bytes.len()
                && bytes[cursor] == b'e'
                && cursor + 1 < bytes.len()
                && bytes[cursor + 1].is_ascii_digit()
            {
                let mut end = cursor + 1;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                if let Ok(value) = name[cursor + 1..end].parse::<i64>() {
                    if value > 0 {
                        return Some(value);
                    }
                }
            }
        }
        index += 1;
    }

    // [02] or [2]; pure numeric brackets only (skip [1080p], [GB], ...)
    let chars: Vec<char> = name.chars().collect();
    let mut brace_index = 0;
    while brace_index < chars.len() {
        if chars[brace_index] == '[' {
            let mut end = brace_index + 1;
            while end < chars.len() && chars[end].is_ascii_digit() {
                end += 1;
            }
            if end > brace_index + 1
                && end < chars.len()
                && chars[end] == ']'
                && end - brace_index - 1 <= 3
            {
                let value: String = chars[brace_index + 1..end].iter().collect();
                if let Ok(value) = value.parse::<i64>() {
                    if value > 0 {
                        return Some(value);
                    }
                }
            }
        }
        brace_index += 1;
    }

    // 第02话 / 第2集
    for marker in ["第", "集", "話", "话"] {
        if let Some(start) = name.find(marker) {
            let rest = &name[start + marker.len()..];
            let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
            if let Ok(value) = digits.parse::<i64>() {
                if value > 0 {
                    return Some(value);
                }
            }
        }
    }

    // trailing " - 02" / " E02"
    let trimmed = name.trim_end_matches([')', ']', ' ']);
    if let Some((_, tail)) = trimmed.rsplit_once(" - ") {
        let digits: String = tail.chars().take_while(|ch| ch.is_ascii_digit()).collect();
        if let Ok(value) = digits.parse::<i64>() {
            if value > 0 {
                return Some(value);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use localreel_kernel::migrations as kernel_migrations;

    use crate::migrations;

    #[test]
    fn extracts_common_episode_patterns() {
        assert_eq!(
            episode_key("[TUZI][12-Sai. Chicchana Mune no Tokimeki][01][GB][1280X720].mp4"),
            Some(1)
        );
        assert_eq!(
            episode_key("～小小胸口的怦然心动～12岁 - S01E12.mp4"),
            Some(12)
        );
        assert_eq!(episode_key("Show - 03.mkv"), Some(3));
        assert_eq!(episode_key("Show 第04话.mp4"), Some(4));
        assert_eq!(episode_key("Show [1080p].mp4"), None);
    }

    #[test]
    fn migrates_state_by_episode_without_overwriting_newer_progress() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("current.db").to_str().unwrap(), 1).unwrap();
        kernel_migrations::apply(&db, "kernel", localreel_kernel::jobs::migrations()).unwrap();
        kernel_migrations::apply(&db, "media", migrations::list()).unwrap();

        let now = localreel_kernel::time::now_secs();
        db.with_write(|connection| {
            connection.execute(
                "INSERT INTO sources (id, uri, kind, enabled, created_at, updated_at)
                 VALUES (1, 'file:///media', 'local', 1, ?1, ?1)",
                [now],
            )?;
            for (id, filename) in [(1i64, "Show - S01E01.mp4"), (2, "Show - S01E02.mp4")] {
                connection.execute(
                    "INSERT INTO media (id, uid, source_id, uri, canonical_path, filename,
                                        duration, file_size, modified_at, created_at, updated_at)
                     VALUES (?1, ?2, 1, ?3, ?3, ?4, 100.0, 0, 0, ?5, ?5)",
                    rusqlite::params![
                        id,
                        format!("uid-{id}"),
                        format!("file:///media/{filename}"),
                        filename,
                        now
                    ],
                )?;
            }
            connection.execute(
                "INSERT INTO media_state (media_id, user_id, liked, favorite, position, duration, updated_at)
                 VALUES (1, 1, 0, 0, 50.0, 100.0, ?1)",
                [now],
            )?;
            Ok(())
        })
        .unwrap();

        let legacy_path = dir.path().join("legacy.db");
        let legacy = Connection::open(&legacy_path).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE videos (id INTEGER PRIMARY KEY, filename TEXT NOT NULL);
                 CREATE TABLE video_states (
                     video_id INTEGER PRIMARY KEY, liked INTEGER, favorite INTEGER,
                     position REAL, duration REAL
                 );",
            )
            .unwrap();
        for (id, filename, liked, favorite, position) in [
            (1i64, "[01].mp4", 1, 0, 42.0),
            (2, "[02].mp4", 0, 1, 10.0),
            (3, "[03].mp4", 0, 0, 7.0),
        ] {
            legacy
                .execute(
                    "INSERT INTO videos (id, filename) VALUES (?1, ?2)",
                    rusqlite::params![id, filename],
                )
                .unwrap();
            legacy
                .execute(
                    "INSERT INTO video_states
                     (video_id, liked, favorite, position, duration) VALUES (?1, ?2, ?3, ?4, 100)",
                    rusqlite::params![id, liked, favorite, position],
                )
                .unwrap();
        }
        drop(legacy);

        let report = migrate_legacy_state(&db, &legacy_path, None, true).unwrap();
        assert_eq!(report.legacy_states, 3);
        assert_eq!(report.matched, 2);
        assert_eq!(report.unmatched, 1);
        assert!(report.updated >= 1);

        let preview = db
            .with_read(|connection| {
                Ok(connection.query_row(
                    "SELECT favorite, position FROM media_state WHERE media_id = 1",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!(preview, (0, 50.0));

        let report = migrate_legacy_state(&db, &legacy_path, None, false).unwrap();
        assert_eq!(report.updated, 2);
        let state = db
            .with_read(|connection| {
                Ok(connection.query_row(
                    "SELECT liked, favorite, position FROM media_state WHERE media_id = 1",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, f64>(2)?,
                        ))
                    },
                )?)
            })
            .unwrap();
        assert_eq!(state, (1, 0, 50.0));
        let state = db
            .with_read(|connection| {
                Ok(connection.query_row(
                    "SELECT liked, favorite, position FROM media_state WHERE media_id = 2",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, f64>(2)?,
                        ))
                    },
                )?)
            })
            .unwrap();
        assert_eq!(state, (0, 1, 10.0));
    }
}
