use crate::common::{RawUsageMessage, home_dir};
use napi_derive::napi;
use rusqlite::{Connection, params};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

static CACHE_DISABLED: AtomicBool = AtomicBool::new(false);
static DB_LOCK: Mutex<()> = Mutex::new(());

const PARSE_CACHE_MAX_ENTRIES: usize = 20_000;

#[napi(object)]
#[derive(Clone, Debug, PartialEq)]
pub struct FileMessagesCacheEntry {
    pub path: String,
    pub messages: u32,
    pub bytes: f64,
}

#[napi(object)]
#[derive(Clone, Debug, PartialEq)]
pub struct FileMessagesCacheInfo {
    pub exists_on_disk: bool,
    pub file_bytes: f64,
    pub entries: u32,
    pub messages: u32,
}

pub fn cache_db_path() -> PathBuf {
    home_dir()
        .join(".mytokens")
        .join("cache")
        .join("parse-cache.db")
}

fn legacy_json_cache_paths() -> [PathBuf; 2] {
    let dir = home_dir().join(".mytokens").join("cache");
    [
        dir.join("parse-cache-v1.json"),
        dir.join("parse-cache-v2.json"),
    ]
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         CREATE TABLE IF NOT EXISTS file_cache (
             path TEXT PRIMARY KEY,
             identity TEXT NOT NULL,
             messages_json TEXT NOT NULL,
             msg_count INTEGER NOT NULL,
             byte_size INTEGER NOT NULL,
             updated_at INTEGER NOT NULL
         );",
    )
}

fn open_cache_db() -> Option<Connection> {
    let path = cache_db_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(conn) = Connection::open(&path) {
        if init_schema(&conn).is_ok() {
            return Some(conn);
        }
    }
    // Corrupt DB fallback: remove corrupt file and recreate cleanly
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(format!("{}-wal", path.display()));
    let _ = fs::remove_file(format!("{}-shm", path.display()));
    let conn = Connection::open(&path).ok()?;
    init_schema(&conn).ok()?;
    Some(conn)
}

fn file_identity_key(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();
    let mut key = format!("{}|{}|{}", path.display(), mtime_ms, meta.len());

    let wal_path = PathBuf::from(format!("{}-wal", path.display()));
    if let Ok(wal_meta) = fs::metadata(&wal_path) {
        if let Ok(wal_mtime) = wal_meta.modified() {
            if let Ok(dur) = wal_mtime.duration_since(UNIX_EPOCH) {
                key.push_str(&format!("|{}|{}", dur.as_millis(), wal_meta.len()));
            }
        }
    }
    Some(key)
}

pub fn set_cache_disabled(disabled: bool) {
    CACHE_DISABLED.store(disabled, Ordering::SeqCst);
}

pub fn cached_file_messages<F>(path: &Path, parse: F) -> Vec<RawUsageMessage>
where
    F: FnOnce() -> Vec<RawUsageMessage>,
{
    if CACHE_DISABLED.load(Ordering::SeqCst) {
        return parse();
    }

    let Some(identity) = file_identity_key(path) else {
        return parse();
    };
    let path_str = path.to_string_lossy().to_string();

    if let Ok(_guard) = DB_LOCK.lock() {
        if let Some(conn) = open_cache_db() {
            let cached_json: Option<String> = conn
                .query_row(
                    "SELECT messages_json FROM file_cache WHERE path = ?1 AND identity = ?2",
                    params![&path_str, &identity],
                    |row| row.get(0),
                )
                .ok();
            if let Some(json_str) = cached_json {
                if let Ok(msgs) = serde_json::from_str::<Vec<RawUsageMessage>>(&json_str) {
                    return msgs;
                }
            }
        }
    }

    let messages = parse();

    if let Ok(json_str) = serde_json::to_string(&messages) {
        if let Ok(_guard) = DB_LOCK.lock() {
            if let Some(conn) = open_cache_db() {
                let now = std::time::SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let _ = conn.execute(
                    "INSERT OR REPLACE INTO file_cache (path, identity, messages_json, msg_count, byte_size, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        &path_str,
                        &identity,
                        &json_str,
                        messages.len() as i64,
                        json_str.len() as i64,
                        now
                    ],
                );
            }
        }
    }

    messages
}

pub fn prune_cache_if_needed() {
    if CACHE_DISABLED.load(Ordering::SeqCst) {
        return;
    }
    if let Ok(_guard) = DB_LOCK.lock() {
        for legacy in legacy_json_cache_paths() {
            let _ = fs::remove_file(legacy);
        }
        if let Some(conn) = open_cache_db() {
            let _ = conn.execute(
                "DELETE FROM file_cache WHERE path NOT IN (
                    SELECT path FROM file_cache ORDER BY updated_at DESC LIMIT ?1
                )",
                params![PARSE_CACHE_MAX_ENTRIES as i64],
            );
        }
    }
}

pub fn clear_cache() {
    if let Ok(_guard) = DB_LOCK.lock() {
        let db_path = cache_db_path();
        let _ = fs::remove_file(&db_path);
        let _ = fs::remove_file(format!("{}-wal", db_path.display()));
        let _ = fs::remove_file(format!("{}-shm", db_path.display()));
        for legacy in legacy_json_cache_paths() {
            let _ = fs::remove_file(legacy);
        }
    }
}

pub fn list_cache() -> Vec<FileMessagesCacheEntry> {
    let Ok(_guard) = DB_LOCK.lock() else {
        return Vec::new();
    };
    let db_path = cache_db_path();
    if !db_path.exists() {
        return Vec::new();
    }
    let Some(conn) = open_cache_db() else {
        return Vec::new();
    };
    let mut stmt =
        match conn.prepare("SELECT path, msg_count, byte_size FROM file_cache ORDER BY path ASC") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
    let rows = match stmt.query_map([], |row| {
        Ok(FileMessagesCacheEntry {
            path: row.get::<_, String>(0)?,
            messages: row.get::<_, i64>(1)?.max(0) as u32,
            bytes: row.get::<_, i64>(2)?.max(0) as f64,
        })
    }) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    rows.filter_map(|r| r.ok()).collect()
}

pub fn cache_info() -> FileMessagesCacheInfo {
    let Ok(_guard) = DB_LOCK.lock() else {
        return FileMessagesCacheInfo {
            exists_on_disk: false,
            file_bytes: 0.0,
            entries: 0,
            messages: 0,
        };
    };
    let db_path = cache_db_path();
    let meta = fs::metadata(&db_path).ok();
    let exists_on_disk = meta.is_some();
    let file_bytes = meta.map(|m| m.len() as f64).unwrap_or(0.0);
    if !exists_on_disk {
        return FileMessagesCacheInfo {
            exists_on_disk: false,
            file_bytes: 0.0,
            entries: 0,
            messages: 0,
        };
    }
    let Some(conn) = open_cache_db() else {
        return FileMessagesCacheInfo {
            exists_on_disk,
            file_bytes,
            entries: 0,
            messages: 0,
        };
    };
    let (entries, messages): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(msg_count), 0) FROM file_cache",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or((0, 0));

    FileMessagesCacheInfo {
        exists_on_disk,
        file_bytes,
        entries: entries.max(0) as u32,
        messages: messages.max(0) as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_support::{make_temp_dir, set_file_mtime_ms, with_temp_home};
    use crate::common::{build_raw_message, normalize_tokens};

    fn message_at(ts: f64) -> RawUsageMessage {
        build_raw_message(
            "test",
            "default",
            ts,
            "gpt-4o".to_string(),
            "openai".to_string(),
            normalize_tokens(1, 2, 0, 0, 0),
            None,
            None,
        )
    }

    #[test]
    fn parses_once_and_serves_repeat_calls_from_cache() {
        with_temp_home(|_| {
            let dir = make_temp_dir("pcache");
            let path = dir.join("s.jsonl");
            fs::write(&path, "{}").unwrap();

            let mut calls = 0;
            let first = cached_file_messages(&path, || {
                calls += 1;
                vec![message_at(1_709_287_200_000.0)]
            });
            let second = cached_file_messages(&path, || {
                calls += 1;
                vec![message_at(1_709_287_200_000.0)]
            });

            assert_eq!(calls, 1);
            assert_eq!(second.len(), 1);
            assert_eq!(second[0].date_ms, first[0].date_ms);
            assert_eq!(second[0].model_id, "gpt-4o");
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn re_parses_when_file_content_changes() {
        with_temp_home(|_| {
            let dir = make_temp_dir("pcache");
            let path = dir.join("s.jsonl");
            fs::write(&path, "{\"a\":1}").unwrap();

            let mut calls = 0;
            cached_file_messages(&path, || {
                calls += 1;
                vec![message_at(1_000.0)]
            });
            fs::write(&path, "{\"a\":1234}").unwrap();
            cached_file_messages(&path, || {
                calls += 1;
                vec![message_at(1_000.0)]
            });

            assert_eq!(calls, 2);
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn re_parses_when_sqlite_wal_sidecar_changes() {
        with_temp_home(|_| {
            let dir = make_temp_dir("pcache");
            let path = dir.join("db.sqlite");
            fs::write(&path, "{}").unwrap();

            let mut calls = 0;
            cached_file_messages(&path, || {
                calls += 1;
                vec![message_at(1_000.0)]
            });

            let wal = PathBuf::from(format!("{}-wal", path.display()));
            fs::write(&wal, "wal-page-data").unwrap();
            set_file_mtime_ms(&wal, 2_000_000_000_000);

            cached_file_messages(&path, || {
                calls += 1;
                vec![message_at(1_000.0)]
            });

            assert_eq!(calls, 2);
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn falls_back_to_parsing_when_cache_file_is_corrupt() {
        with_temp_home(|temp_home| {
            let cache_dir = temp_home.join(".mytokens").join("cache");
            fs::create_dir_all(&cache_dir).unwrap();
            fs::write(cache_dir.join("parse-cache.db"), "{not sqlite").unwrap();

            let dir = make_temp_dir("pcache");
            let path = dir.join("s.jsonl");
            fs::write(&path, "{}").unwrap();

            let mut calls = 0;
            let served = cached_file_messages(&path, || {
                calls += 1;
                vec![message_at(1_000.0)]
            });

            assert_eq!(calls, 1);
            assert_eq!(served.len(), 1);
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn skips_reading_and_storing_when_disabled() {
        with_temp_home(|_| {
            let dir = make_temp_dir("pcache");
            let path = dir.join("s.jsonl");
            fs::write(&path, "{}").unwrap();

            set_cache_disabled(true);

            let mut calls = 0;
            cached_file_messages(&path, || {
                calls += 1;
                vec![message_at(1_000.0)]
            });
            cached_file_messages(&path, || {
                calls += 1;
                vec![message_at(1_000.0)]
            });
            assert_eq!(calls, 2);

            set_cache_disabled(false);
            let mut fresh_calls = 0;
            cached_file_messages(&path, || {
                fresh_calls += 1;
                Vec::new()
            });
            assert_eq!(fresh_calls, 1);
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn clear_list_and_info_work_as_expected() {
        with_temp_home(|_| {
            assert!(!cache_info().exists_on_disk);

            let dir = make_temp_dir("pcache");
            let path_b = dir.join("b.jsonl");
            let path_a = dir.join("a.jsonl");
            fs::write(&path_b, "{}").unwrap();
            fs::write(&path_a, "{}").unwrap();

            cached_file_messages(&path_a, || vec![message_at(1.0), message_at(2.0)]);
            cached_file_messages(&path_b, || vec![message_at(3.0)]);

            let listed = list_cache();
            assert_eq!(listed.len(), 2);
            assert_eq!(listed[0].path, path_a.to_string_lossy());
            assert_eq!(listed[0].messages, 2);
            assert_eq!(listed[1].path, path_b.to_string_lossy());
            assert_eq!(listed[1].messages, 1);

            let info = cache_info();
            assert!(info.exists_on_disk);
            assert!(info.file_bytes > 0.0);
            assert_eq!(info.entries, 2);
            assert_eq!(info.messages, 3);

            clear_cache();
            assert!(!cache_info().exists_on_disk);
            let _ = fs::remove_dir_all(&dir);
        });
    }
}
