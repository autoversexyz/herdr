//! Native dialog inspection and guarded input belong to the runtime, not clients.
use bytes::Bytes;
use sha2::{Digest, Sha256};

use super::responses::{encode_error, encode_error_body, encode_success};
use crate::api::schema::{AgentPromptAnswerParams, AgentStatus, ResponseResult};
use crate::app::App;

/// Only explicit single digit menu keys are supported. No yes/no inference,
/// permission classification, fuzzy matching, default choice or trailing Enter.
fn options(screen: &str) -> Option<Vec<serde_json::Value>> {
    if screen.len() > 4096 || screen.lines().count() > 32 {
        return None;
    }
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in screen.lines() {
        let line = line
            .trim_start()
            .trim_start_matches(['❯', '>'])
            .trim_start();
        let line = line.strip_prefix('[').unwrap_or(line);
        let bytes = line.as_bytes();
        if bytes.len() < 4
            || !(b'1'..=b'9').contains(&bytes[0])
            || !matches!(bytes[1], b'.' | b')' | b']')
            || bytes[2] != b' '
        {
            continue;
        }
        let key = &line[..1];
        let label = line[3..].trim();
        if label.is_empty() || !seen.insert(key) {
            return None;
        }
        result.push(serde_json::json!({"key": key, "label": label}));
    }
    (result.len() >= 2).then_some(result)
}

impl App {
    pub(super) fn handle_agent_prompt_answer(
        &mut self,
        id: String,
        params: AgentPromptAnswerParams,
    ) -> String {
        if params.option.is_some() != params.expected_prompt.is_some() {
            return encode_error(
                id,
                "invalid_params",
                "option and expected_prompt must be supplied together",
            );
        }
        self.reconcile_managed_agent_target(&params.target);
        let agent = match self.agent_info_for_target(&params.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        if agent.agent_status != AgentStatus::Blocked
            || agent.launch_pending
            || agent.agent_session.is_none()
        {
            return encode_error(
                id,
                "native_prompt_unavailable",
                "a blocked native conversation is required",
            );
        }
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let terminal = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .and_then(|terminal_id| self.state.terminals.get(terminal_id));
        let Some(terminal) = terminal else {
            return encode_error(
                id,
                "native_prompt_unavailable",
                "native terminal is missing",
            );
        };
        let Some(expected_agent) = terminal.effective_known_agent() else {
            return encode_error(id, "native_prompt_unavailable", "native agent is unknown");
        };
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return encode_error(id, "native_prompt_unavailable", "native runtime is missing");
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return encode_error(id, "native_prompt_unavailable", "foreground agent changed");
        }
        let seq = runtime.content_seq();
        let screen = runtime.detection_text();
        if !seq.is_multiple_of(2) || runtime.content_seq() != seq {
            return encode_error(
                id,
                "native_prompt_changed",
                "terminal changed during capture",
            );
        }
        let Some(choices) = options(&screen) else {
            return encode_error(
                id,
                "native_prompt_unsupported",
                "a complete bounded numbered menu is required",
            );
        };
        let binding = serde_json::json!({"terminal_id": agent.terminal_id, "pane_id": agent.pane_id,
            "agent": agent.agent, "native_session": agent.agent_session, "state_change_seq": agent.state_change_seq});
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(serde_json::json!([binding, screen]).to_string().as_bytes())
        );
        let mut input_queued = false;
        if let Some(option) = &params.option {
            if params.expected_prompt.as_deref() != Some(&fingerprint) {
                return encode_error(
                    id,
                    "native_prompt_changed",
                    "prompt or native binding changed; inspect again",
                );
            }
            if !choices
                .iter()
                .any(|item| item["key"].as_str() == Some(option))
            {
                return encode_error(
                    id,
                    "invalid_option",
                    "option is not an explicit key in this prompt",
                );
            }
            let encoded = match super::super::api_helpers::encode_api_keys(
                runtime,
                std::slice::from_ref(option),
            ) {
                Ok(value) => value.into_iter().flatten().collect::<Vec<u8>>(),
                Err(_) => {
                    return encode_error(id, "invalid_option", "option key cannot be encoded")
                }
            };
            if let Err(err) = runtime.send_if_content_unchanged(seq, Bytes::from(encoded)) {
                return encode_error(id, "native_prompt_not_sent", err.to_string());
            }
            input_queued = true;
        }
        encode_success(
            id,
            ResponseResult::NativePrompt {
                prompt: serde_json::json!({
                    "binding": binding, "screen": screen, "options": choices, "fingerprint": fingerprint,
                    "input_queued": input_queued, "acceptance": "unverified"
                }),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        detect::{Agent, AgentState},
        workspace::Workspace,
    };

    #[tokio::test]
    async fn native_prompt_answer_checks_binding_menu_and_lifecycle_before_one_key() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("prompt")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("prompt-fixture".into());
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Blocked);
        terminal.set_agent_session_ref(
            "herdr:claude".into(),
            "claude".into(),
            crate::agent_resume::AgentSessionRef::id("native-one"),
            Some(1),
        );
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Blocked);
        let (runtime, mut input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(b"Choose?\r\n1. First\r\n2. Second");
        app.state.insert_test_runtime(pane_id, runtime);
        let read = AgentPromptAnswerParams {
            target: "prompt-fixture".into(),
            option: None,
            expected_prompt: None,
        };
        let value: serde_json::Value =
            serde_json::from_str(&app.handle_agent_prompt_answer("read".into(), read.clone()))
                .unwrap();
        let fingerprint = value["result"]["prompt"]["fingerprint"]
            .as_str()
            .unwrap_or_else(|| panic!("prompt fingerprint missing: {value}"))
            .to_string();
        assert!(input.try_recv().is_err());
        let answer = AgentPromptAnswerParams {
            option: Some("2".into()),
            expected_prompt: Some(fingerprint),
            ..read.clone()
        };
        let invalid = AgentPromptAnswerParams {
            option: Some("9".into()),
            ..answer.clone()
        };
        let value: serde_json::Value =
            serde_json::from_str(&app.handle_agent_prompt_answer("invalid".into(), invalid))
                .unwrap();
        assert_eq!(value["error"]["code"], "invalid_option");
        assert!(input.try_recv().is_err());
        let value: serde_json::Value =
            serde_json::from_str(&app.handle_agent_prompt_answer("send".into(), answer.clone()))
                .unwrap();
        assert_eq!(value["result"]["prompt"]["input_queued"], true);
        assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"2"));
        assert!(input.try_recv().is_err());
        app.lookup_runtime_sender(0, pane_id)
            .unwrap()
            .test_process_pty_bytes(b"\r\nchanged prompt");
        let value: serde_json::Value =
            serde_json::from_str(&app.handle_agent_prompt_answer("changed".into(), answer.clone()))
                .unwrap();
        assert_eq!(value["error"]["code"], "native_prompt_changed");
        assert!(input.try_recv().is_err());
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Claude), AgentState::Idle);
        let value: serde_json::Value =
            serde_json::from_str(&app.handle_agent_prompt_answer("idle".into(), answer)).unwrap();
        assert_eq!(value["error"]["code"], "native_prompt_unavailable");
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn native_prompt_content_race_rejects_before_enqueue() {
        let (runtime, mut input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let before = runtime.content_seq();
        runtime.test_process_pty_bytes(b"changed");
        assert!(runtime
            .send_if_content_unchanged(before, Bytes::from_static(b"1"))
            .is_err());
        assert!(input.try_recv().is_err());
        assert!(runtime
            .send_if_content_unchanged(runtime.content_seq(), Bytes::from_static(b"2"))
            .is_ok());
        assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"2"));
    }

    #[test]
    fn native_prompt_options_are_explicit_and_bounded() {
        let menu = options("Choose a mode?\n❯ 1. First mode\n  2. Second mode").unwrap();
        assert_eq!(
            menu[0],
            serde_json::json!({"key":"1", "label":"First mode"})
        );
        assert_eq!(menu[1]["key"], "2");
        assert!(options("Press enter to approve").is_none());
        assert!(options("1. First\n1. Repeated\n2. Second").is_none());
        assert!(options(&format!("{}\n1. A\n2. B", "x".repeat(4096))).is_none());
        assert!(options(&format!("{}1. A\n2. B", "x\n".repeat(32))).is_none());
    }
}
