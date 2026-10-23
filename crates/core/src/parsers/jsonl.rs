use crate::cache::cached_file_messages;
use crate::common::{
    build_raw_message, file_modified_timestamp_ms, file_predates_range,
    filter_messages_by_date_range, normalize_tokens, normalize_workspace_key, parse_timestamp_str,
    resolve_home, scan_directory, workspace_label_from_key, DateRange, RawUsageMessage,
};
use memchr::memmem;
use rayon::prelude::*;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

// ─── Kimi ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct KimiWireLine {
    timestamp: Option<f64>,
    message: Option<KimiWireMessage>,
    r#type: Option<String>,
}

#[derive(Deserialize)]
struct KimiWireMessage {
    r#type: String,
    payload: Option<KimiStatusPayload>,
}

#[derive(Deserialize)]
struct KimiStatusPayload {
    token_usage: Option<KimiTokenUsage>,
}

#[derive(Deserialize)]
struct KimiTokenUsage {
    input_other: Option<i64>,
    output: Option<i64>,
    input_cache_read: Option<i64>,
    input_cache_creation: Option<i64>,
}

pub fn parse_kimi(range: DateRange) -> Vec<RawUsageMessage> {
    let root = resolve_home("~/.kimi/sessions");
    let files: Vec<PathBuf> = scan_directory(&root, "*.jsonl")
        .into_iter()
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()) == Some("wire.jsonl")
                && !file_predates_range(p, range)
        })
        .collect();

    let messages: Vec<RawUsageMessage> = files
        .into_par_iter()
        .flat_map(|path| cached_file_messages(&path, || parse_kimi_file(&path)))
        .collect();

    filter_messages_by_date_range(messages, range)
}

fn parse_kimi_file(path: &Path) -> Vec<RawUsageMessage> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let model = read_kimi_model_from_config(path);
    let fallback_ts = file_modified_timestamp_ms(path);
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut line_buf = String::new();
    let mut messages = Vec::new();
    let status_finder = memmem::Finder::new(b"StatusUpdate");

    while reader.read_line(&mut line_buf).unwrap_or(0) > 0 {
        let trimmed = line_buf.trim();
        if status_finder.find(trimmed.as_bytes()).is_none() {
            line_buf.clear();
            continue;
        }
        if let Ok(wire) = serde_json::from_str::<KimiWireLine>(trimmed) {
            if wire.r#type.as_deref() != Some("metadata") {
                if let Some(msg) = wire.message {
                    if msg.r#type == "StatusUpdate" {
                        if let Some(tu) = msg.payload.and_then(|p| p.token_usage) {
                            let input = tu.input_other.unwrap_or(0).max(0);
                            let output = tu.output.unwrap_or(0).max(0);
                            let cr = tu.input_cache_read.unwrap_or(0).max(0);
                            let cw = tu.input_cache_creation.unwrap_or(0).max(0);
                            if input + output + cr + cw > 0 {
                                let ts = wire.timestamp.map(|t| t * 1000.0).unwrap_or(fallback_ts);
                                messages.push(build_raw_message(
                                    "kimi",
                                    "default",
                                    ts,
                                    model.clone(),
                                    "moonshot".to_string(),
                                    normalize_tokens(input, output, cr, cw, 0),
                                    None,
                                    None,
                                ));
                            }
                        }
                    }
                }
            }
        }
        line_buf.clear();
    }
    messages
}

fn read_kimi_model_from_config(wire_path: &Path) -> String {
    if let Some(sessions_dir) = wire_path.parent().and_then(|p| p.parent()).and_then(|p| p.parent())
    {
        let config_path = sessions_dir.join("config.json");
        if let Ok(text) = fs::read_to_string(config_path) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(m) = val.get("model").and_then(|v| v.as_str()) {
                    if !m.trim().is_empty() {
                        return m.trim().to_string();
                    }
                }
            }
        }
    }
    "kimi-for-coding".to_string()
}

// ─── OpenClaw ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct OpenClawSessionEntry {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "sessionFile")]
    session_file: Option<String>,
}

#[derive(Deserialize)]
struct OpenClawEntry {
    r#type: String,
    message: Option<OpenClawMessage>,
    #[serde(rename = "customType")]
    custom_type: Option<String>,
    data: Option<OpenClawModelData>,
    #[serde(rename = "modelId")]
    model_id: Option<String>,
    provider: Option<String>,
}

#[derive(Deserialize)]
struct OpenClawMessage {
    role: Option<String>,
    usage: Option<OpenClawUsage>,
    timestamp: Option<f64>,
    provider: Option<String>,
    model: Option<String>,
}

#[derive(Deserialize)]
struct OpenClawModelData {
    provider: Option<String>,
    #[serde(rename = "modelId")]
    model_id: Option<String>,
}

#[derive(Deserialize)]
struct OpenClawUsage {
    input: Option<i64>,
    output: Option<i64>,
    #[serde(rename = "cacheRead")]
    cache_read: Option<i64>,
    #[serde(rename = "cacheWrite")]
    cache_write: Option<i64>,
}

pub fn parse_openclaw(range: DateRange) -> Vec<RawUsageMessage> {
    let root = resolve_home("~/.openclaw/agents");
    let mut session_paths: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    for index_path in scan_directory(&root, "*.json") {
        if index_path.file_name().and_then(|n| n.to_str()) == Some("sessions.json") {
            if let Ok(text) = fs::read_to_string(&index_path) {
                if let Ok(index) =
                    serde_json::from_str::<HashMap<String, OpenClawSessionEntry>>(&text)
                {
                    if let Some(index_dir) = index_path.parent() {
                        for entry in index.values() {
                            let resolved = resolve_openclaw_session_path(index_dir, entry);
                            if resolved.exists() && seen.insert(resolved.clone()) {
                                session_paths.push(resolved);
                            }
                        }
                    }
                }
            }
        }
    }

    for jsonl_path in scan_directory(&root, "*.jsonl") {
        if seen.insert(jsonl_path.clone()) {
            session_paths.push(jsonl_path);
        }
    }

    let messages: Vec<RawUsageMessage> = session_paths
        .into_par_iter()
        .filter(|p| !file_predates_range(p, range))
        .flat_map(|path| cached_file_messages(&path, || parse_openclaw_session(&path)))
        .collect();

    filter_messages_by_date_range(messages, range)
}

fn resolve_openclaw_session_path(index_dir: &Path, entry: &OpenClawSessionEntry) -> PathBuf {
    if let Some(ref sf) = entry.session_file {
        let trimmed = sf.trim();
        if !trimmed.is_empty() {
            let p = Path::new(trimmed);
            if p.is_absolute() {
                return p.to_path_buf();
            }
            return index_dir.join(trimmed);
        }
    }
    index_dir.join(format!("{}.jsonl", entry.session_id))
}

fn parse_openclaw_session(path: &Path) -> Vec<RawUsageMessage> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let file_mtime = file_modified_timestamp_ms(path);
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut line_buf = String::new();
    let mut messages = Vec::new();
    let mut current_model: Option<String> = None;
    let mut current_provider: Option<String> = None;

    while reader.read_line(&mut line_buf).unwrap_or(0) > 0 {
        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            line_buf.clear();
            continue;
        }
        let Ok(entry) = serde_json::from_str::<OpenClawEntry>(trimmed) else {
            line_buf.clear();
            continue;
        };
        match entry.r#type.as_str() {
            "model_change" => {
                if let Some(m) = entry.model_id {
                    current_model = Some(m);
                }
                if let Some(p) = entry.provider {
                    current_provider = Some(p);
                }
            }
            "custom" => {
                if entry.custom_type.as_deref() == Some("model-snapshot") {
                    if let Some(data) = entry.data {
                        if let Some(m) = data.model_id {
                            current_model = Some(m);
                        }
                        if let Some(p) = data.provider {
                            current_provider = Some(p);
                        }
                    }
                }
            }
            "message" => {
                if let Some(msg) = entry.message {
                    if msg.role.as_deref() == Some("assistant") {
                        if let Some(usage) = msg.usage {
                            let model_opt = msg.model.or_else(|| current_model.clone());
                            let provider = msg
                                .provider
                                .or_else(|| current_provider.clone())
                                .unwrap_or_else(|| "unknown".to_string());
                            if let Some(model) = model_opt {
                                current_model = Some(model.clone());
                                current_provider = Some(provider.clone());
                                let ts = msg.timestamp.unwrap_or(file_mtime);
                                messages.push(build_raw_message(
                                    "openclaw",
                                    "default",
                                    ts,
                                    model,
                                    provider,
                                    normalize_tokens(
                                        usage.input.unwrap_or(0),
                                        usage.output.unwrap_or(0),
                                        usage.cache_read.unwrap_or(0),
                                        usage.cache_write.unwrap_or(0),
                                        0,
                                    ),
                                    None,
                                    None,
                                ));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        line_buf.clear();
    }
    messages
}

// ─── Pi ──────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PiHeader {
    r#type: String,
    id: String,
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct PiEntry {
    r#type: String,
    timestamp: Option<String>,
    message: Option<PiMessage>,
}

#[derive(Deserialize)]
struct PiMessage {
    role: Option<String>,
    usage: Option<PiUsage>,
    model: Option<String>,
    provider: Option<String>,
}

#[derive(Deserialize)]
struct PiUsage {
    input: Option<i64>,
    output: Option<i64>,
    #[serde(rename = "cacheRead")]
    cache_read: Option<i64>,
    #[serde(rename = "cacheWrite")]
    cache_write: Option<i64>,
}

pub fn parse_pi(range: DateRange) -> Vec<RawUsageMessage> {
    let root = resolve_home("~/.pi/agent/sessions");
    let files = scan_directory(&root, "*.jsonl");

    let messages: Vec<RawUsageMessage> = files
        .into_par_iter()
        .filter(|p| !file_predates_range(p, range))
        .flat_map(|path| cached_file_messages(&path, || parse_pi_file(&path)))
        .collect();

    filter_messages_by_date_range(messages, range)
}

fn parse_pi_file(path: &Path) -> Vec<RawUsageMessage> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let fallback_ts = file_modified_timestamp_ms(path);
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut line_buf = String::new();
    let mut messages = Vec::new();
    let mut has_header = false;
    let mut ws_key: Option<String> = None;
    let mut ws_label: Option<String> = None;
    let usage_finder = memmem::Finder::new(b"\"usage\"");

    while reader.read_line(&mut line_buf).unwrap_or(0) > 0 {
        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            line_buf.clear();
            continue;
        }
        if !has_header {
            let Ok(header) = serde_json::from_str::<PiHeader>(trimmed) else {
                return Vec::new();
            };
            if header.r#type != "session" || header.id.is_empty() {
                return Vec::new();
            }
            has_header = true;
            ws_key = header.cwd.filter(|s| !s.is_empty());
            ws_label = ws_key
                .as_deref()
                .and_then(|k| k.split('/').filter(|s| !s.is_empty()).last())
                .map(|s| s.to_string());
            line_buf.clear();
            continue;
        }

        if usage_finder.find(trimmed.as_bytes()).is_none() {
            line_buf.clear();
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<PiEntry>(trimmed) {
            if entry.r#type == "message" {
                if let Some(msg) = entry.message {
                    if msg.role.as_deref() == Some("assistant") {
                        if let (Some(usage), Some(model), Some(provider)) =
                            (msg.usage, msg.model, msg.provider)
                        {
                            let ts = entry
                                .timestamp
                                .as_deref()
                                .and_then(parse_timestamp_str)
                                .unwrap_or(fallback_ts);
                            messages.push(build_raw_message(
                                "pi",
                                "default",
                                ts,
                                model,
                                provider,
                                normalize_tokens(
                                    usage.input.unwrap_or(0),
                                    usage.output.unwrap_or(0),
                                    usage.cache_read.unwrap_or(0),
                                    usage.cache_write.unwrap_or(0),
                                    0,
                                ),
                                ws_label.clone(),
                                ws_key.clone(),
                            ));
                        }
                    }
                }
            }
        }
        line_buf.clear();
    }
    messages
}

// ─── Qwen ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct QwenLine {
    r#type: Option<String>,
    model: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<QwenUsageMetadata>,
}

#[derive(Deserialize)]
struct QwenUsageMetadata {
    #[serde(rename = "promptTokenCount")]
    prompt_token_count: Option<i64>,
    #[serde(rename = "candidatesTokenCount")]
    candidates_token_count: Option<i64>,
    #[serde(rename = "thoughtsTokenCount")]
    thoughts_token_count: Option<i64>,
    #[serde(rename = "cachedContentTokenCount")]
    cached_content_token_count: Option<i64>,
}

pub fn parse_qwen(range: DateRange) -> Vec<RawUsageMessage> {
    let root = resolve_home("~/.qwen/projects");
    let files = scan_directory(&root, "*.jsonl");

    let messages: Vec<RawUsageMessage> = files
        .into_par_iter()
        .filter(|p| !file_predates_range(p, range))
        .flat_map(|path| cached_file_messages(&path, || parse_qwen_file(&path)))
        .collect();

    filter_messages_by_date_range(messages, range)
}

fn parse_qwen_file(path: &Path) -> Vec<RawUsageMessage> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let file_mtime = file_modified_timestamp_ms(path);
    let (ws_key, ws_label) = qwen_workspace_from_path(path);
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut line_buf = String::new();
    let mut messages = Vec::new();
    let meta_finder = memmem::Finder::new(b"usageMetadata");

    while reader.read_line(&mut line_buf).unwrap_or(0) > 0 {
        let trimmed = line_buf.trim();
        if meta_finder.find(trimmed.as_bytes()).is_none() {
            line_buf.clear();
            continue;
        }
        if let Ok(qline) = serde_json::from_str::<QwenLine>(trimmed) {
            if qline.r#type.as_deref() == Some("assistant") {
                if let Some(u) = qline.usage_metadata {
                    let input = u.prompt_token_count.unwrap_or(0).max(0);
                    let output = u.candidates_token_count.unwrap_or(0).max(0);
                    let reasoning = u.thoughts_token_count.unwrap_or(0).max(0);
                    let cache_read = u.cached_content_token_count.unwrap_or(0).max(0);
                    if input + output + reasoning + cache_read > 0 {
                        let model = qline.model.unwrap_or_else(|| "unknown".to_string());
                        let ts = qline
                            .timestamp
                            .as_deref()
                            .and_then(parse_timestamp_str)
                            .unwrap_or(file_mtime);
                        messages.push(build_raw_message(
                            "qwen",
                            "default",
                            ts,
                            model,
                            "qwen".to_string(),
                            normalize_tokens(input, output, cache_read, 0, reasoning),
                            ws_label.clone(),
                            ws_key.clone(),
                        ));
                    }
                }
            }
        }
        line_buf.clear();
    }
    messages
}

fn qwen_workspace_from_path(path: &Path) -> (Option<String>, Option<String>) {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').collect();
    for i in (3..parts.len()).rev() {
        if parts[i - 3] == "projects" && parts[i - 1] == "chats" {
            let key = normalize_workspace_key(parts[i - 2]);
            let label = key.as_deref().and_then(workspace_label_from_key);
            return (key, label);
        }
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_support::with_temp_home;

    #[test]
    fn parses_kimi_status_updates_and_skips_metadata() {
        with_temp_home(|home| {
            assert!(parse_kimi(DateRange::default()).is_empty());
            let dir = home.join(".kimi/sessions/ses-1");
            fs::create_dir_all(&dir).unwrap();
            let lines = [
                r#"{"type":"metadata","model":"kimi-k2"}"#,
                r#"{"type":"message","message":{"type":"UserMessage","payload":{}}}"#,
                r#"{"type":"message","timestamp":1700000000,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":100,"output":50,"input_cache_read":20,"input_cache_creation":5},"message_id":"msg-1"}}}"#,
            ];
            fs::write(dir.join("wire.jsonl"), lines.join("\n")).unwrap();

            let res = parse_kimi(DateRange::default());
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].source, "kimi");
            assert_eq!(res[0].provider, "moonshot");
            assert_eq!(res[0].input, 100.0);
            assert_eq!(res[0].output, 50.0);
            assert_eq!(res[0].cache_input, 20.0);
            assert_eq!(res[0].cache_output, 5.0);
        });
    }

    #[test]
    fn parses_openclaw_without_duplicate_counting() {
        with_temp_home(|home| {
            assert!(parse_openclaw(DateRange::default()).is_empty());
            let dir = home.join(".openclaw/agents");
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("sessions.json"),
                r#"{"session-1":{"sessionId":"session-1","sessionFile":"session-1.jsonl"}}"#,
            )
            .unwrap();
            let lines = [
                r#"{"type":"model_change","modelId":"gpt-4o","provider":"openai"}"#,
                r#"{"type":"message","message":{"role":"user","usage":{"input":100,"output":50}}}"#,
                r#"{"type":"message","message":{"role":"assistant","model":"gpt-4o","provider":"openai","usage":{"input":100,"output":50,"cacheRead":20,"cacheWrite":5},"timestamp":1700000000000}}"#,
            ];
            fs::write(dir.join("session-1.jsonl"), lines.join("\n")).unwrap();

            let res = parse_openclaw(DateRange::default());
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].source, "openclaw");
            assert_eq!(res[0].model_id, "gpt-4o");
            assert_eq!(res[0].provider, "openai");
            assert_eq!(res[0].input, 100.0);
            assert_eq!(res[0].output, 50.0);
            assert_eq!(res[0].cache_input, 20.0);
            assert_eq!(res[0].cache_output, 5.0);
        });
    }

    #[test]
    fn parses_pi_sessions_and_aborts_on_invalid_header() {
        with_temp_home(|home| {
            assert!(parse_pi(DateRange::default()).is_empty());
            let dir = home.join(".pi/agent/sessions");
            fs::create_dir_all(&dir).unwrap();
            let valid_lines = [
                r#"{"type":"session","id":"ses-1","cwd":"/home/alice/project"}"#,
                r#"{"type":"message","message":{"role":"user","model":"gpt-4o","provider":"openai","usage":{"input":100,"output":50}}}"#,
                r#"{"type":"message","timestamp":"2024-12-01T10:00:00Z","message":{"role":"assistant","model":"gpt-4o","provider":"openai","usage":{"input":100,"output":50,"cacheRead":10,"cacheWrite":5}}}"#,
            ];
            fs::write(dir.join("session.jsonl"), valid_lines.join("\n")).unwrap();

            let invalid_lines = [
                r#"{"type":"not-session","id":"ses-1"}"#,
                r#"{"type":"message","message":{"role":"assistant","model":"gpt-4o","provider":"openai","usage":{"input":100,"output":50}}}"#,
            ];
            fs::write(dir.join("bad.jsonl"), invalid_lines.join("\n")).unwrap();

            let res = parse_pi(DateRange::default());
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].source, "pi");
            assert_eq!(res[0].model_id, "gpt-4o");
            assert_eq!(res[0].input, 100.0);
            assert_eq!(res[0].output, 50.0);
            assert_eq!(res[0].cache_input, 10.0);
            assert_eq!(res[0].cache_output, 5.0);
            assert_eq!(res[0].project_path.as_deref(), Some("/home/alice/project"));
        });
    }

    #[test]
    fn parses_qwen_usage_metadata() {
        with_temp_home(|home| {
            assert!(parse_qwen(DateRange::default()).is_empty());
            let dir = home.join(".qwen/projects/myproject/chats");
            fs::create_dir_all(&dir).unwrap();
            let lines = [
                r#"{"type":"user","usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":50}}"#,
                r#"{"type":"assistant","usageMetadata":{"promptTokenCount":0,"candidatesTokenCount":0}}"#,
                r#"{"type":"assistant","model":"qwen-max","timestamp":"2024-12-01T10:00:00Z","usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":50,"thoughtsTokenCount":2,"cachedContentTokenCount":20}}"#,
            ];
            fs::write(dir.join("chat.jsonl"), lines.join("\n")).unwrap();

            let res = parse_qwen(DateRange::default());
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].source, "qwen");
            assert_eq!(res[0].model_id, "qwen-max");
            assert_eq!(res[0].provider, "qwen");
            assert_eq!(res[0].input, 100.0);
            assert_eq!(res[0].output, 50.0);
            assert_eq!(res[0].reasoning, 2.0);
            assert_eq!(res[0].cache_input, 20.0);
            assert_eq!(res[0].project_path.as_deref(), Some("myproject"));
        });
    }
}
