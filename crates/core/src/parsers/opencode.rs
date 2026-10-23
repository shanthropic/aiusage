use crate::cache::cached_file_messages;
use crate::common::{
    build_raw_message, derive_agent, filter_messages_by_date_range, normalize_tokens,
    normalize_workspace_key, resolve_home, scan_directory, workspace_label_from_key, DateRange,
    RawUsageMessage,
};
use rayon::prelude::*;
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use std::fs;
use std::path::Path;

#[derive(Deserialize)]
struct OpenCodeMessage {
    role: Option<String>,
    #[serde(rename = "modelID")]
    model_id: Option<String>,
    #[serde(rename = "providerID")]
    provider_id: Option<String>,
    model: Option<OpenCodeModelRef>,
    tokens: Option<OpenCodeTokens>,
    time: Option<OpenCodeTime>,
    agent: Option<String>,
    mode: Option<String>,
    path: Option<OpenCodePath>,
}

#[derive(Deserialize)]
struct OpenCodeModelRef {
    id: Option<String>,
    #[serde(rename = "providerID")]
    provider_id: Option<String>,
}

#[derive(Deserialize)]
struct OpenCodeTokens {
    input: Option<i64>,
    output: Option<i64>,
    reasoning: Option<i64>,
    cache: Option<OpenCodeCache>,
}

#[derive(Deserialize)]
struct OpenCodeCache {
    read: Option<i64>,
    write: Option<i64>,
}

#[derive(Deserialize)]
struct OpenCodeTime {
    created: Option<f64>,
}

#[derive(Deserialize)]
struct OpenCodePath {
    root: Option<String>,
}

pub fn parse_opencode(range: DateRange) -> Vec<RawUsageMessage> {
    let db_path = resolve_home("~/.local/share/opencode/opencode.db");
    if db_path.exists() {
        let msgs = cached_file_messages(&db_path, || parse_opencode_sqlite(&db_path));
        if !msgs.is_empty() {
            return filter_messages_by_date_range(msgs, range);
        }
    }

    let legacy_dir = resolve_home("~/.local/share/opencode/storage/message");
    let files = scan_directory(&legacy_dir, "*.json");
    let results: Vec<RawUsageMessage> = files
        .into_par_iter()
        .flat_map(|path| cached_file_messages(&path, || parse_opencode_file(&path).into_iter().collect()))
        .collect();
    filter_messages_by_date_range(results, range)
}

fn parse_opencode_sqlite(db_path: &Path) -> Vec<RawUsageMessage> {
    let Ok(conn) = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return Vec::new();
    };

    let queries = [
        "SELECT
            COALESCE(json_extract(m.data, '$.modelID'), json_extract(m.data, '$.model.id')),
            COALESCE(json_extract(m.data, '$.providerID'), json_extract(m.data, '$.model.providerID')),
            json_extract(m.data, '$.mode'),
            json_extract(m.data, '$.agent'),
            json_extract(m.data, '$.time.created'),
            COALESCE(NULLIF(s.directory, ''), json_extract(m.data, '$.path.root')),
            COALESCE(json_extract(m.data, '$.tokens.input'), 0),
            COALESCE(json_extract(m.data, '$.tokens.output'), 0),
            COALESCE(json_extract(m.data, '$.tokens.reasoning'), 0),
            COALESCE(json_extract(m.data, '$.tokens.cache.read'), 0),
            COALESCE(json_extract(m.data, '$.tokens.cache.write'), 0)
         FROM session_message m
         LEFT JOIN session_v2 s ON s.id = m.session_id
         WHERE m.type = 'assistant' AND json_extract(m.data, '$.tokens') IS NOT NULL",
        "SELECT
            COALESCE(json_extract(m.data, '$.modelID'), json_extract(m.data, '$.model.id')),
            COALESCE(json_extract(m.data, '$.providerID'), json_extract(m.data, '$.model.providerID')),
            json_extract(m.data, '$.mode'),
            json_extract(m.data, '$.agent'),
            json_extract(m.data, '$.time.created'),
            COALESCE(NULLIF(s.directory, ''), json_extract(m.data, '$.path.root')),
            COALESCE(json_extract(m.data, '$.tokens.input'), 0),
            COALESCE(json_extract(m.data, '$.tokens.output'), 0),
            COALESCE(json_extract(m.data, '$.tokens.reasoning'), 0),
            COALESCE(json_extract(m.data, '$.tokens.cache.read'), 0),
            COALESCE(json_extract(m.data, '$.tokens.cache.write'), 0)
         FROM message m
         LEFT JOIN session s ON s.id = m.session_id
         WHERE json_extract(m.data, '$.role') = 'assistant' AND json_extract(m.data, '$.tokens') IS NOT NULL",
        "SELECT
            COALESCE(json_extract(m.data, '$.modelID'), json_extract(m.data, '$.model.id')),
            COALESCE(json_extract(m.data, '$.providerID'), json_extract(m.data, '$.model.providerID')),
            json_extract(m.data, '$.mode'),
            json_extract(m.data, '$.agent'),
            json_extract(m.data, '$.time.created'),
            json_extract(m.data, '$.path.root'),
            COALESCE(json_extract(m.data, '$.tokens.input'), 0),
            COALESCE(json_extract(m.data, '$.tokens.output'), 0),
            COALESCE(json_extract(m.data, '$.tokens.reasoning'), 0),
            COALESCE(json_extract(m.data, '$.tokens.cache.read'), 0),
            COALESCE(json_extract(m.data, '$.tokens.cache.write'), 0)
         FROM message m
         WHERE json_extract(m.data, '$.role') = 'assistant' AND json_extract(m.data, '$.tokens') IS NOT NULL",
    ];

    for sql in queries {
        let Ok(mut stmt) = conn.prepare(sql) else {
            continue;
        };
        let Ok(rows) = stmt.query_map([], |row| {
            let model_id: Option<String> = row.get(0).ok();
            let provider_id: Option<String> = row.get(1).ok();
            let mode: Option<String> = row.get(2).ok();
            let agent: Option<String> = row.get(3).ok();
            let created: f64 = row.get::<_, f64>(4).unwrap_or(0.0);
            let workspace_root: Option<String> = row.get(5).ok();
            let input: i64 = row.get::<_, i64>(6).unwrap_or(0);
            let output: i64 = row.get::<_, i64>(7).unwrap_or(0);
            let reasoning: i64 = row.get::<_, i64>(8).unwrap_or(0);
            let cache_read: i64 = row.get::<_, i64>(9).unwrap_or(0);
            let cache_write: i64 = row.get::<_, i64>(10).unwrap_or(0);
            Ok((
                model_id,
                provider_id,
                mode,
                agent,
                created,
                workspace_root,
                input,
                output,
                reasoning,
                cache_read,
                cache_write,
            ))
        }) else {
            continue;
        };

        let mut messages = Vec::new();
        for r in rows.flatten() {
            let Some(model_id) = r.0.filter(|s| !s.is_empty()) else {
                continue;
            };
            let provider = r.1.filter(|s| !s.is_empty()).unwrap_or_else(|| "unknown".to_string());
            let resolved_agent = resolve_opencode_agent(r.2.as_deref(), r.3.as_deref());
            let ws_key = r.5.as_deref().and_then(normalize_workspace_key);
            let ws_label = ws_key.as_deref().and_then(workspace_label_from_key);

            messages.push(build_raw_message(
                "opencode",
                &resolved_agent,
                r.4,
                model_id,
                provider,
                normalize_tokens(r.6, r.7, r.9, r.10, r.8),
                ws_label,
                ws_key,
            ));
        }
        return messages;
    }

    Vec::new()
}

fn parse_opencode_file(path: &Path) -> Option<RawUsageMessage> {
    let text = fs::read_to_string(path).ok()?;
    let msg: OpenCodeMessage = serde_json::from_str(&text).ok()?;
    if msg.role.as_deref() != Some("assistant") {
        return None;
    }
    let tokens = msg.tokens?;
    let model_id = msg.model_id.or_else(|| msg.model.as_ref()?.id.clone())?;
    let provider = msg
        .provider_id
        .or_else(|| msg.model.as_ref()?.provider_id.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let cache = tokens.cache.unwrap_or(OpenCodeCache {
        read: Some(0),
        write: Some(0),
    });
    let resolved_agent = resolve_opencode_agent(msg.mode.as_deref(), msg.agent.as_deref());
    let timestamp = msg.time.and_then(|t| t.created).unwrap_or(0.0);
    let ws_key = msg
        .path
        .and_then(|p| p.root)
        .as_deref()
        .and_then(normalize_workspace_key);
    let ws_label = ws_key.as_deref().and_then(workspace_label_from_key);

    Some(build_raw_message(
        "opencode",
        &resolved_agent,
        timestamp,
        model_id,
        provider,
        normalize_tokens(
            tokens.input.unwrap_or(0),
            tokens.output.unwrap_or(0),
            cache.read.unwrap_or(0),
            cache.write.unwrap_or(0),
            tokens.reasoning.unwrap_or(0),
        ),
        ws_label,
        ws_key,
    ))
}

fn resolve_opencode_agent(mode: Option<&str>, agent: Option<&str>) -> String {
    if let Some(raw) = mode.or(agent) {
        let norm = normalize_opencode_agent_name(raw);
        return derive_agent(Some(&norm));
    }
    if let Some(m) = mode {
        return match m.to_lowercase().as_str() {
            "plan" => "plan".to_string(),
            "build" => "build".to_string(),
            "agent" => "agent".to_string(),
            "ask" => "ask".to_string(),
            "debug" => "debug".to_string(),
            _ => "default".to_string(),
        };
    }
    "default".to_string()
}

fn normalize_opencode_agent_name(agent: &str) -> String {
    let cleaned = agent.trim();
    match cleaned.to_lowercase().as_str() {
        "sisyphus (ultraworker)" | "sisyphus - ultraworker" | "sisyphus ultraworker"
        | "sisyphus" => "Sisyphus".to_string(),
        "hephaestus (deep agent)" | "hephaestus - deep agent" | "hephaestus deep agent"
        | "hephaestus" => "Hephaestus".to_string(),
        "prometheus (plan builder)" | "prometheus - plan builder" | "prometheus plan builder"
        | "prometheus (planner)" | "prometheus" => "Prometheus".to_string(),
        "atlas (plan executor)" | "atlas - plan executor" | "atlas plan executor" | "atlas"
        | "orchestrator-sisyphus" => "Atlas".to_string(),
        "metis (plan consultant)" | "metis - plan consultant" | "metis plan consultant"
        | "metis" => "Metis".to_string(),
        "momus (plan critic)" | "momus - plan critic" | "momus plan critic"
        | "momus (plan reviewer)" | "momus" => "Momus".to_string(),
        "sisyphus-junior" => "Sisyphus-Junior".to_string(),
        "planner-sisyphus" => "Planner-Sisyphus".to_string(),
        _ => derive_agent(Some(cleaned)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_support::with_temp_home;

    #[test]
    fn parses_opencode_sqlite_and_legacy_json() {
        with_temp_home(|home| {
            assert!(parse_opencode(DateRange::default()).is_empty());

            // Legacy JSON fallback
            let legacy_dir = home.join(".local/share/opencode/storage/message");
            fs::create_dir_all(&legacy_dir).unwrap();
            fs::write(
                legacy_dir.join("msg_123.json"),
                r#"{"id":"msg_123","sessionID":"ses_456","role":"assistant","modelID":"claude-sonnet-4","providerID":"anthropic","cost":0.05,"tokens":{"input":100,"output":50,"reasoning":0,"cache":{"read":10,"write":5}},"time":{"created":1700000000000.0},"path":{"root":"/Users/alice/legacy-repo"},"agent":"sisyphus (ultraworker)"}"#,
            )
            .unwrap();
            fs::write(
                legacy_dir.join("msg_user.json"),
                r#"{"id":"msg_user","sessionID":"ses_001","role":"user","modelID":"claude-sonnet-4","tokens":{"input":100,"output":50,"cache":{"read":0,"write":0}},"time":{"created":1700000000000.0}}"#,
            )
            .unwrap();

            let legacy_res = parse_opencode(DateRange::default());
            assert_eq!(legacy_res.len(), 1);
            assert_eq!(legacy_res[0].model_id, "claude-sonnet-4");
            assert_eq!(
                legacy_res[0].project_path.as_deref(),
                Some("/Users/alice/legacy-repo")
            );
            assert_eq!(legacy_res[0].agent, "default");

            // SQLite DB
            let db_dir = home.join(".local/share/opencode");
            let db_path = db_dir.join("opencode.db");
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, data TEXT NOT NULL);
                 CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL);
                 INSERT INTO session (id, directory) VALUES ('ses_001', '/Users/alice/opencode-repo');
                 INSERT INTO message (id, session_id, data) VALUES ('msg_001', 'ses_001', '{\"role\":\"assistant\",\"modelID\":\"claude-sonnet-4\",\"providerID\":\"anthropic\",\"cost\":0.05,\"tokens\":{\"input\":1000,\"output\":500,\"reasoning\":0,\"cache\":{\"read\":200,\"write\":50}},\"time\":{\"created\":1700000000000.0}}');
                 INSERT INTO message (id, session_id, data) VALUES ('msg_002', 'ses_002', '{\"role\":\"assistant\",\"modelID\":\"claude-sonnet-4\",\"providerID\":\"anthropic\",\"cost\":0.05,\"tokens\":{\"input\":1000,\"output\":500,\"reasoning\":0,\"cache\":{\"read\":200,\"write\":50}},\"time\":{\"created\":1700000000000.0}}');",
            )
            .unwrap();
            drop(conn);

            let db_res = parse_opencode(DateRange::default());
            assert_eq!(db_res.len(), 2);
            assert_eq!(db_res[0].source, "opencode");
            assert_eq!(db_res[0].input, 1000.0);
            assert_eq!(db_res[0].output, 500.0);
            assert_eq!(db_res[0].cache_input, 200.0);
            assert_eq!(db_res[0].cache_output, 50.0);
            assert_eq!(
                db_res[0].project_path.as_deref(),
                Some("/Users/alice/opencode-repo")
            );
        });
    }
}
