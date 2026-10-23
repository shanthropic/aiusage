use crate::cache::cached_file_messages;
use crate::common::{
    DateRange, RawUsageMessage, build_raw_message, canonical_provider, extract_i64,
    file_modified_timestamp_ms, file_predates_range, filter_messages_by_date_range,
    infer_provider_from_model, normalize_tokens, parse_timestamp_str, resolve_home,
    workspace_label_from_key,
};
use chrono::{Datelike, Local, TimeZone};
use rayon::prelude::*;
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

fn open_ro(path: &Path) -> Option<Connection> {
    if !path.exists() {
        return None;
    }
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

// ─── Copilot ─────────────────────────────────────────────────────────────────

pub fn parse_copilot(range: DateRange) -> Vec<RawUsageMessage> {
    let db_path = resolve_home("~/.copilot/session-store.db");
    if db_path.exists() {
        let msgs = cached_file_messages(&db_path, || parse_copilot_sqlite(&db_path));
        if !msgs.is_empty() {
            return filter_messages_by_date_range(msgs, range);
        }
    }
    let jsonl_path = resolve_home("~/.copilot/usage.jsonl");
    if !jsonl_path.exists() {
        return Vec::new();
    }
    let msgs = cached_file_messages(&jsonl_path, || parse_copilot_jsonl(&jsonl_path));
    filter_messages_by_date_range(msgs, range)
}

fn parse_copilot_sqlite(db_path: &Path) -> Vec<RawUsageMessage> {
    let Some(conn) = open_ro(db_path) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT
            COALESCE(e.model, ''),
            COALESCE(e.input_tokens, 0),
            COALESCE(e.output_tokens, 0),
            COALESCE(e.cache_read_tokens, 0),
            COALESCE(e.cache_write_tokens, 0),
            COALESCE(e.reasoning_tokens, 0),
            COALESCE(e.created_at, ''),
            s.cwd,
            s.repository
         FROM assistant_usage_events e
         LEFT JOIN sessions s ON s.id = e.session_id
         ORDER BY e.created_at ASC",
    ) else {
        return Vec::new();
    };

    let Ok(rows) = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0).unwrap_or_default(),
            row.get::<_, i64>(1).unwrap_or(0).max(0),
            row.get::<_, i64>(2).unwrap_or(0).max(0),
            row.get::<_, i64>(3).unwrap_or(0).max(0),
            row.get::<_, i64>(4).unwrap_or(0).max(0),
            row.get::<_, i64>(5).unwrap_or(0).max(0),
            row.get::<_, String>(6).unwrap_or_default(),
            row.get::<_, Option<String>>(7).ok().flatten(),
            row.get::<_, Option<String>>(8).ok().flatten(),
        ))
    }) else {
        return Vec::new();
    };

    let mut messages = Vec::new();
    for r in rows.flatten() {
        if r.1 == 0 && r.2 == 0 && r.3 == 0 && r.4 == 0 && r.5 == 0 {
            continue;
        }
        let Some(ts) = parse_timestamp_str(&r.6) else {
            continue;
        };
        let model = if r.0.trim().is_empty() {
            "unknown".to_string()
        } else {
            r.0.trim().to_string()
        };
        let inferred = infer_provider_from_model(&model);
        let provider = if inferred == "unknown" {
            "github-copilot".to_string()
        } else {
            inferred
        };
        let proj_name = r.8.as_deref().and_then(workspace_label_from_key);
        let proj_path = r.7.filter(|s| !s.is_empty());

        messages.push(build_raw_message(
            "copilot",
            "default",
            ts,
            model,
            provider,
            normalize_tokens(r.1, r.2, r.3, r.4, r.5),
            proj_name,
            proj_path,
        ));
    }
    messages
}

fn parse_copilot_jsonl(path: &Path) -> Vec<RawUsageMessage> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let fallback_ts = file_modified_timestamp_ms(path);
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut line_buf = String::new();
    let mut messages = Vec::new();

    while reader.read_line(&mut line_buf).unwrap_or(0) > 0 {
        let trimmed = line_buf.trim();
        let Ok(Value::Object(span)) = serde_json::from_str::<Value>(trimmed) else {
            line_buf.clear();
            continue;
        };
        if span.get("type").and_then(|v| v.as_str()) != Some("span") {
            line_buf.clear();
            continue;
        }
        let Some(attrs) = span.get("attributes").and_then(|a| a.as_object()) else {
            line_buf.clear();
            continue;
        };
        let is_chat = attrs.get("gen_ai.operation.name").and_then(|v| v.as_str()) == Some("chat")
            || span
                .get("name")
                .and_then(|v| v.as_str())
                .map(|n| n.starts_with("chat "))
                .unwrap_or(false);
        if !is_chat {
            line_buf.clear();
            continue;
        }

        let input = attrs
            .get("gen_ai.usage.input_tokens")
            .and_then(extract_i64)
            .unwrap_or(0)
            .max(0);
        let output = attrs
            .get("gen_ai.usage.output_tokens")
            .and_then(extract_i64)
            .unwrap_or(0)
            .max(0);
        let cache_read = attrs
            .get("gen_ai.usage.cache_read.input_tokens")
            .and_then(extract_i64)
            .unwrap_or(0)
            .max(0);
        let cache_write = attrs
            .get("gen_ai.usage.cache_write.input_tokens")
            .and_then(extract_i64)
            .unwrap_or(0)
            .max(0);
        let reasoning = attrs
            .get("gen_ai.usage.reasoning.output_tokens")
            .and_then(extract_i64)
            .unwrap_or(0)
            .max(0);

        let cr_for_in = cache_read.min(input);
        let tokens = normalize_tokens(
            input - cr_for_in,
            output,
            cache_read,
            cache_write,
            reasoning,
        );
        if tokens.input == 0
            && tokens.output == 0
            && tokens.cache_input == 0
            && tokens.cache_output == 0
            && tokens.reasoning == 0
        {
            line_buf.clear();
            continue;
        }

        let model = attrs
            .get("gen_ai.response.model")
            .or_else(|| attrs.get("gen_ai.request.model"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        let inferred = infer_provider_from_model(&model);
        let provider = if inferred == "unknown" {
            "github-copilot".to_string()
        } else {
            inferred
        };
        let ts = otel_ts(span.get("endTime"))
            .or_else(|| otel_ts(span.get("startTime")))
            .unwrap_or(fallback_ts);

        messages.push(build_raw_message(
            "copilot", "default", ts, model, provider, tokens, None, None,
        ));
        line_buf.clear();
    }
    messages
}

fn otel_ts(val: Option<&Value>) -> Option<f64> {
    let arr = val?.as_array()?;
    if arr.len() < 2 {
        return None;
    }
    let secs = extract_i64(&arr[0])? as f64;
    let nanos = extract_i64(&arr[1])? as f64;
    Some(secs * 1000.0 + nanos / 1_000_000.0)
}

// ─── Hermes ──────────────────────────────────────────────────────────────────

pub fn parse_hermes(range: DateRange) -> Vec<RawUsageMessage> {
    let mut paths = vec![
        resolve_home("~/.hermes/state.db"),
        resolve_home("~/Library/Application Support/hermes/state.db"),
    ];
    if let Ok(home) = env::var("HERMES_HOME") {
        paths.insert(0, PathBuf::from(format!("{home}/state.db")));
    }

    for db_path in paths {
        if !db_path.exists() {
            continue;
        }
        let msgs = cached_file_messages(&db_path, || parse_hermes_db(&db_path));
        if !msgs.is_empty() {
            return filter_messages_by_date_range(msgs, range);
        }
    }
    Vec::new()
}

fn parse_hermes_db(db_path: &Path) -> Vec<RawUsageMessage> {
    let Some(conn) = open_ro(db_path) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT
            model,
            billing_provider,
            COALESCE(started_at, 0),
            COALESCE(input_tokens, 0),
            COALESCE(output_tokens, 0),
            COALESCE(cache_read_tokens, 0),
            COALESCE(cache_write_tokens, 0),
            COALESCE(reasoning_tokens, 0)
         FROM sessions
         WHERE model IS NOT NULL AND TRIM(model) != ''",
    ) else {
        return Vec::new();
    };

    let Ok(rows) = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0).unwrap_or_default(),
            row.get::<_, Option<String>>(1).ok().flatten(),
            row.get::<_, f64>(2).unwrap_or(0.0),
            row.get::<_, i64>(3).unwrap_or(0).max(0),
            row.get::<_, i64>(4).unwrap_or(0).max(0),
            row.get::<_, i64>(5).unwrap_or(0).max(0),
            row.get::<_, i64>(6).unwrap_or(0).max(0),
            row.get::<_, i64>(7).unwrap_or(0).max(0),
        ))
    }) else {
        return Vec::new();
    };

    let mut messages = Vec::new();
    for r in rows.flatten() {
        if r.3 + r.4 + r.5 + r.6 + r.7 == 0 {
            continue;
        }
        let provider =
            r.1.as_deref()
                .and_then(|bp| canonical_provider(bp.trim()))
                .unwrap_or_else(|| infer_provider_from_model(&r.0));
        let ts = if r.2 > 1e12 { r.2 } else { r.2 * 1000.0 };
        messages.push(build_raw_message(
            "hermes",
            "default",
            ts,
            r.0,
            provider,
            normalize_tokens(r.3, r.4, r.5, r.6, r.7),
            None,
            None,
        ));
    }
    messages
}

// ─── Goose ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct GooseModelConfig {
    model_name: String,
}

pub fn parse_goose(range: DateRange) -> Vec<RawUsageMessage> {
    let mut paths = vec![
        resolve_home("~/.local/share/goose/sessions/sessions.db"),
        resolve_home("~/Library/Application Support/goose/sessions/sessions.db"),
        resolve_home("~/.local/share/Block/goose/sessions/sessions.db"),
    ];
    if let Ok(root) = env::var("GOOSE_PATH_ROOT") {
        paths.insert(
            0,
            PathBuf::from(format!("{root}/data/sessions/sessions.db")),
        );
    }

    for db_path in paths {
        if !db_path.exists() {
            continue;
        }
        let msgs = cached_file_messages(&db_path, || parse_goose_db(&db_path));
        if !msgs.is_empty() {
            return filter_messages_by_date_range(msgs, range);
        }
    }
    Vec::new()
}

fn parse_goose_db(db_path: &Path) -> Vec<RawUsageMessage> {
    let Some(conn) = open_ro(db_path) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT
            model_config_json,
            provider_name,
            COALESCE(created_at, ''),
            COALESCE(accumulated_input_tokens, input_tokens, 0),
            COALESCE(accumulated_output_tokens, output_tokens, 0),
            COALESCE(accumulated_total_tokens, total_tokens, 0)
         FROM sessions
         WHERE model_config_json IS NOT NULL AND TRIM(model_config_json) != ''",
    ) else {
        return Vec::new();
    };

    let Ok(rows) = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0).unwrap_or_default(),
            row.get::<_, Option<String>>(1).ok().flatten(),
            row.get::<_, String>(2).unwrap_or_default(),
            row.get::<_, i64>(3).unwrap_or(0).max(0),
            row.get::<_, i64>(4).unwrap_or(0).max(0),
            row.get::<_, i64>(5).unwrap_or(0).max(0),
        ))
    }) else {
        return Vec::new();
    };

    let mut messages = Vec::new();
    for r in rows.flatten() {
        let Ok(cfg) = serde_json::from_str::<GooseModelConfig>(&r.0) else {
            continue;
        };
        let model_id = cfg.model_name.trim().to_string();
        if model_id.is_empty() || (r.3 == 0 && r.4 == 0 && r.5 == 0) {
            continue;
        }
        let provider =
            r.1.as_deref()
                .and_then(|p| canonical_provider(p.trim()))
                .unwrap_or_else(|| {
                    let inf = infer_provider_from_model(&model_id);
                    if inf.is_empty() {
                        "goose".to_string()
                    } else {
                        inf
                    }
                });
        let raw_ts = parse_timestamp_str(&r.2).unwrap_or(0.0);
        let ts = if raw_ts > 1e12 {
            raw_ts
        } else {
            raw_ts * 1000.0
        };
        let reasoning = if r.5 > r.3 + r.4 { r.5 - r.3 - r.4 } else { 0 };

        messages.push(build_raw_message(
            "goose",
            "default",
            ts,
            model_id,
            provider,
            normalize_tokens(r.3, r.4, 0, 0, reasoning),
            None,
            None,
        ));
    }
    messages
}

// ─── Kilo ────────────────────────────────────────────────────────────────────

pub fn parse_kilo(range: DateRange) -> Vec<RawUsageMessage> {
    let db_path = resolve_home("~/.local/share/kilo/kilo.db");
    if !db_path.exists() {
        return Vec::new();
    }
    let msgs = cached_file_messages(&db_path, || parse_kilo_db(&db_path));
    filter_messages_by_date_range(msgs, range)
}

fn parse_kilo_db(db_path: &Path) -> Vec<RawUsageMessage> {
    let Some(conn) = open_ro(db_path) else {
        return Vec::new();
    };
    let fallback_ts = file_modified_timestamp_ms(db_path);
    let Ok(mut stmt) = conn.prepare(
        "SELECT
            json_extract(m.data, '$.modelID'),
            json_extract(m.data, '$.providerID'),
            json_extract(m.data, '$.agent'),
            json_extract(m.data, '$.mode'),
            json_extract(m.data, '$.time.created'),
            COALESCE(json_extract(m.data, '$.tokens.input'), 0),
            COALESCE(json_extract(m.data, '$.tokens.output'), 0),
            COALESCE(json_extract(m.data, '$.tokens.reasoning'), 0),
            COALESCE(json_extract(m.data, '$.tokens.cache.read'), 0),
            COALESCE(json_extract(m.data, '$.tokens.cache.write'), 0)
         FROM message m
         WHERE json_extract(m.data, '$.role') = 'assistant'
           AND json_extract(m.data, '$.tokens') IS NOT NULL",
    ) else {
        return Vec::new();
    };

    let Ok(rows) = stmt.query_map([], |row| {
        Ok((
            row.get::<_, Option<String>>(0).ok().flatten(),
            row.get::<_, Option<String>>(1).ok().flatten(),
            row.get::<_, Option<String>>(2).ok().flatten(),
            row.get::<_, Option<String>>(3).ok().flatten(),
            row.get::<_, Option<f64>>(4).ok().flatten(),
            row.get::<_, i64>(5).unwrap_or(0).max(0),
            row.get::<_, i64>(6).unwrap_or(0).max(0),
            row.get::<_, i64>(7).unwrap_or(0).max(0),
            row.get::<_, i64>(8).unwrap_or(0).max(0),
            row.get::<_, i64>(9).unwrap_or(0).max(0),
        ))
    }) else {
        return Vec::new();
    };

    let mut messages = Vec::new();
    for r in rows.flatten() {
        let Some(model_id) = r.0.filter(|s| !s.is_empty()) else {
            continue;
        };
        let provider =
            r.1.filter(|s| !s.is_empty())
                .unwrap_or_else(|| infer_provider_from_model(&model_id));
        let agent = if r.2.is_some() || r.3.is_some() {
            "agent"
        } else {
            "default"
        };
        let ts = r.4.filter(|&t| t > 0.0).unwrap_or(fallback_ts);
        messages.push(build_raw_message(
            "kilo",
            agent,
            ts,
            model_id,
            provider,
            normalize_tokens(r.5, r.6, r.8, r.9, r.7),
            None,
            None,
        ));
    }
    messages
}

// ─── Crush ───────────────────────────────────────────────────────────────────

pub fn parse_crush(range: DateRange) -> Vec<RawUsageMessage> {
    let xdg_path = resolve_home("~/.local/share/crush/projects.json");
    let Ok(text) = fs::read_to_string(xdg_path) else {
        return Vec::new();
    };
    let Ok(Value::Array(projects)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };

    for proj in projects {
        let Some(p_str) = proj.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        let db_path = PathBuf::from(format!("{p_str}/crush.db"));
        if !db_path.exists() {
            continue;
        }
        let msgs = cached_file_messages(&db_path, || parse_crush_db(&db_path));
        if !msgs.is_empty() {
            return filter_messages_by_date_range(msgs, range);
        }
    }
    Vec::new()
}

fn parse_crush_db(db_path: &Path) -> Vec<RawUsageMessage> {
    let Some(conn) = open_ro(db_path) else {
        return Vec::new();
    };
    let Ok(mut root_stmt) = conn.prepare(
        "SELECT id, COALESCE(cost, 0), COALESCE(created_at, 0), COALESCE(updated_at, 0)
         FROM sessions
         WHERE parent_session_id IS NULL
           AND (COALESCE(message_count, 0) > 0 OR COALESCE(cost, 0) > 0)
         ORDER BY created_at ASC",
    ) else {
        return Vec::new();
    };

    let Ok(roots) = root_stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0).unwrap_or_default(),
            row.get::<_, f64>(1).unwrap_or(0.0).max(0.0),
            row.get::<_, f64>(2).unwrap_or(0.0),
            row.get::<_, f64>(3).unwrap_or(0.0),
        ))
    }) else {
        return Vec::new();
    };
    let root_rows: Vec<_> = roots.flatten().collect();
    if root_rows.is_empty() {
        return Vec::new();
    }

    let mut buckets: HashMap<String, BTreeMap<String, (f64, usize)>> = HashMap::new();
    if let Ok(mut astmt) = conn.prepare(
        "WITH RECURSIVE session_tree(root_session_id, session_id) AS (
            SELECT id, id FROM sessions WHERE parent_session_id IS NULL
            UNION ALL
            SELECT st.root_session_id, s.id FROM sessions s
            JOIN session_tree st ON s.parent_session_id = st.session_id
         )
         SELECT st.root_session_id, COALESCE(m.created_at, 0)
         FROM session_tree st
         JOIN messages m ON m.session_id = st.session_id
         WHERE m.role = 'assistant'
         ORDER BY st.root_session_id ASC, m.created_at ASC",
    ) {
        if let Ok(arows) = astmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0).unwrap_or_default(),
                r.get::<_, f64>(1).unwrap_or(0.0),
            ))
        }) {
            for (sid, created) in arows.flatten() {
                let Some(ts_ms) = norm_crush_ts(created) else {
                    continue;
                };
                let Some(day_key) = local_day_key(ts_ms) else {
                    continue;
                };
                let entry = buckets
                    .entry(sid)
                    .or_default()
                    .entry(day_key)
                    .or_insert((ts_ms, 0));
                entry.0 = entry.0.min(ts_ms);
                entry.1 += 1;
            }
        }
    }

    let mut messages = Vec::new();
    for (sid, cost, created, updated) in root_rows {
        if let Some(day_map) = buckets.get(&sid) {
            let mut days: Vec<_> = day_map.values().copied().collect();
            days.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            for (ts_ms, _) in days {
                messages.push(build_raw_message(
                    "crush",
                    "default",
                    ts_ms,
                    "session-total".to_string(),
                    "crush".to_string(),
                    normalize_tokens(0, 0, 0, 0, 0),
                    None,
                    None,
                ));
            }
            continue;
        }
        if cost <= 0.0 {
            continue;
        }
        let Some(ts_ms) = norm_crush_ts(updated).or_else(|| norm_crush_ts(created)) else {
            continue;
        };
        messages.push(build_raw_message(
            "crush",
            "default",
            ts_ms,
            "session-total".to_string(),
            "crush".to_string(),
            normalize_tokens(0, 0, 0, 0, 0),
            None,
            None,
        ));
    }

    messages.sort_by(|a, b| {
        a.date_ms
            .partial_cmp(&b.date_ms)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    messages
}

fn norm_crush_ts(raw: f64) -> Option<f64> {
    if raw <= 0.0 {
        None
    } else if raw >= 100_000_000_000.0 {
        Some(raw)
    } else {
        Some(raw * 1000.0)
    }
}

fn local_day_key(ts_ms: f64) -> Option<String> {
    let dt = Local.timestamp_millis_opt(ts_ms as i64).single()?;
    Some(format!(
        "{:04}-{:02}-{:02}",
        dt.year(),
        dt.month(),
        dt.day()
    ))
}

// ─── Synthetic (Octofriend) ──────────────────────────────────────────────────

pub fn parse_synthetic(range: DateRange) -> Vec<RawUsageMessage> {
    let paths = [
        resolve_home("~/.local/share/octofriend/octofriend.db"),
        resolve_home("~/Library/Application Support/octofriend/octofriend.db"),
    ];
    for db_path in paths {
        if !db_path.exists() {
            continue;
        }
        let msgs = cached_file_messages(&db_path, || parse_synthetic_db(&db_path));
        if !msgs.is_empty() {
            return filter_messages_by_date_range(msgs, range);
        }
    }
    Vec::new()
}

fn parse_synthetic_db(db_path: &Path) -> Vec<RawUsageMessage> {
    let Some(conn) = open_ro(db_path) else {
        return Vec::new();
    };
    let mut messages = Vec::new();

    if let Ok(mut stmt) = conn.prepare(
        "SELECT
            COALESCE(model, ''),
            COALESCE(input_tokens, 0),
            COALESCE(output_tokens, 0),
            COALESCE(cache_read_tokens, 0),
            COALESCE(cache_write_tokens, 0),
            COALESCE(reasoning_tokens, 0),
            COALESCE(timestamp, 0),
            COALESCE(provider, 'synthetic')
         FROM messages
         WHERE input_tokens IS NOT NULL OR output_tokens IS NOT NULL",
    ) {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0).unwrap_or_default(),
                r.get::<_, i64>(1).unwrap_or(0).max(0),
                r.get::<_, i64>(2).unwrap_or(0).max(0),
                r.get::<_, i64>(3).unwrap_or(0).max(0),
                r.get::<_, i64>(4).unwrap_or(0).max(0),
                r.get::<_, i64>(5).unwrap_or(0).max(0),
                r.get::<_, f64>(6).unwrap_or(0.0),
                r.get::<_, String>(7)
                    .unwrap_or_else(|_| "synthetic".to_string()),
            ))
        }) {
            for r in rows.flatten() {
                if r.1 + r.2 + r.3 + r.4 + r.5 == 0 {
                    continue;
                }
                let ts = if r.6 > 1e12 { r.6 } else { r.6 * 1000.0 };
                messages.push(build_raw_message(
                    "synthetic",
                    "default",
                    ts,
                    normalize_synthetic_model(&r.0),
                    if r.7.is_empty() {
                        "synthetic".to_string()
                    } else {
                        r.7
                    },
                    normalize_tokens(r.1, r.2, r.3, r.4, r.5),
                    None,
                    None,
                ));
            }
        }
    }

    if messages.is_empty() {
        if let Ok(mut stmt) = conn.prepare(
            "SELECT COALESCE(model, ''), COALESCE(input_tokens, 0), COALESCE(output_tokens, 0), COALESCE(timestamp, 0)
             FROM token_usage
             WHERE input_tokens > 0 OR output_tokens > 0",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0).unwrap_or_default(),
                    r.get::<_, i64>(1).unwrap_or(0).max(0),
                    r.get::<_, i64>(2).unwrap_or(0).max(0),
                    r.get::<_, f64>(3).unwrap_or(0.0),
                ))
            }) {
                for r in rows.flatten() {
                    if r.1 + r.2 == 0 {
                        continue;
                    }
                    let ts = if r.3 > 1e12 { r.3 } else { r.3 * 1000.0 };
                    messages.push(build_raw_message(
                        "synthetic",
                        "default",
                        ts,
                        normalize_synthetic_model(&r.0),
                        "synthetic".to_string(),
                        normalize_tokens(r.1, r.2, 0, 0, 0),
                        None,
                        None,
                    ));
                }
            }
        }
    }

    messages
}

fn normalize_synthetic_model(model_id: &str) -> String {
    let lower = model_id.to_lowercase();
    if let Some(rest) = lower.strip_prefix("hf:") {
        if let Some(idx) = rest.find('/') {
            return rest[idx + 1..].to_string();
        }
        return rest.to_string();
    }
    if let Some(rest) = lower.strip_prefix("accounts/") {
        if let Some(idx) = rest.find("/models/") {
            return rest[idx + 8..].to_string();
        }
    }
    lower
}

// ─── Antigravity (SQLite + Protobuf) ─────────────────────────────────────────

pub fn parse_antigravity(range: DateRange) -> Vec<RawUsageMessage> {
    let dir = resolve_home("~/.gemini/antigravity-cli/conversations");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("db"))
        .collect();
    files.sort();

    let messages: Vec<RawUsageMessage> = files
        .into_par_iter()
        .filter(|p| !file_predates_range(p, range))
        .flat_map(|path| cached_file_messages(&path, || parse_antigravity_db(&path)))
        .collect();

    filter_messages_by_date_range(messages, range)
}

fn parse_antigravity_db(path: &Path) -> Vec<RawUsageMessage> {
    let Some(conn) = open_ro(path) else {
        return Vec::new();
    };
    let traj_blob: Option<Vec<u8>> = conn
        .query_row(
            "SELECT data FROM trajectory_metadata_blob LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();
    let session_ts = traj_blob
        .as_deref()
        .and_then(|b| proto_timestamp_ms(b, 2))
        .unwrap_or_else(|| file_modified_timestamp_ms(path));

    let Ok(mut stmt) = conn.prepare("SELECT data FROM gen_metadata ORDER BY idx") else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0)) else {
        return Vec::new();
    };
    let blobs: Vec<Vec<u8>> = rows.flatten().collect();

    let (by_display, sole_model) = build_antigravity_session_models(&blobs);
    let mut seen_ids = HashSet::new();
    let mut messages = Vec::new();

    for blob in &blobs {
        let Some(chat_model) = proto_bytes_field(blob, 1) else {
            continue;
        };
        let Some(usage) = proto_bytes_field(chat_model, 4) else {
            continue;
        };
        let input = (proto_varint_field(usage, 1) + proto_varint_field(usage, 2)) as i64;
        let cache_read = proto_varint_field(usage, 5) as i64;
        let output = proto_varint_field(usage, 9) as i64;
        let reasoning = proto_varint_field(usage, 10) as i64;
        if input == 0 && output == 0 && cache_read == 0 && reasoning == 0 {
            continue;
        }
        if let Some(resp_id) = proto_string_field(usage, 11) {
            if !seen_ids.insert(resp_id) {
                continue;
            }
        }
        let resp_model = proto_string_field(chat_model, 19);
        let model_raw = resp_model
            .clone()
            .filter(|m| !m.eq_ignore_ascii_case("gemini-default"))
            .or_else(|| {
                proto_string_field(chat_model, 21)
                    .and_then(|lbl| by_display.get(&lbl).cloned())
                    .or_else(|| sole_model.clone())
            })
            .or(resp_model)
            .unwrap_or_else(|| "unknown".to_string());

        let ts = proto_bytes_field(chat_model, 9)
            .and_then(|gt| proto_timestamp_ms(gt, 4))
            .unwrap_or(session_ts);
        if ts <= 0.0 {
            continue;
        }
        let provider = infer_provider_from_model(&model_raw);
        messages.push(build_raw_message(
            "antigravity",
            "default",
            ts,
            model_raw,
            provider,
            normalize_tokens(input, output, cache_read, 0, reasoning),
            None,
            None,
        ));
    }

    messages
}

fn build_antigravity_session_models(
    blobs: &[Vec<u8>],
) -> (HashMap<String, String>, Option<String>) {
    let mut by_display: HashMap<String, Option<String>> = HashMap::new();
    let mut distinct_models = HashSet::new();
    let mut unresolved_labels = Vec::new();

    for blob in blobs {
        let Some(chat_model) = proto_bytes_field(blob, 1) else {
            continue;
        };
        let label = proto_string_field(chat_model, 21);
        let model = proto_string_field(chat_model, 19);
        match model {
            Some(ref m) if !m.eq_ignore_ascii_case("gemini-default") => {
                distinct_models.insert(m.clone());
                if let Some(lbl) = label {
                    match by_display.get(&lbl) {
                        None => {
                            by_display.insert(lbl, Some(m.clone()));
                        }
                        Some(Some(ex)) if ex != m => {
                            by_display.insert(lbl, None);
                        }
                        _ => {}
                    }
                }
            }
            _ => {
                if let Some(lbl) = label {
                    unresolved_labels.push(lbl);
                }
            }
        }
    }

    let resolved: HashMap<String, String> = by_display
        .into_iter()
        .filter_map(|(k, v)| v.map(|m| (k, m)))
        .collect();
    let all_identified = unresolved_labels.iter().all(|l| resolved.contains_key(l));
    let sole = if distinct_models.len() == 1 && all_identified {
        distinct_models.into_iter().next()
    } else {
        None
    };
    (resolved, sole)
}

fn read_varint(buf: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;
    while pos < buf.len() {
        let byte = buf[pos];
        pos += 1;
        result |= ((byte & 0x7f) as u64).checked_shl(shift)?;
        if (byte & 0x80) == 0 {
            return Some((result, pos));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}

fn proto_bytes_field(buf: &[u8], target_field: u64) -> Option<&[u8]> {
    let mut pos = 0;
    while pos < buf.len() {
        let (key, after_key) = read_varint(buf, pos)?;
        pos = after_key;
        let field = key >> 3;
        let wire = key & 7;
        match wire {
            0 => {
                let (_, after) = read_varint(buf, pos)?;
                pos = after;
            }
            1 => pos = pos.checked_add(8)?,
            2 => {
                let (len, after_len) = read_varint(buf, pos)?;
                pos = after_len;
                let end = pos.checked_add(len as usize)?;
                if end > buf.len() {
                    return None;
                }
                if field == target_field {
                    return Some(&buf[pos..end]);
                }
                pos = end;
            }
            5 => pos = pos.checked_add(4)?,
            _ => return None,
        }
    }
    None
}

fn proto_raw_varint_field(buf: &[u8], target_field: u64) -> Option<u64> {
    let mut pos = 0;
    while pos < buf.len() {
        let (key, after_key) = read_varint(buf, pos)?;
        pos = after_key;
        let field = key >> 3;
        let wire = key & 7;
        match wire {
            0 => {
                let (val, after) = read_varint(buf, pos)?;
                if field == target_field {
                    return Some(val);
                }
                pos = after;
            }
            1 => pos = pos.checked_add(8)?,
            2 => {
                let (len, after_len) = read_varint(buf, pos)?;
                pos = after_len.checked_add(len as usize)?;
            }
            5 => pos = pos.checked_add(4)?,
            _ => return None,
        }
    }
    None
}

fn proto_varint_field(buf: &[u8], target_field: u64) -> u64 {
    proto_raw_varint_field(buf, target_field).unwrap_or(0)
}

fn proto_string_field(buf: &[u8], target_field: u64) -> Option<String> {
    let bytes = proto_bytes_field(buf, target_field)?;
    let s = std::str::from_utf8(bytes).ok()?.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn proto_timestamp_ms(parent: &[u8], field: u64) -> Option<f64> {
    let ts = proto_bytes_field(parent, field)?;
    let seconds = proto_raw_varint_field(ts, 1)?;
    let nanos = proto_raw_varint_field(ts, 2).unwrap_or(0);
    if nanos > 999_999_999 {
        return None;
    }
    Some((seconds as f64) * 1000.0 + ((nanos / 1_000_000) as f64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_support::with_temp_home;

    fn encode_varint(mut val: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut b = (val & 0x7f) as u8;
            val >>= 7;
            if val != 0 {
                b |= 0x80;
            }
            out.push(b);
            if val == 0 {
                break;
            }
        }
        out
    }

    fn enc_varint(field: u64, val: u64) -> Vec<u8> {
        let mut out = encode_varint(field << 3);
        out.extend(encode_varint(val));
        out
    }

    fn enc_len(field: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = encode_varint((field << 3) | 2);
        out.extend(encode_varint(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn parses_copilot_sqlite_and_legacy_jsonl() {
        with_temp_home(|home| {
            assert!(parse_copilot(DateRange::default()).is_empty());
            let dir = home.join(".copilot");
            fs::create_dir_all(&dir).unwrap();

            let lines = [
                r#"{"type":"span","traceId":"trace-1","spanId":"tool-1","name":"execute_tool rg","attributes":{"gen_ai.operation.name":"execute_tool","gen_ai.tool.name":"rg"}}"#,
                r#"{"type":"span","traceId":"trace-1","spanId":"span-1","name":"chat claude-sonnet-4","startTime":[1775934260,133000000],"endTime":[1775934264,967317833],"attributes":{"gen_ai.operation.name":"chat","gen_ai.request.model":"claude-sonnet-4","gen_ai.response.model":"claude-sonnet-4","gen_ai.usage.input_tokens":19452,"gen_ai.usage.output_tokens":281,"gen_ai.usage.cache_read.input_tokens":123,"gen_ai.usage.reasoning.output_tokens":128}}"#,
            ];
            fs::write(dir.join("usage.jsonl"), lines.join("\n")).unwrap();

            let jsonl_res = parse_copilot(DateRange::default());
            assert_eq!(jsonl_res.len(), 1);
            assert_eq!(jsonl_res[0].model_id, "claude-sonnet-4");
            assert_eq!(jsonl_res[0].provider, "anthropic");
            assert_eq!(jsonl_res[0].input, 19329.0);
            assert_eq!(jsonl_res[0].output, 281.0);
            assert_eq!(jsonl_res[0].cache_input, 123.0);
            assert_eq!(jsonl_res[0].reasoning, 128.0);

            let conn = Connection::open(dir.join("session-store.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, repository TEXT);
                 CREATE TABLE assistant_usage_events (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, model TEXT NOT NULL, input_tokens INTEGER, output_tokens INTEGER, cache_read_tokens INTEGER, cache_write_tokens INTEGER, reasoning_tokens INTEGER, created_at TEXT);
                 INSERT INTO sessions (id, cwd, repository) VALUES ('sess-1', '/Users/test/project', 'user/project');
                 INSERT INTO assistant_usage_events (session_id, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, created_at) VALUES ('sess-1', 'gpt-5-mini', 13296, 218, 5376, 0, 128, '2026-08-12T02:30:23.563Z');",
            )
            .unwrap();
            drop(conn);

            let db_res = parse_copilot(DateRange::default());
            assert_eq!(db_res.len(), 1);
            assert_eq!(db_res[0].model_id, "gpt-5-mini");
            assert_eq!(db_res[0].provider, "openai");
            assert_eq!(db_res[0].input, 13296.0);
            assert_eq!(db_res[0].project_name.as_deref(), Some("project"));
            assert_eq!(
                db_res[0].project_path.as_deref(),
                Some("/Users/test/project")
            );
        });
    }

    #[test]
    fn parses_hermes_goose_kilo_crush_synthetic_and_antigravity() {
        with_temp_home(|home| {
            // Hermes
            let h_dir = home.join(".hermes");
            fs::create_dir_all(&h_dir).unwrap();
            let conn = Connection::open(h_dir.join("state.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, model TEXT, billing_provider TEXT, started_at INTEGER, message_count INTEGER, input_tokens INTEGER, output_tokens INTEGER, cache_read_tokens INTEGER, cache_write_tokens INTEGER, reasoning_tokens INTEGER, estimated_cost_usd REAL, actual_cost_usd REAL);
                 INSERT INTO sessions (id, model, billing_provider, started_at, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens) VALUES ('ses-1', 'claude-sonnet-4', 'anthropic', 1700000000, 100, 50, 20, 5, 2);",
            )
            .unwrap();
            drop(conn);
            let h_res = parse_hermes(DateRange::default());
            assert_eq!(h_res.len(), 1);
            assert_eq!(h_res[0].input, 100.0);

            // Goose
            let g_dir = home.join(".local/share/goose/sessions");
            fs::create_dir_all(&g_dir).unwrap();
            let conn = Connection::open(g_dir.join("sessions.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, model_config_json TEXT, provider_name TEXT, created_at TEXT, total_tokens INTEGER, input_tokens INTEGER, output_tokens INTEGER, accumulated_total_tokens INTEGER, accumulated_input_tokens INTEGER, accumulated_output_tokens INTEGER);
                 INSERT INTO sessions (id, model_config_json, provider_name, created_at, accumulated_input_tokens, accumulated_output_tokens, accumulated_total_tokens) VALUES ('ses-1', '{\"model_name\":\"claude-sonnet-4\"}', 'anthropic', '2024-12-01 10:00:00', 100, 50, 152);",
            )
            .unwrap();
            drop(conn);
            let g_res = parse_goose(DateRange::default());
            assert_eq!(g_res.len(), 1);
            assert_eq!(g_res[0].reasoning, 2.0);

            // Kilo
            let k_dir = home.join(".local/share/kilo");
            fs::create_dir_all(&k_dir).unwrap();
            let conn = Connection::open(k_dir.join("kilo.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE message (id TEXT PRIMARY KEY, data TEXT NOT NULL);
                 INSERT INTO message (id, data) VALUES ('msg-1', '{\"role\":\"assistant\",\"modelID\":\"claude-sonnet-4\",\"providerID\":\"anthropic\",\"tokens\":{\"input\":100,\"output\":50,\"reasoning\":2,\"cache\":{\"read\":20,\"write\":5}},\"time\":{\"created\":1700000000000}}');",
            )
            .unwrap();
            drop(conn);
            let k_res = parse_kilo(DateRange::default());
            assert_eq!(k_res.len(), 1);
            assert_eq!(k_res[0].input, 100.0);

            // Crush
            let c_share = home.join(".local/share/crush");
            let c_proj = home.join("projects/myproject");
            fs::create_dir_all(&c_share).unwrap();
            fs::create_dir_all(&c_proj).unwrap();
            fs::write(
                c_share.join("projects.json"),
                serde_json::json!([{ "path": c_proj }]).to_string(),
            )
            .unwrap();
            let conn = Connection::open(c_proj.join("crush.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, parent_session_id TEXT, cost REAL, created_at INTEGER, updated_at INTEGER, message_count INTEGER);
                 CREATE TABLE messages (id TEXT PRIMARY KEY, session_id TEXT, role TEXT, created_at INTEGER);
                 INSERT INTO sessions (id, parent_session_id, cost, created_at, updated_at, message_count) VALUES ('root-1', NULL, 0.05, 1700000000, 1700003600, 2);
                 INSERT INTO messages (id, session_id, role, created_at) VALUES ('m1', 'root-1', 'assistant', 1700000000);",
            )
            .unwrap();
            drop(conn);
            let c_res = parse_crush(DateRange::default());
            assert_eq!(c_res.len(), 1);
            assert_eq!(c_res[0].model_id, "session-total");

            // Synthetic
            let s_dir = home.join(".local/share/octofriend");
            fs::create_dir_all(&s_dir).unwrap();
            let conn = Connection::open(s_dir.join("octofriend.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE messages (id TEXT PRIMARY KEY, model TEXT, input_tokens INTEGER, output_tokens INTEGER, cache_read_tokens INTEGER, cache_write_tokens INTEGER, reasoning_tokens INTEGER, cost REAL, timestamp INTEGER, session_id TEXT, provider TEXT);
                 INSERT INTO messages (id, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, timestamp, provider) VALUES ('msg-1', 'HF:meta-llama/Llama-2-7b', 100, 50, 20, 5, 2, 1700000000, 'openai');",
            )
            .unwrap();
            drop(conn);
            let s_res = parse_synthetic(DateRange::default());
            assert_eq!(s_res.len(), 1);
            assert_eq!(s_res[0].model_id, "llama-2-7b");

            // Antigravity
            let a_dir = home.join(".gemini/antigravity-cli/conversations");
            fs::create_dir_all(&a_dir).unwrap();
            let mut usage_bytes = Vec::new();
            usage_bytes.extend(enc_varint(1, 1132));
            usage_bytes.extend(enc_varint(2, 500));
            usage_bytes.extend(enc_varint(5, 16000));
            usage_bytes.extend(enc_varint(9, 300));
            usage_bytes.extend(enc_varint(10, 40));
            usage_bytes.extend(enc_len(11, b"resp-1"));

            let mut ts_bytes = Vec::new();
            ts_bytes.extend(enc_varint(1, 1787336121));
            ts_bytes.extend(enc_varint(2, 250_000_000));

            let mut cm_bytes = Vec::new();
            cm_bytes.extend(enc_len(4, &usage_bytes));
            cm_bytes.extend(enc_len(9, &enc_len(4, &ts_bytes)));
            cm_bytes.extend(enc_len(19, b"gemini-3-flash"));
            let blob = enc_len(1, &cm_bytes);

            let conn = Connection::open(a_dir.join("conv-1.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE gen_metadata (idx integer, data blob, size integer NOT NULL DEFAULT 0);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
                [&blob],
            )
            .unwrap();
            drop(conn);

            let a_res = parse_antigravity(DateRange::default());
            assert_eq!(a_res.len(), 1);
            assert_eq!(a_res[0].model_id, "gemini-3-flash");
            assert_eq!(a_res[0].input, 1632.0);
            assert_eq!(a_res[0].cache_input, 16000.0);
            assert_eq!(a_res[0].output, 300.0);
            assert_eq!(a_res[0].reasoning, 40.0);
            assert_eq!(a_res[0].date_ms, 1_787_336_121_250.0);
        });
    }
}
