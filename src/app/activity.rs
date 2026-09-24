use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::api::schema::{activity::MAX_ACTIVITY_SOURCES, ActivityReportParams, ActivitySource};

#[derive(Default)]
pub(crate) struct ActivityState(BTreeMap<String, (u64, Instant, ActivitySource)>);

impl ActivityState {
    pub(crate) fn report(
        &mut self,
        report: ActivityReportParams,
        now: Instant,
    ) -> Result<bool, &'static str> {
        report.validate()?;
        self.expire(now);
        if let Some((seq, _, _)) = self.0.get(&report.source) {
            if report.seq <= *seq {
                return Ok(false);
            }
        } else if self.0.len() >= MAX_ACTIVITY_SOURCES {
            return Err("at most 8 activity sources are allowed");
        }
        self.0.insert(
            report.source.clone(),
            (
                report.seq,
                now + Duration::from_millis(report.ttl_ms),
                ActivitySource {
                    source: report.source,
                    tasks: report.tasks,
                    omitted: report.omitted,
                },
            ),
        );
        Ok(true)
    }

    pub(crate) fn snapshot(&self) -> Vec<ActivitySource> {
        self.0
            .values()
            .filter(|(_, _, source)| !source.tasks.is_empty() || source.omitted > 0)
            .map(|(_, _, source)| source.clone())
            .collect()
    }

    pub(crate) fn next_expiry(&self) -> Option<Instant> {
        self.0.values().map(|(_, deadline, _)| *deadline).min()
    }

    pub(crate) fn expire(&mut self, now: Instant) {
        self.0.retain(|_, (_, deadline, _)| *deadline > now);
    }
}

impl super::App {
    pub(super) fn handle_activity_report(
        &mut self,
        id: String,
        params: ActivityReportParams,
    ) -> String {
        match self.state.activity.report(params, Instant::now()) {
            Ok(changed) => {
                self.sync_agent_metadata_deadline();
                super::api::responses::encode_success(
                    id,
                    crate::api::schema::ResponseResult::ActivityReported { changed },
                )
            }
            Err(message) => super::api::responses::encode_error(id, "invalid_params", message),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn report() -> ActivityReportParams {
        serde_json::from_value(serde_json::json!({
            "source":"session-hygiene", "seq":1, "ttl_ms":20000, "omitted":0,
            "tasks":[{"id":"task-a", "request_id":"build-a", "harness":"codex",
                "workspace":"canvas", "model":"sol", "effort":"high", "elapsed_seconds":42,
                "status":"running", "progress":"monitoring"}]
        }))
        .unwrap()
    }

    #[test]
    fn activity_replacement_order_expiry_and_bounds() {
        let mut state = ActivityState::default();
        let now = Instant::now();
        assert!(state.report(report(), now).unwrap());
        let mut older = report();
        older.tasks.clear();
        assert!(!state.report(older.clone(), now).unwrap());
        assert_eq!(state.snapshot()[0].tasks.len(), 1);
        older.seq = 2;
        assert!(state.report(older, now).unwrap());
        assert!(state.snapshot().is_empty());
        assert!(!state.report(report(), now).unwrap());
        let mut next = report();
        next.seq = 3;
        state.report(next, now).unwrap();
        state.expire(now + Duration::from_secs(20));
        assert!(state.snapshot().is_empty());
        assert!(state.next_expiry().is_none());
        let mut omitted = report();
        omitted.tasks.clear();
        omitted.omitted = 3;
        assert!(state.report(omitted, now + Duration::from_secs(20)).unwrap());
        assert_eq!(state.snapshot()[0].omitted, 3);
        state.expire(now + Duration::from_secs(40));
        let mut invalid = report();
        invalid.tasks[0].request_id = "escape\x1b[2J".into();
        assert!(state.report(invalid, now).is_err());
        let mut invalid = report();
        invalid.tasks.push(invalid.tasks[0].clone());
        assert!(state.report(invalid, now).is_err());
        let mut invalid = report();
        invalid.ttl_ms = 120001;
        assert!(state.report(invalid, now).is_err());
        let mut invalid = report();
        invalid.tasks = vec![invalid.tasks[0].clone(); 101];
        assert!(state.report(invalid, now).is_err());
        for i in 0..8 {
            let mut r = report();
            r.source = format!("s{i}");
            state.report(r, now).unwrap();
        }
        assert!(state.report(report(), now).is_err());
    }

    #[test]
    fn activity_api_creates_no_native_resources_and_expires_on_metadata_deadline() {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let before = app.session_snapshot();
        let raw = app.handle_api_request(crate::api::schema::Request {
            id: "report".into(),
            method: crate::api::schema::Method::ActivityReport(report()),
        });
        assert!(raw.contains("\"changed\":true"));
        let current = app.session_snapshot();
        assert_eq!(current.panes, before.panes);
        assert_eq!(current.agents, before.agents);
        assert_eq!(current.workspaces, before.workspaces);
        assert_eq!(current.activity[0].tasks[0].request_id, "build-a");
        let deadline = app.state.next_agent_metadata_expiry().unwrap();
        assert!(app.expire_due_metadata(deadline));
        assert!(app.session_snapshot().activity.is_empty());
    }
}
