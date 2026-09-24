//! Optional, bounded observations of work without a terminal pane.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ActivityTask {
    pub id: String,
    pub request_id: String,
    pub harness: String,
    pub workspace: String,
    pub status: String,
    pub progress: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub elapsed_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ActivityReportParams {
    pub source: String,
    pub seq: u64,
    pub ttl_ms: u64,
    pub tasks: Vec<ActivityTask>,
    #[serde(default)]
    pub omitted: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ActivitySource {
    pub source: String,
    pub tasks: Vec<ActivityTask>,
    pub omitted: u32,
}
