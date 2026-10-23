use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone};
use napi_derive::napi;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

#[napi(object)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RawUsageMessage {
    pub source: String,
    pub agent: String,
    pub r#type: String,
    pub date_ms: f64,
    pub model_id: String,
    pub provider: String,
    pub input: f64,
    pub output: f64,
    pub reasoning: f64,
    pub cache_input: f64,
    pub cache_output: f64,
    pub project_name: Option<String>,
    pub project_path: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DateRange {
    pub from_ms: Option<f64>,
    pub to_ms: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Tokens {
    pub input: i64,
    pub output: i64,
    pub cache_input: i64,
    pub cache_output: i64,
    pub reasoning: i64,
}

pub fn normalize_tokens(
    input: i64,
    output: i64,
    cache_input: i64,
    cache_output: i64,
    reasoning: i64,
) -> Tokens {
    Tokens {
        input: input.max(0),
        output: output.max(0),
        cache_input: cache_input.max(0),
        cache_output: cache_output.max(0),
        reasoning: reasoning.max(0),
    }
}

pub fn build_raw_message(
    source: &str,
    agent: &str,
    date_ms: f64,
    model_id: String,
    provider: String,
    tokens: Tokens,
    project_name: Option<String>,
    project_path: Option<String>,
) -> RawUsageMessage {
    RawUsageMessage {
        source: source.to_string(),
        agent: agent.to_string(),
        r#type: "assistant".to_string(),
        date_ms,
        model_id,
        provider,
        input: tokens.input as f64,
        output: tokens.output as f64,
        reasoning: tokens.reasoning as f64,
        cache_input: tokens.cache_input as f64,
        cache_output: tokens.cache_output as f64,
        project_name,
        project_path,
    }
}

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn resolve_home(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }
    if let Some(stripped) = path.strip_prefix("~/") {
        return home_dir().join(stripped);
    }
    PathBuf::from(path)
}

pub fn file_modified_timestamp_ms(path: &Path) -> f64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64() * 1000.0)
                .unwrap_or(0.0)
        })
}

pub fn file_predates_range(path: &Path, range: DateRange) -> bool {
    if let Some(from_ms) = range.from_ms {
        return file_modified_timestamp_ms(path) < from_ms;
    }
    false
}

pub fn filter_messages_by_date_range(
    messages: Vec<RawUsageMessage>,
    range: DateRange,
) -> Vec<RawUsageMessage> {
    if range.from_ms.is_none() && range.to_ms.is_none() {
        return messages;
    }
    messages
        .into_iter()
        .filter(|m| {
            if let Some(from_ms) = range.from_ms {
                if m.date_ms < from_ms {
                    return false;
                }
            }
            if let Some(to_ms) = range.to_ms {
                if m.date_ms > to_ms {
                    return false;
                }
            }
            true
        })
        .collect()
}

pub fn scan_directory(root: &Path, pattern: &str) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    let mut result: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            let name = e.file_name().to_string_lossy();
            match pattern {
                "*.json" => name.ends_with(".json"),
                "*.jsonl" => name.ends_with(".jsonl"),
                "*.json|*.jsonl" => name.ends_with(".json") || name.ends_with(".jsonl"),
                "*.csv" => name.ends_with(".csv"),
                _ => false,
            }
        })
        .map(|e| e.into_path())
        .collect();
    result.sort();
    result
}

pub fn parse_timestamp_str(s: &str) -> Option<f64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains('T') || trimmed.contains('-') {
        if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
            return Some(dt.timestamp_millis() as f64);
        }
        for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"] {
            if let Ok(ndt) = NaiveDateTime::parse_from_str(trimmed, fmt) {
                if let Some(ldt) = Local.from_local_datetime(&ndt).single() {
                    return Some(ldt.timestamp_millis() as f64);
                }
                return Some(ndt.and_utc().timestamp_millis() as f64);
            }
        }
        if let Ok(nd) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
            if let Some(ndt) = nd.and_hms_opt(0, 0, 0) {
                return Some(ndt.and_utc().timestamp_millis() as f64);
            }
        }
    }
    if let Ok(n) = trimmed.parse::<f64>() {
        if n > 0.0 {
            return Some(if n >= 1_000_000_000_000.0 { n } else { n * 1000.0 });
        }
    }
    None
}

pub fn parse_timestamp_value(val: &Value) -> Option<f64> {
    match val {
        Value::String(s) => parse_timestamp_str(s),
        Value::Number(n) => {
            let v = n.as_f64()?;
            if v <= 0.0 {
                return None;
            }
            Some(if v >= 1_000_000_000_000.0 { v } else { v * 1000.0 })
        }
        Value::Array(arr) if arr.len() >= 2 => {
            let secs = parse_timestamp_value(&arr[0])?;
            let nanos = parse_timestamp_value(&arr[1])?;
            Some(secs + nanos / 1_000_000.0)
        }
        _ => None,
    }
}

pub fn extract_i64(val: &Value) -> Option<i64> {
    match val {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.floor() as i64)),
        Value::String(s) => s.trim().parse::<f64>().ok().map(|f| f.floor() as i64),
        _ => None,
    }
}

pub fn extract_f64(val: &Value) -> Option<f64> {
    match val {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

pub fn extract_string(val: &Value) -> Option<String> {
    val.as_str().map(|s| s.to_string())
}

pub fn infer_provider_from_model(model: &str) -> String {
    let lower = model.to_lowercase();
    if lower.contains("claude")
        || lower.contains("anthropic")
        || contains_delimited(&lower, "opus")
        || contains_delimited(&lower, "sonnet")
        || contains_delimited(&lower, "haiku")
    {
        return "anthropic".to_string();
    }
    if lower.contains("gpt")
        || lower.contains("openai")
        || contains_delimited(&lower, "o1")
        || contains_delimited(&lower, "o3")
        || contains_delimited(&lower, "o4")
    {
        return "openai".to_string();
    }
    if lower.contains("gemini") || lower.contains("google") {
        return "google".to_string();
    }
    if lower.contains("grok") {
        return "xai".to_string();
    }
    if lower.contains("deepseek") {
        return "deepseek".to_string();
    }
    if lower.contains("mistral") || lower.contains("mixtral") {
        return "mistral".to_string();
    }
    if lower.contains("llama") || contains_delimited(&lower, "meta") {
        return "meta".to_string();
    }
    if lower.contains("qwen") {
        return "qwen".to_string();
    }
    "unknown".to_string()
}

fn contains_delimited(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let Some(idx) = haystack.find(needle) else {
        return false;
    };
    let before_ok = idx == 0 || !bytes[idx - 1].is_ascii_alphanumeric();
    let after_pos = idx + needle.len();
    let after_ok = after_pos >= bytes.len() || !bytes[after_pos].is_ascii_alphanumeric();
    before_ok && after_ok
}

pub fn canonical_provider(raw: &str) -> Option<String> {
    provider_tags(raw).into_iter().next()
}

pub fn provider_tags(raw: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let mut push_segment = |segment: &str| {
        if let Some(tag) = canonicalize_provider_segment(segment) {
            if !tags.contains(&tag) {
                tags.push(tag);
            }
        }
    };
    for segment in raw.trim().trim_end_matches('/').split('/') {
        push_segment(segment);
        if segment.contains('.') {
            for dotted in segment.split('.') {
                push_segment(dotted);
            }
        }
    }
    tags
}

fn canonicalize_provider_segment(segment: &str) -> Option<String> {
    let normalized = segment
        .trim()
        .trim_end_matches('/')
        .to_lowercase()
        .replace('-', "_");
    match normalized.as_str() {
        "" | "unknown" => None,
        "x_ai" | "xai" => Some("xai".to_string()),
        "z_ai" | "zai" => Some("zai".to_string()),
        "moonshot" | "moonshotai" => Some("moonshotai".to_string()),
        "meta" | "meta_llama" => Some("meta_llama".to_string()),
        "azure" | "azure_ai" => Some("azure_ai".to_string()),
        "anthropic" | "vertex" | "vertex_ai" => Some("anthropic".to_string()),
        "together" | "together_ai" => Some("together_ai".to_string()),
        "fireworks" | "fireworks_ai" => Some("fireworks_ai".to_string()),
        "google" | "gemini" => Some("google".to_string()),
        "openai" | "openai_codex" => Some("openai".to_string()),
        "mistral" | "mistralai" => Some("mistralai".to_string()),
        "ai21" => Some("ai21".to_string()),
        other => {
            if other.chars().any(|c| c.is_ascii_digit()) {
                None
            } else {
                Some(other.to_string())
            }
        }
    }
}

pub fn normalize_agent_name(agent: &str) -> String {
    let cleaned: String = agent
        .chars()
        .filter(|c| !matches!(*c, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}'))
        .collect();
    let trimmed = cleaned.trim();
    let stripped = strip_agent_prefix(trimmed);
    let canonical = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = canonical.to_lowercase();

    if lower.contains("plan") {
        if lower.contains("omo") || lower.contains("sisyphus") {
            return "Planner-Sisyphus".to_string();
        }
        return titlecase_agent(&canonical);
    }
    match lower.as_str() {
        "omo" | "sisyphus" => "Sisyphus".to_string(),
        "orchestrator-sisyphus" => "Atlas".to_string(),
        "sisyphus-junior" => "Sisyphus-Junior".to_string(),
        "planner-sisyphus" => "Planner-Sisyphus".to_string(),
        _ => titlecase_agent(&canonical),
    }
}

fn strip_agent_prefix(name: &str) -> &str {
    let lower = name.to_lowercase();
    for prefix in ["astrape:", "oh-my-claudecode:", "oh-my-codex:"] {
        if lower.starts_with(prefix) {
            return &name[prefix.len()..];
        }
    }
    name
}

fn titlecase_agent(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    name.split('-')
        .flat_map(|p| p.split_whitespace())
        .map(|w| match w.to_lowercase().as_str() {
            "ui" => "UI".to_string(),
            "ux" => "UX".to_string(),
            "api" => "API".to_string(),
            other => {
                let mut chars = other.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn derive_agent(agent: Option<&str>) -> String {
    let Some(a) = agent else {
        return "default".to_string();
    };
    let lower = a.to_lowercase();
    if lower.contains("plan") {
        return "plan".to_string();
    }
    if lower.contains("build") || lower == "headless" {
        return "build".to_string();
    }
    if lower.contains("agent") {
        return "agent".to_string();
    }
    if lower.contains("debug") {
        return "debug".to_string();
    }
    if lower.contains("ask") {
        return "ask".to_string();
    }
    "default".to_string()
}

pub fn normalize_workspace_key(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let preserve_unc = trimmed.starts_with("\\\\") || trimmed.starts_with("//");
    let mut normalized = trimmed.replace('\\', "/");
    if preserve_unc {
        let mut body = normalized.trim_start_matches('/').to_string();
        while body.contains("//") {
            body = body.replace("//", "/");
        }
        normalized = format!("//{body}");
    } else {
        while normalized.contains("//") {
            normalized = normalized.replace("//", "/");
        }
    }
    let min_len = if preserve_unc { 2 } else { 1 };
    if normalized.len() > min_len && normalized.ends_with('/') {
        normalized.pop();
    }
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

pub fn workspace_label_from_key(key: &str) -> Option<String> {
    key.split('/')
        .filter(|s| !s.is_empty())
        .last()
        .map(|s| s.to_string())
}

#[cfg(test)]
pub mod test_support {
    use crate::cache::set_cache_disabled;
    use std::env;
    use std::fs::{self, File};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, UNIX_EPOCH};

    static ENV_LOCK: Mutex<()> = Mutex::new(());
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    pub fn make_temp_dir(prefix: &str) -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = env::temp_dir().join(format!("mytokens-{prefix}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn set_file_mtime_ms(path: &Path, ms: u64) {
        let file = File::options().write(true).open(path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_millis(ms))
            .unwrap();
    }

    pub fn with_temp_home<F: FnOnce(&Path)>(f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp_home = make_temp_dir("home");
        let old_home = env::var_os("HOME");
        let old_profile = env::var_os("USERPROFILE");
        unsafe {
            env::set_var("HOME", &temp_home);
            env::set_var("USERPROFILE", &temp_home);
        }
        set_cache_disabled(false);

        f(&temp_home);

        unsafe {
            match old_home {
                Some(v) => env::set_var("HOME", v),
                None => env::remove_var("HOME"),
            }
            match old_profile {
                Some(v) => env::set_var("USERPROFILE", v),
                None => env::remove_var("USERPROFILE"),
            }
        }
        let _ = fs::remove_dir_all(&temp_home);
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{make_temp_dir, set_file_mtime_ms};
    use super::*;

    fn message_at(ts: f64) -> RawUsageMessage {
        build_raw_message(
            "test",
            "default",
            ts,
            "gpt-4o".to_string(),
            "openai".to_string(),
            normalize_tokens(1, 1, 0, 0, 0),
            None,
            None,
        )
    }

    const DAY: f64 = 24.0 * 60.0 * 60.0 * 1000.0;
    const JAN1: f64 = 1_704_067_200_000.0; // 2024-01-01T00:00:00Z

    fn sample_messages() -> Vec<RawUsageMessage> {
        vec![
            message_at(JAN1),
            message_at(JAN1 + DAY),
            message_at(JAN1 + 2.0 * DAY),
        ]
    }

    #[test]
    fn filter_messages_by_date_range_returns_unchanged_when_bounds_null() {
        let msgs = sample_messages();
        let res = filter_messages_by_date_range(msgs.clone(), DateRange::default());
        assert_eq!(res, msgs);
    }

    #[test]
    fn filter_messages_by_date_range_keeps_on_or_after_from() {
        let res = filter_messages_by_date_range(
            sample_messages(),
            DateRange {
                from_ms: Some(JAN1 + DAY),
                to_ms: None,
            },
        );
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].date_ms, JAN1 + DAY);
    }

    #[test]
    fn filter_messages_by_date_range_keeps_on_or_before_to() {
        let res = filter_messages_by_date_range(
            sample_messages(),
            DateRange {
                from_ms: None,
                to_ms: Some(JAN1 + DAY),
            },
        );
        assert_eq!(res.len(), 2);
        assert_eq!(res[1].date_ms, JAN1 + DAY);
    }

    #[test]
    fn filter_messages_by_date_range_applies_both_bounds_inclusively() {
        let res = filter_messages_by_date_range(
            sample_messages(),
            DateRange {
                from_ms: Some(JAN1),
                to_ms: Some(JAN1 + DAY),
            },
        );
        assert_eq!(res.len(), 2);
    }

    #[test]
    fn filter_messages_by_date_range_returns_empty_when_out_of_range() {
        let res = filter_messages_by_date_range(
            sample_messages(),
            DateRange {
                from_ms: Some(JAN1 + 10.0 * DAY),
                to_ms: Some(JAN1 + 20.0 * DAY),
            },
        );
        assert!(res.is_empty());
    }

    #[test]
    fn file_predates_range_checks_mtime_against_from_bound() {
        let dir = make_temp_dir("range");
        let path = dir.join("log.jsonl");
        fs::write(&path, "{}").unwrap();

        set_file_mtime_ms(&path, 0);
        assert!(!file_predates_range(&path, DateRange::default()));

        let jan_2024 = parse_timestamp_str("2024-01-01T00:00:00Z").unwrap() as u64;
        let jun_1_2024 = parse_timestamp_str("2024-06-01T00:00:00Z").unwrap();
        let jun_15_2024 = parse_timestamp_str("2024-06-15T00:00:00Z").unwrap() as u64;

        set_file_mtime_ms(&path, jan_2024);
        assert!(file_predates_range(
            &path,
            DateRange {
                from_ms: Some(jun_1_2024),
                to_ms: None,
            }
        ));

        set_file_mtime_ms(&path, jun_15_2024);
        assert!(!file_predates_range(
            &path,
            DateRange {
                from_ms: Some(jun_1_2024),
                to_ms: None,
            }
        ));
        let _ = fs::remove_dir_all(&dir);
    }
}
