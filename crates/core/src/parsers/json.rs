use crate::cache::cached_file_messages;
use crate::common::{
    build_raw_message, extract_f64, file_modified_timestamp_ms, file_predates_range,
    filter_messages_by_date_range, infer_provider_from_model, normalize_tokens,
    parse_timestamp_str, parse_timestamp_value, resolve_home, scan_directory, DateRange,
    RawUsageMessage,
};
use chrono::{Local, NaiveDate, TimeZone};
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;

// ─── Amp ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AmpThread {
    created: Option<f64>,
    messages: Option<Vec<AmpMessage>>,
    #[serde(rename = "usageLedger")]
    usage_ledger: Option<AmpUsageLedger>,
}

#[derive(Deserialize)]
struct AmpMessage {
    role: Option<String>,
    #[serde(rename = "messageId")]
    message_id: Option<i64>,
    usage: Option<AmpMessageUsage>,
}

#[derive(Deserialize)]
struct AmpMessageUsage {
    model: Option<String>,
    #[serde(rename = "inputTokens")]
    input_tokens: Option<i64>,
    #[serde(rename = "outputTokens")]
    output_tokens: Option<i64>,
    #[serde(rename = "cacheReadInputTokens")]
    cache_read_input_tokens: Option<i64>,
    #[serde(rename = "cacheCreationInputTokens")]
    cache_creation_input_tokens: Option<i64>,
    credits: Option<f64>,
}

#[derive(Deserialize)]
struct AmpUsageLedger {
    events: Option<Vec<AmpUsageEvent>>,
}

#[derive(Deserialize)]
struct AmpUsageEvent {
    timestamp: Option<String>,
    model: Option<String>,
    credits: Option<f64>,
    tokens: Option<AmpTokens>,
    #[serde(rename = "toMessageId")]
    to_message_id: Option<i64>,
}

#[derive(Deserialize)]
struct AmpTokens {
    input: Option<i64>,
    output: Option<i64>,
    #[serde(rename = "cacheReadInputTokens")]
    cache_read_input_tokens: Option<i64>,
    #[serde(rename = "cacheCreationInputTokens")]
    cache_creation_input_tokens: Option<i64>,
}

#[derive(Clone)]
struct AmpUsageRecord {
    model: String,
    timestamp: f64,
    has_explicit_timestamp: bool,
    message_id: Option<i64>,
    ledger_to_message_id: Option<i64>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    cost: f64,
}

pub fn parse_amp(range: DateRange) -> Vec<RawUsageMessage> {
    let root = resolve_home("~/.local/share/amp/threads");
    let files: Vec<_> = scan_directory(&root, "*.json")
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("T-"))
                .unwrap_or(false)
                && !file_predates_range(p, range)
        })
        .collect();

    let messages: Vec<RawUsageMessage> = files
        .into_par_iter()
        .flat_map(|path| cached_file_messages(&path, || parse_amp_file(&path)))
        .collect();

    filter_messages_by_date_range(messages, range)
}

fn parse_amp_file(path: &Path) -> Vec<RawUsageMessage> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let file_mtime = file_modified_timestamp_ms(path);
    let Ok(thread) = serde_json::from_str::<AmpThread>(&text) else {
        return Vec::new();
    };
    let thread_created = thread.created.unwrap_or(0.0);

    let mut ledger_records = Vec::new();
    if let Some(events) = thread.usage_ledger.and_then(|l| l.events) {
        for ev in events {
            let Some(model) = ev.model else { continue };
            let explicit_ts = ev.timestamp.as_deref().and_then(parse_timestamp_str);
            let ts = explicit_ts
                .filter(|&t| t != 0.0)
                .unwrap_or(if thread_created != 0.0 {
                    thread_created
                } else {
                    file_mtime
                });
            let t = ev.tokens.unwrap_or(AmpTokens {
                input: Some(0),
                output: Some(0),
                cache_read_input_tokens: Some(0),
                cache_creation_input_tokens: Some(0),
            });
            ledger_records.push(AmpUsageRecord {
                model,
                timestamp: ts,
                has_explicit_timestamp: explicit_ts.is_some(),
                message_id: None,
                ledger_to_message_id: ev.to_message_id.filter(|&id| id > 0),
                input: t.input.unwrap_or(0).max(0),
                output: t.output.unwrap_or(0).max(0),
                cache_read: t.cache_read_input_tokens.unwrap_or(0).max(0),
                cache_write: t.cache_creation_input_tokens.unwrap_or(0).max(0),
                cost: ev.credits.unwrap_or(0.0).max(0.0),
            });
        }
    }

    let mut message_records = Vec::new();
    let base_ts = if thread_created != 0.0 {
        thread_created
    } else {
        file_mtime
    };
    if let Some(msgs) = thread.messages {
        for msg in msgs {
            if msg.role.as_deref() != Some("assistant") {
                continue;
            }
            let Some(u) = msg.usage else { continue };
            let Some(model) = u.model else { continue };
            let mid = msg.message_id.unwrap_or(0);
            message_records.push(AmpUsageRecord {
                model,
                timestamp: base_ts + (mid as f64) * 1000.0,
                has_explicit_timestamp: false,
                message_id: if mid > 0 { Some(mid) } else { None },
                ledger_to_message_id: None,
                input: u.input_tokens.unwrap_or(0).max(0),
                output: u.output_tokens.unwrap_or(0).max(0),
                cache_read: u.cache_read_input_tokens.unwrap_or(0).max(0),
                cache_write: u.cache_creation_input_tokens.unwrap_or(0).max(0),
                cost: u.credits.unwrap_or(0.0).max(0.0),
            });
        }
    }

    let mut all_records = if ledger_records.is_empty() {
        message_records
    } else {
        let mut consumed = vec![false; ledger_records.len()];
        let mut search_start = 0;
        let mut unmatched = Vec::new();
        for mr in message_records {
            if let Some(idx) =
                find_matching_ledger(&ledger_records, &consumed, search_start, &mr)
            {
                consumed[idx] = true;
                search_start = idx + 1;
                if !ledger_records[idx].has_explicit_timestamp {
                    ledger_records[idx].timestamp = mr.timestamp;
                }
                if ledger_records[idx].cost <= 0.0 && mr.cost > 0.0 {
                    ledger_records[idx].cost = mr.cost;
                }
                ledger_records[idx].message_id = mr.message_id;
            } else {
                unmatched.push(mr);
            }
        }
        ledger_records.extend(unmatched);
        ledger_records
    };

    all_records.sort_by(|a, b| {
        a.timestamp
            .partial_cmp(&b.timestamp)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    all_records
        .into_iter()
        .map(|r| {
            let provider = infer_provider_from_model(&r.model);
            build_raw_message(
                "amp",
                "default",
                r.timestamp,
                r.model,
                provider,
                normalize_tokens(r.input, r.output, r.cache_read, r.cache_write, 0),
                None,
                None,
            )
        })
        .collect()
}

fn find_matching_ledger(
    ledger: &[AmpUsageRecord],
    consumed: &[bool],
    search_start: usize,
    mr: &AmpUsageRecord,
) -> Option<usize> {
    let order = (search_start..ledger.len()).chain(0..search_start);
    if mr.message_id.is_some() {
        for i in order.clone() {
            if !consumed[i] && ledger[i].ledger_to_message_id == mr.message_id {
                return Some(i);
            }
        }
    }
    for i in order {
        if !consumed[i]
            && ledger[i].model == mr.model
            && ledger[i].input == mr.input
            && ledger[i].output == mr.output
            && ledger[i].cache_read == mr.cache_read
            && ledger[i].cache_write == mr.cache_write
        {
            return Some(i);
        }
    }
    None
}

// ─── Codebuff ────────────────────────────────────────────────────────────────

pub fn parse_codebuff(range: DateRange) -> Vec<RawUsageMessage> {
    let channels = ["manicode", "manicode-dev", "manicode-staging"];
    let files: Vec<_> = channels
        .iter()
        .flat_map(|ch| scan_directory(&resolve_home(&format!("~/.config/{ch}/projects")), "*.json"))
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()) == Some("chat-messages.json")
                && !file_predates_range(p, range)
        })
        .collect();

    let messages: Vec<RawUsageMessage> = files
        .into_par_iter()
        .flat_map(|path| cached_file_messages(&path, || parse_codebuff_file(&path)))
        .collect();

    filter_messages_by_date_range(messages, range)
}

#[derive(Default)]
struct CbUsage {
    model: Option<String>,
    credits: f64,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
}

impl CbUsage {
    fn has_signal(&self) -> bool {
        self.input > 0
            || self.output > 0
            || self.cache_read > 0
            || self.cache_write > 0
            || self.credits > 0.0
    }
    fn merge_fallback(&mut self, other: CbUsage) {
        if self.input <= 0 {
            self.input = other.input;
        }
        if self.output <= 0 {
            self.output = other.output;
        }
        if self.cache_read <= 0 {
            self.cache_read = other.cache_read;
        }
        if self.cache_write <= 0 {
            self.cache_write = other.cache_write;
        }
        if self.model.is_none() {
            self.model = other.model;
        }
        if self.credits <= 0.0 {
            self.credits = other.credits;
        }
    }
}

fn parse_codebuff_file(path: &Path) -> Vec<RawUsageMessage> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };

    let chat_id_ts = derive_codebuff_chat_ts(path);
    let file_mtime = file_modified_timestamp_ms(path);
    let mut results = Vec::new();

    for item in items {
        let Some(msg) = item.as_object() else {
            continue;
        };
        let variant = msg
            .get("variant")
            .or_else(|| msg.get("role"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !matches!(variant, "ai" | "agent" | "assistant") {
            continue;
        }

        let usage = extract_cb_usage(msg);
        if !usage.has_signal() {
            continue;
        }

        let ts = msg
            .get("timestamp")
            .and_then(parse_timestamp_value)
            .or_else(|| msg.get("createdAt").and_then(parse_timestamp_value))
            .or_else(|| {
                msg.get("metadata")
                    .and_then(|m| m.get("timestamp"))
                    .and_then(parse_timestamp_value)
            })
            .or_else(|| chat_id_ts.filter(|&t| t > 0.0))
            .unwrap_or(file_mtime);

        let model = usage.model.unwrap_or_else(|| "codebuff-unknown".to_string());
        let provider = infer_provider_from_model(&model);
        results.push(build_raw_message(
            "codebuff",
            "default",
            ts,
            model,
            provider,
            normalize_tokens(
                usage.input,
                usage.output,
                usage.cache_read,
                usage.cache_write,
                0,
            ),
            None,
            None,
        ));
    }

    results
}

fn extract_cb_usage(msg: &serde_json::Map<String, Value>) -> CbUsage {
    let mut u = CbUsage::default();
    if let Some(meta) = msg.get("metadata").and_then(|m| m.as_object()) {
        if let Some(m) = meta.get("model").and_then(|v| v.as_str()) {
            u.model = Some(m.to_string());
        }
        if let Some(uo) = meta.get("usage").and_then(|v| v.as_object()) {
            u.merge_fallback(parse_cb_usage_obj(uo));
        }
        if let Some(cbu) = meta
            .get("codebuff")
            .and_then(|c| c.get("usage"))
            .and_then(|v| v.as_object())
        {
            u.merge_fallback(parse_cb_usage_obj(cbu));
        }
        if let Some(hist) = meta
            .get("runState")
            .and_then(|r| r.get("sessionState"))
            .and_then(|s| s.get("mainAgentState"))
            .and_then(|m| m.get("messageHistory"))
            .and_then(|h| h.as_array())
        {
            let mut acc = CbUsage::default();
            let mut found = false;
            for entry in hist.iter().rev() {
                if entry.get("role").and_then(|v| v.as_str()) != Some("assistant") {
                    continue;
                }
                let Some(po) = entry.get("providerOptions").and_then(|v| v.as_object()) else {
                    continue;
                };
                let mut eu = CbUsage::default();
                if let Some(pu) = po.get("usage").and_then(|v| v.as_object()) {
                    eu.merge_fallback(parse_cb_usage_obj(pu));
                }
                if let Some(cb) = po.get("codebuff").and_then(|v| v.as_object()) {
                    if let Some(cbu) = cb.get("usage").and_then(|v| v.as_object()) {
                        eu.merge_fallback(parse_cb_usage_obj(cbu));
                    }
                    if let Some(m) = cb.get("model").and_then(|v| v.as_str()) {
                        eu.model = Some(m.to_string());
                    }
                }
                if eu.has_signal() || eu.model.is_some() {
                    found = true;
                }
                acc.merge_fallback(eu);
            }
            if found {
                u.merge_fallback(acc);
            }
        }
    }
    if let Some(c) = msg.get("credits").and_then(extract_f64) {
        if c > 0.0 && u.credits <= 0.0 {
            u.credits = c;
        }
    }
    u
}

fn parse_cb_usage_obj(val: &serde_json::Map<String, Value>) -> CbUsage {
    let pick = |keys: &[&str]| -> Option<i64> {
        for &k in keys {
            if let Some(v) = val.get(k).and_then(extract_f64) {
                return Some(v as i64);
            }
        }
        None
    };
    let input = pick(&[
        "inputTokens",
        "input_tokens",
        "promptTokens",
        "prompt_tokens",
    ])
    .unwrap_or(0);
    let output = pick(&[
        "outputTokens",
        "output_tokens",
        "completionTokens",
        "completion_tokens",
    ])
    .unwrap_or(0);
    let cache_read = pick(&[
        "cacheReadInputTokens",
        "cache_read_input_tokens",
        "cachedTokensCreated",
        "cached_tokens_created",
    ])
    .or_else(|| {
        val.get("promptTokensDetails")
            .and_then(|d| d.get("cachedTokens").or_else(|| d.get("cached_tokens")))
            .and_then(extract_f64)
            .map(|f| f as i64)
    })
    .or_else(|| {
        val.get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(extract_f64)
            .map(|f| f as i64)
    })
    .unwrap_or(0);
    let cache_write = pick(&[
        "cacheCreationInputTokens",
        "cache_creation_input_tokens",
        "cacheCreationTokens",
        "cache_creation_tokens",
    ])
    .unwrap_or(0);

    CbUsage {
        model: val
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        credits: val.get("credits").and_then(extract_f64).unwrap_or(0.0),
        input,
        output,
        cache_read,
        cache_write,
    }
}

fn derive_codebuff_chat_ts(path: &Path) -> Option<f64> {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').collect();
    if parts.len() < 2 {
        return None;
    }
    let chat_id = parts[parts.len() - 2];
    let t_idx = chat_id.find('T')?;
    let rebuilt = format!(
        "{}{}",
        &chat_id[..t_idx],
        chat_id[t_idx..].replace('-', ":")
    );
    parse_timestamp_str(&rebuilt)
}

// ─── Droid ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DroidSettingsJson {
    model: Option<String>,
    #[serde(rename = "providerLock")]
    provider_lock: Option<String>,
    #[serde(rename = "providerLockTimestamp")]
    provider_lock_timestamp: Option<String>,
    #[serde(rename = "tokenUsage")]
    token_usage: Option<DroidTokenUsage>,
}

#[derive(Deserialize)]
struct DroidTokenUsage {
    #[serde(rename = "inputTokens")]
    input_tokens: Option<i64>,
    #[serde(rename = "outputTokens")]
    output_tokens: Option<i64>,
    #[serde(rename = "cacheCreationTokens")]
    cache_creation_tokens: Option<i64>,
    #[serde(rename = "cacheReadTokens")]
    cache_read_tokens: Option<i64>,
    #[serde(rename = "thinkingTokens")]
    thinking_tokens: Option<i64>,
}

pub fn parse_droid(range: DateRange) -> Vec<RawUsageMessage> {
    let root = resolve_home("~/.factory/sessions");
    let files: Vec<_> = scan_directory(&root, "*.json")
        .into_iter()
        .filter(|p| {
            p.to_string_lossy().ends_with(".settings.json") && !file_predates_range(p, range)
        })
        .collect();

    let messages: Vec<RawUsageMessage> = files
        .into_par_iter()
        .flat_map(|path| cached_file_messages(&path, || parse_droid_file(&path)))
        .collect();

    filter_messages_by_date_range(messages, range)
}

fn parse_droid_file(path: &Path) -> Vec<RawUsageMessage> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(settings) = serde_json::from_str::<DroidSettingsJson>(&text) else {
        return Vec::new();
    };
    let Some(usage) = settings.token_usage else {
        return Vec::new();
    };
    let input = usage.input_tokens.unwrap_or(0).max(0);
    let output = usage.output_tokens.unwrap_or(0).max(0);
    let cache_write = usage.cache_creation_tokens.unwrap_or(0).max(0);
    let cache_read = usage.cache_read_tokens.unwrap_or(0).max(0);
    let thinking = usage.thinking_tokens.unwrap_or(0).max(0);
    if input + output + cache_write + cache_read + thinking == 0 {
        return Vec::new();
    }

    let provider = settings
        .provider_lock
        .unwrap_or_else(|| infer_provider_from_model(settings.model.as_deref().unwrap_or("")));
    let model = if let Some(ref m) = settings.model {
        normalize_droid_model_name(m)
    } else {
        let jsonl_path = path.to_string_lossy().replace(".settings.json", ".jsonl");
        extract_droid_model_from_jsonl(Path::new(&jsonl_path))
            .unwrap_or_else(|| default_droid_model(&provider))
    };

    let timestamp = settings
        .provider_lock_timestamp
        .as_deref()
        .and_then(parse_timestamp_str)
        .unwrap_or_else(|| file_modified_timestamp_ms(path));
    if timestamp <= 0.0 {
        return Vec::new();
    }

    vec![build_raw_message(
        "droid",
        "default",
        timestamp,
        model,
        provider,
        normalize_tokens(input, output, cache_read, cache_write, thinking),
        None,
        None,
    )]
}

fn normalize_droid_model_name(model: &str) -> String {
    let stripped = model.strip_prefix("custom:").unwrap_or(model);
    let mut res = String::new();
    let mut in_bracket = false;
    for ch in stripped.chars() {
        match ch {
            '[' => in_bracket = true,
            ']' => in_bracket = false,
            _ if !in_bracket => res.push(ch),
            _ => {}
        }
    }
    let lowered = res.trim_end_matches('-').to_lowercase().replace('.', "-");
    let mut collapsed = String::new();
    let mut prev_hyphen = false;
    for ch in lowered.chars() {
        if ch == '-' {
            if !prev_hyphen {
                collapsed.push(ch);
            }
            prev_hyphen = true;
        } else {
            collapsed.push(ch);
            prev_hyphen = false;
        }
    }
    collapsed
}

fn default_droid_model(provider: &str) -> String {
    match provider.to_lowercase().as_str() {
        "anthropic" => "claude-unknown".to_string(),
        "openai" => "gpt-unknown".to_string(),
        "google" => "gemini-unknown".to_string(),
        "xai" => "grok-unknown".to_string(),
        _ => format!("{provider}-unknown"),
    }
}

fn extract_droid_model_from_jsonl(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    for _ in 0..500 {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if let Some(pos) = line.find("Model:") {
            let after = &line[pos + 6..];
            let part = after.split(&['[', '\\', '"'][..]).next()?.trim();
            if !part.is_empty() {
                return Some(normalize_droid_model_name(part));
            }
        }
    }
    None
}

// ─── Mux ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct MuxSessionUsage {
    #[serde(rename = "byModel")]
    by_model: Option<HashMap<String, MuxModelUsage>>,
    #[serde(rename = "lastRequest")]
    last_request: Option<MuxLastRequest>,
}

#[derive(Deserialize)]
struct MuxModelUsage {
    input: Option<MuxTokenBucket>,
    cached: Option<MuxTokenBucket>,
    #[serde(rename = "cacheCreate")]
    cache_create: Option<MuxTokenBucket>,
    output: Option<MuxTokenBucket>,
    reasoning: Option<MuxTokenBucket>,
}

#[derive(Deserialize)]
struct MuxTokenBucket {
    tokens: Option<i64>,
}

#[derive(Deserialize)]
struct MuxLastRequest {
    timestamp: Option<f64>,
}

pub fn parse_mux(range: DateRange) -> Vec<RawUsageMessage> {
    let root = resolve_home("~/.mux/sessions");
    let files: Vec<_> = scan_directory(&root, "*.json")
        .into_iter()
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()) == Some("session-usage.json")
                && !file_predates_range(p, range)
        })
        .collect();

    let messages: Vec<RawUsageMessage> = files
        .into_par_iter()
        .flat_map(|path| cached_file_messages(&path, || parse_mux_file(&path)))
        .collect();

    filter_messages_by_date_range(messages, range)
}

fn parse_mux_file(path: &Path) -> Vec<RawUsageMessage> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(usage) = serde_json::from_str::<MuxSessionUsage>(&text) else {
        return Vec::new();
    };
    let timestamp = usage
        .last_request
        .and_then(|r| r.timestamp)
        .unwrap_or_else(|| file_modified_timestamp_ms(path));
    let Some(by_model) = usage.by_model else {
        return Vec::new();
    };

    let mut messages = Vec::new();
    for (model_key, mu) in by_model {
        let input = mu.input.and_then(|b| b.tokens).unwrap_or(0).max(0);
        let cached = mu.cached.and_then(|b| b.tokens).unwrap_or(0).max(0);
        let cache_create = mu.cache_create.and_then(|b| b.tokens).unwrap_or(0).max(0);
        let output = mu.output.and_then(|b| b.tokens).unwrap_or(0).max(0);
        let reasoning = mu.reasoning.and_then(|b| b.tokens).unwrap_or(0).max(0);
        if input + cached + cache_create + output + reasoning == 0 {
            continue;
        }
        let (provider, model_id) = match model_key.find(':') {
            Some(idx) => (model_key[..idx].to_string(), model_key[idx + 1..].to_string()),
            None => (String::new(), model_key),
        };
        messages.push(build_raw_message(
            "mux",
            "default",
            timestamp,
            model_id,
            provider,
            normalize_tokens(input, output, cached, cache_create, reasoning),
            None,
            None,
        ));
    }
    messages
}

// ─── Cursor (CSV) ────────────────────────────────────────────────────────────

pub fn parse_cursor(range: DateRange) -> Vec<RawUsageMessage> {
    let root = resolve_home("~/.config/tokscale/cursor-cache");
    let files: Vec<_> = scan_directory(&root, "*.csv")
        .into_iter()
        .filter(|p| !file_predates_range(p, range))
        .collect();

    let messages: Vec<RawUsageMessage> = files
        .into_par_iter()
        .flat_map(|path| cached_file_messages(&path, || parse_cursor_file(&path)))
        .collect();

    filter_messages_by_date_range(messages, range)
}

fn parse_cursor_file(path: &Path) -> Vec<RawUsageMessage> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut header_line = String::new();
    if reader.read_line(&mut header_line).unwrap_or(0) == 0 {
        return Vec::new();
    }
    if !header_line.contains("Date") || !header_line.contains("Model") {
        return Vec::new();
    }

    let header_fields = parse_csv_line(header_line.trim_end());
    let has_kind = header_fields.iter().any(|f| f.trim() == "Kind");
    let col_count = header_fields.len();

    let (model_idx, in_cw_idx, in_no_cw_idx, cr_idx, out_idx, cost_idx) =
        if has_kind && col_count >= 11 {
            (4, 6, 7, 8, 9, 11)
        } else if has_kind {
            (2, 3, 4, 5, 6, 8)
        } else {
            (1, 2, 3, 4, 5, 7)
        };

    let mut messages = Vec::new();
    let mut line_buf = String::new();
    while reader.read_line(&mut line_buf).unwrap_or(0) > 0 {
        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            line_buf.clear();
            continue;
        }
        let fields = parse_csv_line(trimmed);
        if fields.len() < cost_idx + 1 {
            line_buf.clear();
            continue;
        }
        let clean = |idx: usize| fields[idx].trim().trim_matches('"').to_string();
        let date_str = clean(0);
        let model = clean(model_idx);
        if model.is_empty() {
            line_buf.clear();
            continue;
        }
        let in_with_cw = clean(in_cw_idx).parse::<i64>().unwrap_or(0);
        let in_no_cw = clean(in_no_cw_idx).parse::<i64>().unwrap_or(0);
        let cache_read = clean(cr_idx).parse::<i64>().unwrap_or(0);
        let output = clean(out_idx).parse::<i64>().unwrap_or(0);

        let ts = parse_cursor_date(&date_str);
        if ts <= 0.0 {
            line_buf.clear();
            continue;
        }
        let cache_write = (in_with_cw - in_no_cw).max(0);
        let provider = infer_provider_from_model(&model);
        messages.push(build_raw_message(
            "cursor",
            "default",
            ts,
            model,
            provider,
            normalize_tokens(in_no_cw, output, cache_read, cache_write, 0),
            None,
            None,
        ));
        line_buf.clear();
    }
    messages
}

fn parse_csv_line(line: &str) -> Vec<&str> {
    let mut fields = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    for (i, b) in line.bytes().enumerate() {
        if b == b'"' {
            in_quotes = !in_quotes;
        } else if b == b',' && !in_quotes {
            fields.push(&line[start..i]);
            start = i + 1;
        }
    }
    fields.push(&line[start..]);
    fields
}

fn parse_cursor_date(s: &str) -> f64 {
    if s.len() == 10 {
        if let Ok(nd) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            if let Some(ndt) = nd.and_hms_opt(12, 0, 0) {
                if let Some(ldt) = Local.from_local_datetime(&ndt).single() {
                    return ldt.timestamp_millis() as f64;
                }
                return ndt.and_utc().timestamp_millis() as f64;
            }
        }
    }
    parse_timestamp_str(s).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_support::{set_file_mtime_ms, with_temp_home};

    #[test]
    fn parses_amp_threads_and_merges_ledger_records() {
        with_temp_home(|home| {
            assert!(parse_amp(DateRange::default()).is_empty());
            let dir = home.join(".local/share/amp/threads");
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("other.json"),
                r#"{"id":"other","messages":[{"role":"assistant","usage":{"model":"gpt-4o","inputTokens":10,"outputTokens":5}}]}"#,
            )
            .unwrap();
            fs::write(
                dir.join("T-1.json"),
                r#"{"id":"T-1","created":1700000000000,"messages":[{"role":"assistant","messageId":1,"usage":{"model":"gpt-4o","inputTokens":100,"outputTokens":50,"cacheReadInputTokens":20,"cacheCreationInputTokens":5,"credits":0.01}}],"usageLedger":{"events":[{"timestamp":"2024-12-01T10:00:00Z","model":"gpt-4o","credits":0.015,"tokens":{"input":100,"output":50,"cacheReadInputTokens":20,"cacheCreationInputTokens":5},"toMessageId":1}]}}"#,
            )
            .unwrap();

            let res = parse_amp(DateRange::default());
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].source, "amp");
            assert_eq!(res[0].model_id, "gpt-4o");
            assert_eq!(res[0].input, 100.0);
            assert_eq!(res[0].output, 50.0);
            assert_eq!(res[0].cache_input, 20.0);
            assert_eq!(res[0].cache_output, 5.0);
        });
    }

    #[test]
    fn parses_codebuff_and_falls_back_to_chat_id_timestamp() {
        with_temp_home(|home| {
            assert!(parse_codebuff(DateRange::default()).is_empty());
            let dir = home.join(".config/manicode/projects/myproject/2024-01-01T10-00-00");
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("chat-messages.json"),
                r#"[{"variant":"user","timestamp":1700000000000,"metadata":{"model":"claude-sonnet-4","usage":{"inputTokens":100,"outputTokens":50}}},{"variant":"assistant","metadata":{"model":"gpt-4o","usage":{"inputTokens":10,"outputTokens":5,"cacheReadInputTokens":2,"cacheCreationInputTokens":1}}}]"#,
            )
            .unwrap();

            let res = parse_codebuff(DateRange::default());
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].source, "codebuff");
            assert_eq!(res[0].model_id, "gpt-4o");
            assert_eq!(res[0].input, 10.0);
            assert_eq!(res[0].output, 5.0);
            assert_eq!(res[0].cache_input, 2.0);
            assert_eq!(res[0].cache_output, 1.0);
            assert!(res[0].date_ms > 1_704_000_000_000.0);
        });
    }

    #[test]
    fn parses_droid_settings_and_normalizes_model_names() {
        with_temp_home(|home| {
            assert!(parse_droid(DateRange::default()).is_empty());
            let dir = home.join(".factory/sessions");
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("zero.settings.json"),
                r#"{"model":"gpt-4o","tokenUsage":{"inputTokens":0,"outputTokens":0}}"#,
            )
            .unwrap();
            fs::write(
                dir.join("uuid.settings.json"),
                r#"{"model":"custom:GPT-4.O[preview]--","providerLock":"openai","providerLockTimestamp":"2024-12-01T10:00:00Z","tokenUsage":{"inputTokens":100,"outputTokens":50,"cacheCreationTokens":5,"cacheReadTokens":20,"thinkingTokens":2}}"#,
            )
            .unwrap();

            let res = parse_droid(DateRange::default());
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].source, "droid");
            assert_eq!(res[0].model_id, "gpt-4-o");
            assert_eq!(res[0].provider, "openai");
            assert_eq!(res[0].input, 100.0);
            assert_eq!(res[0].output, 50.0);
            assert_eq!(res[0].cache_input, 20.0);
            assert_eq!(res[0].cache_output, 5.0);
            assert_eq!(res[0].reasoning, 2.0);
        });
    }

    #[test]
    fn parses_mux_sessions_and_filters_by_mtime() {
        with_temp_home(|home| {
            assert!(parse_mux(DateRange::default()).is_empty());
            let dir = home.join(".mux/sessions");
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("session-usage.json");
            fs::write(
                &path,
                r#"{"version":1,"byModel":{"openai:gpt-4o":{"input":{"tokens":100},"cached":{"tokens":20},"cacheCreate":{"tokens":5},"output":{"tokens":50},"reasoning":{"tokens":2}},"zero":{"input":{"tokens":0}}},"lastRequest":{"model":"gpt-4o","timestamp":1700000000000}}"#,
            )
            .unwrap();

            let res = parse_mux(DateRange::default());
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].source, "mux");
            assert_eq!(res[0].model_id, "gpt-4o");
            assert_eq!(res[0].provider, "openai");
            assert_eq!(res[0].input, 100.0);
            assert_eq!(res[0].output, 50.0);
            assert_eq!(res[0].cache_input, 20.0);
            assert_eq!(res[0].cache_output, 5.0);
            assert_eq!(res[0].reasoning, 2.0);

            set_file_mtime_ms(&path, 1_000_000);
            let old_res = parse_mux(DateRange {
                from_ms: Some(1_700_000_000_000.0),
                to_ms: None,
            });
            assert!(old_res.is_empty());
        });
    }

    #[test]
    fn parses_cursor_v1_v2_v3_csv_formats() {
        with_temp_home(|home| {
            assert!(parse_cursor(DateRange::default()).is_empty());
            let dir = home.join(".config/tokscale/cursor-cache");
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("v1.csv"),
                "Date,Model,Input (cache write),Input (no cache),Cache Read,Output,Total,Cost\n2024-12-01T10:00:00Z,gpt-4o,120,100,20,50,190,0.01\ninvalid,gpt-4o,120,100,20,50,190,0.01",
            )
            .unwrap();
            fs::write(
                dir.join("v2.csv"),
                "Date,Kind,Model,Input (cache write),Input (no cache),Cache Read,Output,Total,Cost\n2024-12-01T10:00:00Z,chat,gpt-4o,120,100,20,50,190,0.01",
            )
            .unwrap();
            fs::write(
                dir.join("v3.csv"),
                "Date,A,B,Kind,Model,C,Input (cache write),Input (no cache),Cache Read,Output,Total,Cost\n2024-12-01T10:00:00Z,x,y,chat,gpt-4o,z,120,100,20,50,190,0.01",
            )
            .unwrap();

            let res = parse_cursor(DateRange::default());
            assert_eq!(res.len(), 3);
            for m in res {
                assert_eq!(m.source, "cursor");
                assert_eq!(m.model_id, "gpt-4o");
                assert_eq!(m.input, 100.0);
                assert_eq!(m.output, 50.0);
                assert_eq!(m.cache_input, 20.0);
                assert_eq!(m.cache_output, 20.0);
            }
        });
    }
}
