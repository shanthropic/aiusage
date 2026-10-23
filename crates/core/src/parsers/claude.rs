use crate::cache::cached_file_messages;
use crate::common::{
    build_raw_message, derive_agent, extract_i64, extract_string, file_modified_timestamp_ms,
    file_predates_range, filter_messages_by_date_range, normalize_agent_name, normalize_tokens,
    normalize_workspace_key, parse_timestamp_value, resolve_home, scan_directory,
    workspace_label_from_key, DateRange, RawUsageMessage, Tokens,
};
use memchr::memmem;
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Deserialize)]
struct AgentMetaFile {
    #[serde(rename = "agentType")]
    agent_type: Option<String>,
}

pub fn parse_claude(range: DateRange) -> Vec<RawUsageMessage> {
    let parent_cache: Mutex<HashMap<PathBuf, HashMap<String, String>>> =
        Mutex::new(HashMap::new());

    let mut files = vec![
        resolve_home("~/.claude/usage.jsonl"),
        resolve_home("~/.config/claude/usage.jsonl"),
    ];
    let project_root = resolve_home("~/.claude/projects");
    files.extend(scan_directory(&project_root, "*.jsonl"));

    let messages: Vec<RawUsageMessage> = files
        .into_par_iter()
        .filter(|p| p.exists() && !file_predates_range(p, range))
        .flat_map(|path| {
            cached_file_messages(&path, || parse_claude_file(&path, &parent_cache))
        })
        .collect();

    filter_messages_by_date_range(messages, range)
}

fn parse_claude_file(
    path: &Path,
    parent_cache: &Mutex<HashMap<PathBuf, HashMap<String, String>>>,
) -> Vec<RawUsageMessage> {
    let (ws_key_from_path, ws_label_from_path) = claude_workspace_from_path(path);
    let mut session_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let fallback_ts = file_modified_timestamp_ms(path);

    if path.extension().and_then(|e| e.to_str()) == Some("json") {
        let json_msgs = parse_claude_headless_json(
            path,
            &session_id,
            fallback_ts,
            ws_key_from_path.clone(),
            ws_label_from_path.clone(),
        );
        if !json_msgs.is_empty() {
            return json_msgs;
        }
    }

    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut line_buf = String::new();

    let mut cwd_key: Option<String> = None;
    let mut cwd_label: Option<String> = None;

    let mut messages: Vec<RawUsageMessage> = Vec::new();
    let mut processed_hashes: HashMap<String, usize> = HashMap::new();
    let mut headless_state = HeadlessState::default();
    let mut sidechain_agent: Option<String> = None;
    let mut sidechain_detected = false;

    let usage_finder = memmem::Finder::new(b"\"usage\"");
    let cwd_finder = memmem::Finder::new(b"\"cwd\"");
    let sidechain_finder = memmem::Finder::new(b"\"isSidechain\"");
    let msg_finder = memmem::Finder::new(b"\"message_");

    while reader.read_line(&mut line_buf).unwrap_or(0) > 0 {
        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            line_buf.clear();
            continue;
        }
        let bytes = trimmed.as_bytes();

        if sidechain_detected
            && (cwd_key.is_some() || cwd_finder.find(bytes).is_none())
            && usage_finder.find(bytes).is_none()
            && msg_finder.find(bytes).is_none()
        {
            line_buf.clear();
            continue;
        }

        let Ok(Value::Object(entry)) = serde_json::from_str::<Value>(trimmed) else {
            line_buf.clear();
            continue;
        };

        if cwd_key.is_none() && cwd_finder.find(bytes).is_some() {
            if let Some(cwd) = entry.get("cwd").and_then(extract_string) {
                if let Some(key) = normalize_workspace_key(&cwd) {
                    cwd_label = workspace_label_from_key(&key);
                    cwd_key = Some(key);
                }
            }
        }

        if !sidechain_detected {
            sidechain_detected = true;
            if sidechain_finder.find(bytes).is_some()
                && entry.get("isSidechain").and_then(|v| v.as_bool()) == Some(true)
            {
                let entry_session_id = entry.get("sessionId").and_then(extract_string);
                if let Some(ref sid) = entry_session_id {
                    session_id = sid.clone();
                }
                let entry_agent_id = entry.get("agentId").and_then(extract_string);
                sidechain_agent = Some(resolve_subagent_name(
                    path,
                    entry_session_id.as_deref(),
                    entry_agent_id.as_deref(),
                    parent_cache,
                ));
            }
        }

        let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if entry_type == "user" {
            line_buf.clear();
            continue;
        }

        if entry_type == "assistant" {
            if let Some((model, usage, msg_id)) = parse_claude_message(&entry) {
                let req_id = entry.get("requestId").and_then(extract_string);
                let dedup_key = match (msg_id, req_id) {
                    (Some(mid), Some(rid)) => Some(format!("{mid}:{rid}")),
                    _ => None,
                };

                if let Some(ref key) = dedup_key {
                    if let Some(&idx) = processed_hashes.get(key) {
                        let existing = &mut messages[idx];
                        existing.input = existing.input.max(usage.input as f64);
                        existing.output = existing.output.max(usage.output as f64);
                        existing.cache_input = existing.cache_input.max(usage.cache_input as f64);
                        existing.cache_output =
                            existing.cache_output.max(usage.cache_output as f64);
                        existing.reasoning = existing.reasoning.max(usage.reasoning as f64);
                        line_buf.clear();
                        continue;
                    }
                }

                let timestamp = entry
                    .get("timestamp")
                    .and_then(parse_timestamp_value)
                    .unwrap_or(fallback_ts);
                let agent_norm = sidechain_agent.as_deref().map(normalize_agent_name);
                let unified = build_raw_message(
                    "claude",
                    &derive_agent(agent_norm.as_deref()),
                    timestamp,
                    model,
                    "anthropic".to_string(),
                    usage,
                    cwd_label.clone().or_else(|| ws_label_from_path.clone()),
                    cwd_key.clone().or_else(|| ws_key_from_path.clone()),
                );
                if let Some(key) = dedup_key {
                    processed_hashes.insert(key, messages.len());
                }
                messages.push(unified);
            }
            line_buf.clear();
            continue;
        }

        if let Some(mut headless) =
            process_claude_headless_entry(&entry, &mut headless_state, fallback_ts)
        {
            headless.project_name = cwd_label.clone().or_else(|| ws_label_from_path.clone());
            headless.project_path = cwd_key.clone().or_else(|| ws_key_from_path.clone());
            messages.push(headless);
        }

        line_buf.clear();
    }

    if let Some(mut finalized) = finalize_headless_state(&mut headless_state, fallback_ts) {
        finalized.project_name = cwd_label.clone().or_else(|| ws_label_from_path.clone());
        finalized.project_path = cwd_key.clone().or_else(|| ws_key_from_path.clone());
        messages.push(finalized);
    }

    if cwd_key.is_some() {
        for msg in &mut messages {
            if msg.project_path.is_none() || msg.project_path == ws_key_from_path {
                msg.project_path = cwd_key.clone();
                msg.project_name = cwd_label.clone();
            }
        }
    }

    messages
}

fn parse_claude_message(
    entry: &serde_json::Map<String, Value>,
) -> Option<(String, Tokens, Option<String>)> {
    let msg = entry.get("message")?.as_object()?;
    let model = msg.get("model").and_then(extract_string)?;
    let usage_obj = msg.get("usage")?.as_object()?;
    let input = usage_obj
        .get("input_tokens")
        .and_then(extract_i64)
        .unwrap_or(0);
    let output = usage_obj
        .get("output_tokens")
        .and_then(extract_i64)
        .unwrap_or(0);
    let cache_read = usage_obj
        .get("cache_read_input_tokens")
        .and_then(extract_i64)
        .unwrap_or(0);
    let cache_write = usage_obj
        .get("cache_creation_input_tokens")
        .and_then(extract_i64)
        .unwrap_or(0);
    let tokens = normalize_tokens(input, output, cache_read, cache_write, 0);
    let id = msg.get("id").and_then(extract_string);
    Some((model, tokens, id))
}

fn claude_workspace_from_path(path: &Path) -> (Option<String>, Option<String>) {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').collect();
    for i in 0..parts.len().saturating_sub(2) {
        if parts[i] == ".claude" && parts[i + 1] == "projects" {
            let key = normalize_workspace_key(parts[i + 2]);
            let label = key.as_deref().and_then(workspace_label_from_key);
            return (key, label);
        }
    }
    (None, None)
}

fn resolve_subagent_name(
    path: &Path,
    parent_session_id: Option<&str>,
    entry_agent_id: Option<&str>,
    parent_cache: &Mutex<HashMap<PathBuf, HashMap<String, String>>>,
) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    if let Some(dir) = path.parent() {
        let meta_path = dir.join(format!("{stem}.meta.json"));
        if let Ok(meta_text) = fs::read_to_string(meta_path) {
            if let Ok(meta) = serde_json::from_str::<AgentMetaFile>(&meta_text) {
                if let Some(agent_type) = meta.agent_type {
                    if !agent_type.trim().is_empty() {
                        return normalize_agent_name(agent_type.trim());
                    }
                }
            }
        }
    }

    let lookup_id = entry_agent_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| sidechain_agent_id_from_stem(&stem));

    if let (Some(parent_sid), Some(target_aid)) = (parent_session_id, lookup_id) {
        if let Some(parent_path) = find_parent_session_path(path, parent_sid) {
            if let Some(sub_type) =
                lookup_subagent_type_in_parent(&parent_path, &target_aid, parent_cache)
            {
                return normalize_agent_name(&sub_type);
            }
        }
    }

    normalize_agent_name("claude-code-subagent")
}

fn sidechain_agent_id_from_stem(stem: &str) -> Option<String> {
    let agent_stem = stem.strip_prefix("agent-")?;
    if !agent_stem.contains('-') {
        return Some(agent_stem.to_string());
    }
    let trailing = agent_stem.split('-').last()?;
    if trailing.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(trailing.to_string());
    }
    Some(agent_stem.to_string())
}

fn find_parent_session_path(sidechain_path: &Path, parent_session_id: &str) -> Option<PathBuf> {
    let parent_file = format!("{parent_session_id}.jsonl");
    let dir = sidechain_path.parent()?;
    if dir.file_name().and_then(|s| s.to_str()) == Some("subagents") {
        if let Some(project_dir) = dir.parent() {
            let candidate = project_dir.join(&parent_file);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    let candidate = dir.join(parent_file);
    if candidate.exists() {
        return Some(candidate);
    }
    None
}

fn lookup_subagent_type_in_parent(
    parent_path: &Path,
    target_agent_id: &str,
    parent_cache: &Mutex<HashMap<PathBuf, HashMap<String, String>>>,
) -> Option<String> {
    if let Ok(cache) = parent_cache.lock() {
        if let Some(map) = cache.get(parent_path) {
            return map.get(target_agent_id).cloned();
        }
    }
    let built = build_parent_subagent_type_lookup(parent_path).unwrap_or_default();
    let res = built.get(target_agent_id).cloned();
    if let Ok(mut cache) = parent_cache.lock() {
        cache.insert(parent_path.to_path_buf(), built);
    }
    res
}

fn build_parent_subagent_type_lookup(parent_path: &Path) -> Option<HashMap<String, String>> {
    let file = File::open(parent_path).ok()?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut line_buf = String::new();
    let mut tool_use_types: HashMap<String, String> = HashMap::new();
    let mut agent_id_links: HashMap<String, String> = HashMap::new();

    let subagent_finder = memmem::Finder::new(b"subagent_type");
    let agent_id_finder = memmem::Finder::new(b"agentId: ");

    while reader.read_line(&mut line_buf).unwrap_or(0) > 0 {
        let trimmed = line_buf.trim();
        let bytes = trimmed.as_bytes();
        if subagent_finder.find(bytes).is_none() && agent_id_finder.find(bytes).is_none() {
            line_buf.clear();
            continue;
        }
        if let Ok(Value::Object(entry)) = serde_json::from_str::<Value>(trimmed) {
            if let Some(content) = entry
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for block in content {
                    let Some(b) = block.as_object() else { continue };
                    let btype = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if btype == "tool_use" {
                        let id = b.get("id").and_then(extract_string);
                        let st = b
                            .get("input")
                            .and_then(|i| i.get("subagent_type"))
                            .and_then(extract_string);
                        if let (Some(id), Some(st)) = (id, st) {
                            tool_use_types.insert(id, st);
                        }
                    } else if btype == "tool_result" {
                        let Some(tool_use_id) = b.get("tool_use_id").and_then(extract_string)
                        else {
                            continue;
                        };
                        if let Some(arr) = b.get("content").and_then(|c| c.as_array()) {
                            for cb in arr {
                                if let Some(text) = cb.get("text").and_then(|t| t.as_str()) {
                                    if let Some(aid) = extract_agent_id_from_text(text) {
                                        agent_id_links.insert(tool_use_id, aid);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        line_buf.clear();
    }

    let mut result = HashMap::new();
    for (tool_use_id, agent_id) in agent_id_links {
        if let Some(st) = tool_use_types.get(&tool_use_id) {
            result.insert(agent_id, st.clone());
        }
    }
    Some(result)
}

fn extract_agent_id_from_text(text: &str) -> Option<String> {
    let marker = "agentId: ";
    let pos = text.find(marker)?;
    let rest = &text[pos + marker.len()..];
    let id: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

#[derive(Default)]
struct HeadlessState {
    model: Option<String>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    timestamp_ms: Option<f64>,
}

fn parse_claude_headless_json(
    path: &Path,
    _session_id: &str,
    fallback_ts: f64,
    ws_key: Option<String>,
    ws_label: Option<String>,
) -> Vec<RawUsageMessage> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    if let Some(mut msg) = extract_claude_headless_message(&map, fallback_ts) {
        msg.project_name = ws_label;
        msg.project_path = ws_key;
        return vec![msg];
    }
    Vec::new()
}

fn process_claude_headless_entry(
    value: &serde_json::Map<String, Value>,
    state: &mut HeadlessState,
    fallback_ts: f64,
) -> Option<RawUsageMessage> {
    let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match event_type {
        "message_start" => {
            let completed = finalize_headless_state(state, fallback_ts);
            if let Some(m) = extract_claude_model(value) {
                state.model = Some(m);
            }
            state.timestamp_ms = extract_claude_timestamp(value).or(state.timestamp_ms);
            if let Some(usage) = value
                .get("message")
                .and_then(|m| m.get("usage"))
                .or_else(|| value.get("usage"))
                .and_then(|u| u.as_object())
            {
                update_claude_usage(state, usage);
            }
            completed
        }
        "message_delta" => {
            if let Some(usage) = value
                .get("usage")
                .or_else(|| value.get("delta").and_then(|d| d.get("usage")))
                .and_then(|u| u.as_object())
            {
                update_claude_usage(state, usage);
            }
            None
        }
        "message_stop" => finalize_headless_state(state, fallback_ts),
        _ => extract_claude_headless_message(value, fallback_ts),
    }
}

fn extract_claude_headless_message(
    value: &serde_json::Map<String, Value>,
    fallback_ts: f64,
) -> Option<RawUsageMessage> {
    let usage = value
        .get("usage")
        .or_else(|| value.get("message").and_then(|m| m.get("usage")))?
        .as_object()?;
    let model = extract_claude_model(value)?;
    let timestamp = extract_claude_timestamp(value).unwrap_or(fallback_ts);
    let tokens = normalize_tokens(
        usage.get("input_tokens").and_then(extract_i64).unwrap_or(0),
        usage
            .get("output_tokens")
            .and_then(extract_i64)
            .unwrap_or(0),
        usage
            .get("cache_read_input_tokens")
            .and_then(extract_i64)
            .unwrap_or(0),
        usage
            .get("cache_creation_input_tokens")
            .and_then(extract_i64)
            .unwrap_or(0),
        0,
    );
    Some(build_raw_message(
        "claude",
        "default",
        timestamp,
        model,
        "anthropic".to_string(),
        tokens,
        None,
        None,
    ))
}

fn extract_claude_model(value: &serde_json::Map<String, Value>) -> Option<String> {
    value
        .get("model")
        .and_then(extract_string)
        .or_else(|| value.get("message").and_then(|m| m.get("model")).and_then(extract_string))
}

fn extract_claude_timestamp(value: &serde_json::Map<String, Value>) -> Option<f64> {
    value
        .get("timestamp")
        .or_else(|| value.get("created_at"))
        .or_else(|| value.get("message").and_then(|m| m.get("created_at")))
        .and_then(parse_timestamp_value)
}

fn update_claude_usage(state: &mut HeadlessState, usage: &serde_json::Map<String, Value>) {
    if let Some(v) = usage.get("input_tokens").and_then(extract_i64) {
        state.input = state.input.max(v);
    }
    if let Some(v) = usage.get("output_tokens").and_then(extract_i64) {
        state.output = state.output.max(v);
    }
    if let Some(v) = usage.get("cache_read_input_tokens").and_then(extract_i64) {
        state.cache_read = state.cache_read.max(v);
    }
    if let Some(v) = usage
        .get("cache_creation_input_tokens")
        .and_then(extract_i64)
    {
        state.cache_write = state.cache_write.max(v);
    }
}

fn finalize_headless_state(state: &mut HeadlessState, fallback_ts: f64) -> Option<RawUsageMessage> {
    let model = state.model.take()?;
    if state.input == 0 && state.output == 0 && state.cache_read == 0 && state.cache_write == 0 {
        state.timestamp_ms = None;
        return None;
    }
    let ts = state.timestamp_ms.take().unwrap_or(fallback_ts);
    let tokens = normalize_tokens(
        state.input,
        state.output,
        state.cache_read,
        state.cache_write,
        0,
    );
    state.input = 0;
    state.output = 0;
    state.cache_read = 0;
    state.cache_write = 0;

    Some(build_raw_message(
        "claude",
        "default",
        ts,
        model,
        "anthropic".to_string(),
        tokens,
        None,
        None,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_support::with_temp_home;

    #[test]
    fn returns_empty_when_no_claude_usage_file_exists() {
        with_temp_home(|_| {
            assert!(parse_claude(DateRange::default()).is_empty());
        });
    }

    #[test]
    fn parses_single_assistant_message() {
        with_temp_home(|home| {
            let dir = home.join(".claude");
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("usage.jsonl"),
                r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_001","message":{"id":"msg_001","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#,
            )
            .unwrap();

            let res = parse_claude(DateRange::default());
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].source, "claude");
            assert_eq!(res[0].model_id, "claude-3-5-sonnet");
            assert_eq!(res[0].input, 100.0);
            assert_eq!(res[0].output, 50.0);
            assert_eq!(res[0].cache_input, 0.0);
            assert_eq!(res[0].cache_output, 0.0);
            assert_eq!(res[0].r#type, "assistant");
        });
    }

    #[test]
    fn deduplicates_streaming_duplicates_with_per_field_max() {
        with_temp_home(|home| {
            let dir = home.join(".claude");
            fs::create_dir_all(&dir).unwrap();
            let lines = [
                r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_001","message":{"id":"msg_001","model":"claude-3-5-sonnet","usage":{"input_tokens":10,"output_tokens":31}}}"#,
                r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.100Z","requestId":"req_001","message":{"id":"msg_001","model":"claude-3-5-sonnet","usage":{"input_tokens":10,"output_tokens":31}}}"#,
                r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.200Z","requestId":"req_001","message":{"id":"msg_001","model":"claude-3-5-sonnet","usage":{"input_tokens":10,"output_tokens":300}}}"#,
            ];
            fs::write(dir.join("usage.jsonl"), lines.join("\n")).unwrap();

            let res = parse_claude(DateRange::default());
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].output, 300.0);
            assert_eq!(res[0].input, 10.0);
        });
    }

    #[test]
    fn detects_workspace_and_prefers_cwd_over_encoded_project_dir() {
        with_temp_home(|home| {
            let project_dir = home.join(".claude").join("projects").join("C--Users-test-Desktop");
            fs::create_dir_all(&project_dir).unwrap();
            fs::write(
                project_dir.join("session.jsonl"),
                r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","cwd":"C:\\Users\\test\\Desktop","message":{"model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#,
            )
            .unwrap();

            let res = parse_claude(DateRange::default());
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].project_path.as_deref(), Some("C:/Users/test/Desktop"));
            assert_eq!(res[0].project_name.as_deref(), Some("Desktop"));
        });
    }

    #[test]
    fn skips_user_messages_and_parses_headless_stream_events() {
        with_temp_home(|home| {
            let dir = home.join(".claude");
            fs::create_dir_all(&dir).unwrap();
            let lines = [
                r#"{"type":"user","timestamp":"2024-12-01T10:00:00.000Z","message":{"content":"Hello"}}"#,
                r#"{"type":"message_start","timestamp":"2025-01-01T00:00:00Z","message":{"id":"msg_1","model":"claude-3-5-sonnet","usage":{"input_tokens":200,"cache_read_input_tokens":20,"cache_creation_input_tokens":5}}}"#,
                r#"{"type":"message_delta","usage":{"output_tokens":80}}"#,
                r#"{"type":"message_stop"}"#,
            ];
            fs::write(dir.join("usage.jsonl"), lines.join("\n")).unwrap();

            let res = parse_claude(DateRange::default());
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].model_id, "claude-3-5-sonnet");
            assert_eq!(res[0].input, 200.0);
            assert_eq!(res[0].output, 80.0);
            assert_eq!(res[0].cache_input, 20.0);
            assert_eq!(res[0].cache_output, 5.0);
        });
    }
}
