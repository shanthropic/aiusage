pub mod claude;
pub mod codex;
pub mod json;
pub mod jsonl;
pub mod opencode;
pub mod roocode;
pub mod sqlite;

use crate::common::{DateRange, RawUsageMessage};
use rayon::prelude::*;

pub fn parse_all_sources(range: DateRange) -> Vec<RawUsageMessage> {
    let tasks: Vec<fn(DateRange) -> Vec<RawUsageMessage>> = vec![
        sqlite::parse_antigravity,
        json::parse_amp,
        claude::parse_claude,
        json::parse_codebuff,
        codex::parse_codex,
        sqlite::parse_copilot,
        sqlite::parse_crush,
        json::parse_cursor,
        json::parse_droid,
        sqlite::parse_goose,
        sqlite::parse_hermes,
        sqlite::parse_kilo,
        roocode::parse_kilocode,
        jsonl::parse_kimi,
        json::parse_mux,
        jsonl::parse_openclaw,
        opencode::parse_opencode,
        jsonl::parse_pi,
        jsonl::parse_qwen,
        roocode::parse_roocode,
        sqlite::parse_synthetic,
    ];

    tasks
        .into_par_iter()
        .flat_map(|parser| parser(range))
        .collect()
}
