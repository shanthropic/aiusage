pub mod cache;
pub mod common;
pub mod parsers;

use cache::{
    FileMessagesCacheEntry, FileMessagesCacheInfo, cache_info, clear_cache, list_cache,
    prune_cache_if_needed, set_cache_disabled,
};
use common::{DateRange, RawUsageMessage};
use napi::bindgen_prelude::*;
use napi_derive::napi;

pub struct LoadUsageTask {
    pub range: DateRange,
    pub fresh: bool,
}

#[napi]
impl Task for LoadUsageTask {
    type Output = Vec<RawUsageMessage>;
    type JsValue = Vec<RawUsageMessage>;

    fn compute(&mut self) -> Result<Self::Output> {
        if self.fresh {
            set_cache_disabled(true);
        }
        let messages = parsers::parse_all_sources(self.range);
        prune_cache_if_needed();
        Ok(messages)
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

#[napi]
pub fn load_usage_data_native(
    from_ms: Option<f64>,
    to_ms: Option<f64>,
    fresh: Option<bool>,
) -> AsyncTask<LoadUsageTask> {
    AsyncTask::new(LoadUsageTask {
        range: DateRange { from_ms, to_ms },
        fresh: fresh.unwrap_or(false),
    })
}

#[napi]
pub fn disable_file_messages_cache() {
    set_cache_disabled(true);
}

#[napi]
pub fn clear_file_messages_cache() {
    clear_cache();
}

#[napi]
pub fn list_file_messages_cache() -> Vec<FileMessagesCacheEntry> {
    list_cache()
}

#[napi]
pub fn get_file_messages_cache_info() -> FileMessagesCacheInfo {
    cache_info()
}
