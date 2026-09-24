//! Optional, bounded observations of work without a terminal pane.
use std::collections::HashSet;

use serde::{Deserialize, Serialize};

pub(crate) const MAX_ACTIVITY_SOURCES: usize = 8;
const INVALID_REPORT_BOUNDS: &str =
    "activity requires a source, 1..120000ms TTL, and at most 100 tasks";

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

impl ActivityReportParams {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if !(1..=120_000).contains(&self.ttl_ms) {
            return Err(INVALID_REPORT_BOUNDS);
        }
        validate_activity_source(&self.source, &self.tasks)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ActivitySource {
    pub source: String,
    pub tasks: Vec<ActivityTask>,
    pub omitted: u32,
}

pub(crate) fn validate_activity_source(
    source: &str,
    tasks: &[ActivityTask],
) -> Result<(), &'static str> {
    if source.is_empty() || source.len() > 64 || has_control_chars(source) || tasks.len() > 100 {
        return Err(INVALID_REPORT_BOUNDS);
    }
    let mut ids = HashSet::new();
    for task in tasks {
        if task.id.is_empty()
            || !ids.insert(&task.id)
            || [
                &task.id,
                &task.request_id,
                &task.harness,
                &task.workspace,
                &task.status,
                &task.progress,
            ]
            .into_iter()
            .chain(
                [task.model.as_ref(), task.effort.as_ref()]
                    .into_iter()
                    .flatten(),
            )
            .any(|text| text.len() > 256 || has_control_chars(text))
        {
            return Err(
                "activity task ids must be unique; text must be bounded and contain no controls",
            );
        }
    }
    Ok(())
}

fn has_control_chars(text: &str) -> bool {
    text.chars().any(char::is_control)
}
