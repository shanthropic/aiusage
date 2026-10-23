use crate::cache::cached_file_messages;
use crate::common::{
    build_raw_message, derive_agent, extract_i64, file_predates_range,
    filter_messages_by_date_range, infer_provider_from_model, normalize_tokens,
    parse_timestamp_value, resolve_home, scan_directory, DateRange, RawUsageMessage,
};
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Deserialize)]
struct UiMessageEntry {
    r#type: Option<String>,
    say: Option<String>,
    text: Option<String>,
    ts: Option<Value>,
}

pub fn parse_roocode(range: DateRange) -> Vec<RawUsageMessage> {
    let roots = [
        resolve_home("~/.config/Code/User/globalStorage/rooveterinaryinc.roo-cline/tasks"),
        resolve_home("~/.vscode-server/data/User/globalStorage/rooveterinaryinc.roo-cline/tasks"),
    ];
    parse_roo_kilo_roots(&roots, "roocode", range)
}

pub fn parse_kilocode(range: DateRange) -> Vec<RawUsageMessage> {
    let roots = [
        resolve_home("~/.config/Code/User/globalStorage/kilocode.kilo-code/tasks"),
        resolve_home("~/.vscode-server/data/User/globalStorage/kilocode.kilo-code/tasks"),
    ];
    parse_roo_kilo_roots(&roots, "kilocode", range)
}

fn parse_roo_kilo_roots(
    roots: &[std::path::PathBuf],
    source_name: &'static str,
    range: DateRange,
) -> Vec<RawUsageMessage> {
    let files: Vec<_> = roots
        .iter()
        .flat_map(|r| scan_directory(r, "*.json"))
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()) == Some("ui_messages.json")
                && !file_predates_range(p, range)
        })
        .collect();

    let messages: Vec<RawUsageMessage> = files
        .into_par_iter()
        .flat_map(|path| cached_file_messages(&path, || parse_roo_kilo_file(&path, source_name)))
        .collect();

    filter_messages_by_date_range(messages, range)
}

fn parse_roo_kilo_file(path: &Path, source_name: &str) -> Vec<RawUsageMessage> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(entries) = serde_json::from_str::<Vec<UiMessageEntry>>(&text) else {
        return Vec::new();
    };

    let (model_id, agent) = read_task_metadata_tail(path);
    let derived_agent = derive_agent(agent.as_deref());
    let mut messages = Vec::new();

    for entry in entries {
        if entry.r#type.as_deref() != Some("say") || entry.say.as_deref() != Some("api_req_started")
        {
            continue;
        }
        let Some(text_payload) = entry.text else {
            continue;
        };
        let Some(ts_val) = entry.ts else {
            continue;
        };
        let Some(timestamp) = parse_entry_timestamp(&ts_val) else {
            continue;
        };
        let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(&text_payload) else {
            continue;
        };

        let tokens_in = payload.get("tokensIn").and_then(extract_i64).unwrap_or(0);
        let tokens_out = payload.get("tokensOut").and_then(extract_i64).unwrap_or(0);
        let cache_reads = payload.get("cacheReads").and_then(extract_i64).unwrap_or(0);
        let cache_writes = payload.get("cacheWrites").and_then(extract_i64).unwrap_or(0);
        let provider = payload
            .get("apiProtocol")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| infer_provider_from_model(&model_id));

        messages.push(build_raw_message(
            source_name,
            &derived_agent,
            timestamp,
            model_id.clone(),
            provider,
            normalize_tokens(tokens_in, tokens_out, cache_reads, cache_writes, 0),
            None,
            None,
        ));
    }

    messages
}

fn read_task_metadata_tail(ui_path: &Path) -> (String, Option<String>) {
    let Some(dir) = ui_path.parent() else {
        return ("unknown".to_string(), None);
    };
    let history_path = dir.join("api_conversation_history.json");
    let Ok(mut file) = File::open(&history_path) else {
        return ("unknown".to_string(), None);
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len == 0 {
        return ("unknown".to_string(), None);
    }

    let tail_size: u64 = 128 * 1024;
    if len > tail_size {
        if file.seek(SeekFrom::End(-(tail_size as i64))).is_ok() {
            let mut buf = Vec::with_capacity(tail_size as usize);
            if file.read_to_end(&mut buf).is_ok() {
                let tail_str = String::from_utf8_lossy(&buf);
                let (model, agent) = extract_model_and_agent(&tail_str);
                if model != "unknown" || agent.is_some() {
                    return (model, agent);
                }
            }
        }
    }

    let _ = file.seek(SeekFrom::Start(0));
    let mut full = String::new();
    if file.read_to_string(&mut full).is_ok() {
        return extract_model_and_agent(&full);
    }
    ("unknown".to_string(), None)
}

fn extract_model_and_agent(content: &str) -> (String, Option<String>) {
    const ENV_START: &str = "<environment_details>";
    const ENV_END: &str = "</environment_details>";

    let mut offset = 0;
    let mut last_model: Option<String> = None;
    let mut last_slug: Option<String> = None;
    let mut last_name: Option<String> = None;

    while let Some(start_rel) = content[offset..].find(ENV_START) {
        let start_idx = offset + start_rel + ENV_START.len();
        let Some(end_rel) = content[start_idx..].find(ENV_END) else {
            break;
        };
        let end_idx = start_idx + end_rel;
        let block = &content[start_idx..end_idx];

        if let Some(m) = extract_tag_value(block, "model") {
            last_model = Some(m);
        }
        if let Some(s) = extract_tag_value(block, "slug") {
            last_slug = Some(s);
        }
        if let Some(n) = extract_tag_value(block, "name") {
            last_name = Some(n);
        }
        offset = end_idx + ENV_END.len();
    }

    (
        last_model.unwrap_or_else(|| "unknown".to_string()),
        last_slug.or(last_name),
    )
}

fn extract_tag_value(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start_idx = block.find(&open)? + open.len();
    let end_rel = block[start_idx..].find(&close)?;
    let val = block[start_idx..start_idx + end_rel].trim();
    if val.is_empty() {
        None
    } else {
        Some(val.to_string())
    }
}

fn parse_entry_timestamp(ts: &Value) -> Option<f64> {
    match ts {
        Value::Number(n) => n.as_f64().filter(|&v| v > 0.0),
        Value::String(s) => {
            if let Ok(n) = s.trim().parse::<f64>() {
                if n > 0.0 {
                    return Some(n);
                }
            }
            parse_timestamp_value(ts)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_support::with_temp_home;

    #[test]
    fn parses_roocode_and_kilocode_entries() {
        with_temp_home(|home| {
            assert!(parse_roocode(DateRange::default()).is_empty());
            assert!(parse_kilocode(DateRange::default()).is_empty());

            let roo_dir = home
                .join(".config/Code/User/globalStorage/rooveterinaryinc.roo-cline/tasks");
            fs::create_dir_all(&roo_dir).unwrap();
            fs::write(
                roo_dir.join("ui_messages.json"),
                r#"[{"type":"say","say":"api_req_started","ts":1700000000000,"text":"{\"tokensIn\":100,\"tokensOut\":50,\"cacheReads\":20,\"cacheWrites\":5,\"cost\":0.01}"},{"type":"say","say":"text","ts":1700000000000,"text":"hello"}]"#,
            )
            .unwrap();
            fs::write(
                roo_dir.join("api_conversation_history.json"),
                "Some conversation text\n<environment_details>\n<model>claude-sonnet-4</model>\n<slug>builder</slug>\n</environment_details>",
            )
            .unwrap();

            let res = parse_roocode(DateRange::default());
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].source, "roocode");
            assert_eq!(res[0].model_id, "claude-sonnet-4");
            assert_eq!(res[0].provider, "anthropic");
            assert_eq!(res[0].input, 100.0);
            assert_eq!(res[0].output, 50.0);
            assert_eq!(res[0].cache_input, 20.0);
            assert_eq!(res[0].cache_output, 5.0);
            assert_eq!(res[0].agent, "build");

            let kilo_dir = home
                .join(".config/Code/User/globalStorage/kilocode.kilo-code/tasks");
            fs::create_dir_all(&kilo_dir).unwrap();
            fs::write(
                kilo_dir.join("ui_messages.json"),
                r#"[{"type":"say","say":"api_req_started","ts":1700000000000,"text":"{\"tokensIn\":200,\"tokensOut\":100,\"cacheReads\":10,\"cacheWrites\":5,\"cost\":0.02,\"apiProtocol\":\"openrouter\"}"}]"#,
            )
            .unwrap();

            let kres = parse_kilocode(DateRange::default());
            assert_eq!(kres.len(), 1);
            assert_eq!(kres[0].source, "kilocode");
            assert_eq!(kres[0].provider, "openrouter");
            assert_eq!(kres[0].input, 200.0);
            assert_eq!(kres[0].output, 100.0);
        });
    }
}
