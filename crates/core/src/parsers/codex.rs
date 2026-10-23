use crate::cache::cached_file_messages;
use crate::common::{
    DateRange, RawUsageMessage, Tokens, build_raw_message, canonical_provider, derive_agent,
    extract_i64, extract_string, file_modified_timestamp_ms, file_predates_range,
    filter_messages_by_date_range, infer_provider_from_model, normalize_agent_name,
    normalize_tokens, normalize_workspace_key, parse_timestamp_value, resolve_home, scan_directory,
    workspace_label_from_key,
};
use memchr::memmem;
use rayon::prelude::*;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CodexTotals {
    input: i64,
    output: i64,
    cached: i64,
    reasoning: i64,
}

#[derive(Default)]
struct CodexParseState {
    current_model: Option<String>,
    previous_totals: Option<CodexTotals>,
    session_is_headless: bool,
    session_provider: Option<String>,
    session_agent: Option<String>,
    session_workspace_key: Option<String>,
    session_workspace_label: Option<String>,
}

pub fn parse_codex(range: DateRange) -> Vec<RawUsageMessage> {
    let mut files = vec![resolve_home("~/.codex/usage.jsonl")];
    let sessions_root = resolve_home("~/.codex/sessions");
    files.extend(scan_directory(&sessions_root, "*.jsonl"));

    let messages: Vec<RawUsageMessage> = files
        .into_par_iter()
        .filter(|p| p.exists() && !file_predates_range(p, range))
        .flat_map(|path| cached_file_messages(&path, || parse_codex_file(&path)))
        .collect();

    filter_messages_by_date_range(messages, range)
}

fn parse_codex_file(path: &Path) -> Vec<RawUsageMessage> {
    let fallback_ts = file_modified_timestamp_ms(path);
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut line_buf = String::new();
    let mut state = CodexParseState::default();
    let mut messages: Vec<RawUsageMessage> = Vec::new();
    let mut pending_model_messages: Vec<RawUsageMessage> = Vec::new();

    let meta_finder = memmem::Finder::new(b"\"session_meta\"");
    let turn_finder = memmem::Finder::new(b"\"turn_context\"");
    let token_finder = memmem::Finder::new(b"\"token_count\"");
    let usage_finder = memmem::Finder::new(b"\"usage\"");

    while reader.read_line(&mut line_buf).unwrap_or(0) > 0 {
        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            line_buf.clear();
            continue;
        }
        let bytes = trimmed.as_bytes();
        if meta_finder.find(bytes).is_none()
            && turn_finder.find(bytes).is_none()
            && token_finder.find(bytes).is_none()
            && usage_finder.find(bytes).is_none()
        {
            line_buf.clear();
            continue;
        }

        let Ok(Value::Object(entry)) = serde_json::from_str::<Value>(trimmed) else {
            line_buf.clear();
            continue;
        };

        let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let mut handled = false;

        if entry_type == "session_meta" {
            if let Some(payload) = entry.get("payload").and_then(|v| v.as_object()) {
                if payload.get("source").and_then(|v| v.as_str()) == Some("exec") {
                    state.session_is_headless = true;
                }
                if let Some(p) = payload.get("model_provider").and_then(extract_string) {
                    state.session_provider = Some(p);
                }
                if let Some(a) = payload.get("agent_nickname").and_then(extract_string) {
                    state.session_agent = Some(a);
                }
                if let Some(cwd) = payload.get("cwd").and_then(extract_string) {
                    state.session_workspace_key = normalize_workspace_key(&cwd);
                    state.session_workspace_label = state
                        .session_workspace_key
                        .as_deref()
                        .and_then(workspace_label_from_key);
                }
            }
            handled = true;
        }

        if entry_type == "turn_context" {
            if let Some(payload) = entry.get("payload").and_then(|v| v.as_object()) {
                if let Some(model) = extract_codex_model(payload) {
                    state.current_model = Some(model.clone());
                    flush_pending(&mut pending_model_messages, &mut messages, &model);
                }
            }
            handled = true;
        }

        if entry_type == "event_msg" {
            if let Some(payload) = entry.get("payload").and_then(|v| v.as_object()) {
                if payload.get("type").and_then(|v| v.as_str()) == Some("token_count") {
                    if let Some(m) = extract_codex_model(payload) {
                        state.current_model = Some(m.clone());
                        flush_pending(&mut pending_model_messages, &mut messages, &m);
                    }
                    if let Some(info) = payload.get("info").and_then(|v| v.as_object()) {
                        if let Some(m) = info
                            .get("model")
                            .or_else(|| info.get("model_name"))
                            .and_then(extract_string)
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                        {
                            state.current_model = Some(m.clone());
                            flush_pending(&mut pending_model_messages, &mut messages, &m);
                        }

                        let total_usage = info
                            .get("total_token_usage")
                            .and_then(|v| v.as_object())
                            .map(totals_from_usage);
                        let last_usage = info
                            .get("last_token_usage")
                            .and_then(|v| v.as_object())
                            .map(totals_from_usage);

                        let (tokens_opt, next_totals) =
                            compute_codex_tokens(total_usage, last_usage, state.previous_totals);
                        if let Some(tokens) = tokens_opt {
                            state.previous_totals = next_totals;
                            let timestamp = entry
                                .get("timestamp")
                                .and_then(parse_timestamp_value)
                                .unwrap_or(fallback_ts);
                            let agent = if state.session_is_headless {
                                Some("headless")
                            } else {
                                state.session_agent.as_deref()
                            };
                            let provider = state.session_provider.as_deref().unwrap_or("openai");
                            let msg = build_codex_message(
                                state.current_model.as_deref().unwrap_or("unknown"),
                                provider,
                                timestamp,
                                tokens,
                                agent,
                                state.session_workspace_label.clone(),
                                state.session_workspace_key.clone(),
                            );
                            if state.current_model.is_some() {
                                messages.push(msg);
                            } else {
                                pending_model_messages.push(msg);
                            }
                        }
                    }
                    handled = true;
                }
            }
        }

        if !handled {
            if let Some(mut headless) = parse_codex_headless_entry(&entry, &mut state, fallback_ts)
            {
                headless.project_name = state.session_workspace_label.clone();
                headless.project_path = state.session_workspace_key.clone();
                messages.push(headless);
            }
        }

        line_buf.clear();
    }

    if !pending_model_messages.is_empty() {
        flush_pending(&mut pending_model_messages, &mut messages, "unknown");
    }

    messages
}

fn flush_pending(
    pending: &mut Vec<RawUsageMessage>,
    messages: &mut Vec<RawUsageMessage>,
    model: &str,
) {
    for mut msg in pending.drain(..) {
        msg.model_id = model.to_string();
        messages.push(msg);
    }
}

fn extract_codex_model(payload: &serde_json::Map<String, Value>) -> Option<String> {
    payload
        .get("model_info")
        .and_then(|m| m.get("slug"))
        .and_then(extract_string)
        .or_else(|| payload.get("model").and_then(extract_string))
        .or_else(|| payload.get("model_name").and_then(extract_string))
        .or_else(|| {
            payload
                .get("info")
                .and_then(|i| i.get("model").or_else(|| i.get("model_name")))
                .and_then(extract_string)
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn totals_from_usage(u: &serde_json::Map<String, Value>) -> CodexTotals {
    let input = u
        .get("input_tokens")
        .and_then(extract_i64)
        .unwrap_or(0)
        .max(0);
    let output = u
        .get("output_tokens")
        .and_then(extract_i64)
        .unwrap_or(0)
        .max(0);
    let cached_in = u
        .get("cached_input_tokens")
        .and_then(extract_i64)
        .unwrap_or(0);
    let cache_read = u
        .get("cache_read_input_tokens")
        .and_then(extract_i64)
        .unwrap_or(0);
    let reasoning = u
        .get("reasoning_output_tokens")
        .and_then(extract_i64)
        .unwrap_or(0)
        .max(0);
    CodexTotals {
        input,
        output,
        cached: cached_in.max(cache_read).max(0),
        reasoning,
    }
}

fn compute_codex_tokens(
    total: Option<CodexTotals>,
    last: Option<CodexTotals>,
    previous: Option<CodexTotals>,
) -> (Option<Tokens>, Option<CodexTotals>) {
    match (total, last, previous) {
        (Some(tot), Some(lst), Some(prev)) => {
            if tot == prev {
                return (None, Some(prev));
            }
            if delta_from(tot, prev).is_none() && looks_like_stale_regression(tot, prev, lst) {
                return (None, Some(prev));
            }
            (Some(totals_to_tokens(lst)), Some(tot))
        }
        (Some(tot), Some(lst), None) => (Some(totals_to_tokens(lst)), Some(tot)),
        (Some(tot), None, Some(prev)) => {
            if tot == prev {
                return (None, Some(prev));
            }
            if let Some(delta) = delta_from(tot, prev) {
                (Some(totals_to_tokens(delta)), Some(tot))
            } else {
                (None, Some(tot))
            }
        }
        (Some(tot), None, None) => (Some(totals_to_tokens(tot)), Some(tot)),
        (None, Some(lst), Some(prev)) => (
            Some(totals_to_tokens(lst)),
            Some(CodexTotals {
                input: prev.input + lst.input,
                output: prev.output + lst.output,
                cached: prev.cached + lst.cached,
                reasoning: prev.reasoning + lst.reasoning,
            }),
        ),
        (None, Some(lst), None) => (Some(totals_to_tokens(lst)), None),
        _ => (None, previous),
    }
}

fn delta_from(curr: CodexTotals, prev: CodexTotals) -> Option<CodexTotals> {
    if curr.input < prev.input
        || curr.output < prev.output
        || curr.cached < prev.cached
        || curr.reasoning < prev.reasoning
    {
        return None;
    }
    Some(CodexTotals {
        input: curr.input - prev.input,
        output: curr.output - prev.output,
        cached: curr.cached - prev.cached,
        reasoning: curr.reasoning - prev.reasoning,
    })
}

fn looks_like_stale_regression(curr: CodexTotals, prev: CodexTotals, lst: CodexTotals) -> bool {
    let prev_total = prev.input + prev.output + prev.cached + prev.reasoning;
    let curr_total = curr.input + curr.output + curr.cached + curr.reasoning;
    let last_total = lst.input + lst.output + lst.cached + lst.reasoning;
    if prev_total <= 0 || curr_total <= 0 || last_total <= 0 {
        return false;
    }
    curr_total * 100 >= prev_total * 98 || curr_total + last_total * 2 >= prev_total
}

fn totals_to_tokens(t: CodexTotals) -> Tokens {
    let clamped_cached = t.cached.min(t.input);
    normalize_tokens(
        t.input - clamped_cached,
        t.output,
        clamped_cached,
        0,
        t.reasoning,
    )
}

fn build_codex_message(
    model: &str,
    provider: &str,
    timestamp: f64,
    tokens: Tokens,
    agent: Option<&str>,
    project_name: Option<String>,
    project_path: Option<String>,
) -> RawUsageMessage {
    let norm_agent = agent.map(normalize_agent_name);
    let resolved_provider = canonical_provider(provider).unwrap_or_else(|| {
        let inferred = infer_provider_from_model(model);
        if inferred == "unknown" {
            "openai".to_string()
        } else {
            inferred
        }
    });
    build_raw_message(
        "codex",
        &derive_agent(norm_agent.as_deref()),
        timestamp,
        model.to_string(),
        resolved_provider,
        tokens,
        project_name,
        project_path,
    )
}

fn parse_codex_headless_entry(
    value: &serde_json::Map<String, Value>,
    state: &mut CodexParseState,
    fallback_ts: f64,
) -> Option<RawUsageMessage> {
    if let Some(m) = extract_codex_model_from_value(value) {
        state.current_model = Some(m);
    }
    let usage = value
        .get("usage")
        .or_else(|| value.get("data").and_then(|d| d.get("usage")))
        .or_else(|| value.get("result").and_then(|r| r.get("usage")))
        .or_else(|| value.get("response").and_then(|r| r.get("usage")))?
        .as_object()?;

    let input_tokens = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .or_else(|| usage.get("input"))
        .and_then(extract_i64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .or_else(|| usage.get("output"))
        .and_then(extract_i64)
        .unwrap_or(0);
    let cached_tokens = usage
        .get("cached_input_tokens")
        .or_else(|| usage.get("cache_read_input_tokens"))
        .or_else(|| usage.get("cached_tokens"))
        .and_then(extract_i64)
        .unwrap_or(0);

    let input = (input_tokens - cached_tokens).max(0);
    if input == 0 && output_tokens == 0 && cached_tokens == 0 {
        return None;
    }

    let model = extract_codex_model_from_value(value)
        .or_else(|| state.current_model.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let timestamp = value
        .get("timestamp")
        .or_else(|| value.get("time"))
        .or_else(|| value.get("created_at"))
        .or_else(|| value.get("data").and_then(|d| d.get("timestamp")))
        .and_then(parse_timestamp_value)
        .unwrap_or(fallback_ts);
    let provider = state.session_provider.as_deref().unwrap_or("openai");
    let agent = if state.session_is_headless {
        Some("headless")
    } else {
        state.session_agent.as_deref()
    };

    Some(build_codex_message(
        &model,
        provider,
        timestamp,
        normalize_tokens(input, output_tokens, cached_tokens, 0, 0),
        agent,
        None,
        None,
    ))
}

fn extract_codex_model_from_value(value: &serde_json::Map<String, Value>) -> Option<String> {
    value
        .get("model")
        .or_else(|| value.get("model_name"))
        .or_else(|| value.get("data").and_then(|d| d.get("model")))
        .or_else(|| value.get("data").and_then(|d| d.get("model_name")))
        .or_else(|| value.get("response").and_then(|r| r.get("model")))
        .and_then(extract_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_support::with_temp_home;
    use std::fs;

    #[test]
    fn returns_empty_when_no_codex_usage_file_exists() {
        with_temp_home(|_| {
            assert!(parse_codex(DateRange::default()).is_empty());
        });
    }

    #[test]
    fn parses_headless_usage_line() {
        with_temp_home(|home| {
            let dir = home.join(".codex");
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("usage.jsonl"),
                r#"{"type":"turn.completed","model":"gpt-4o-mini","usage":{"input_tokens":120,"cached_input_tokens":20,"output_tokens":30}}"#,
            )
            .unwrap();

            let res = parse_codex(DateRange::default());
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].model_id, "gpt-4o-mini");
            assert_eq!(res[0].input, 100.0);
            assert_eq!(res[0].output, 30.0);
            assert_eq!(res[0].cache_input, 20.0);
        });
    }

    #[test]
    fn computes_token_deltas_and_skips_stale_regressions() {
        with_temp_home(|home| {
            let dir = home.join(".codex");
            fs::create_dir_all(&dir).unwrap();
            let lines = [
                r#"{"type":"session_meta","payload":{"source":"chat","model_provider":"openai","agent_nickname":"builder","cwd":"/Users/alice/codex-demo"}}"#,
                r#"{"type":"turn_context","payload":{"model":"gpt-5.2"}}"#,
                r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":5},"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":5}}}}"#,
                r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":110,"cached_input_tokens":22,"output_tokens":33,"reasoning_output_tokens":6},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1}}}}"#,
            ];
            fs::write(dir.join("usage.jsonl"), lines.join("\n")).unwrap();

            let res = parse_codex(DateRange::default());
            assert_eq!(res.len(), 2);
            assert_eq!(res[0].input, 80.0);
            assert_eq!(res[0].output, 30.0);
            assert_eq!(res[0].cache_input, 20.0);
            assert_eq!(res[0].reasoning, 5.0);
            assert_eq!(
                res[0].project_path.as_deref(),
                Some("/Users/alice/codex-demo")
            );
            assert_eq!(res[1].input, 8.0);
            assert_eq!(res[1].output, 3.0);
            assert_eq!(res[1].cache_input, 2.0);
            assert_eq!(res[1].reasoning, 1.0);
        });
    }
}
