//! Native dialog inspection and guarded input belong to the runtime, not clients.
use std::cmp::Ordering;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use sha2::{Digest, Sha256};

use super::responses::{encode_error, encode_error_body, encode_success};
use crate::api::schema::{AgentPromptAnswerParams, AgentStatus, ResponseResult};
use crate::app::agents::runtime_hosts_agent;
use crate::app::App;
use crate::detect::{manifest::after_last_horizontal_rule, Agent};
use crate::terminal::TerminalRuntime;

const CLAUDE_TRUST_LABELS: [&str; 2] = ["No, exit", "Yes, I trust this folder"];
const SELECTION_TIMEOUT: Duration = Duration::from_millis(500);
const SELECTION_POLL_INTERVAL: Duration = Duration::from_millis(10);

fn bounded_menu(screen: &str) -> bool {
    screen.len() <= 4096 && screen.lines().count() <= 32
}

fn numbered_options(screen: &str) -> Option<Vec<serde_json::Value>> {
    if !bounded_menu(screen) {
        return None;
    }
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in screen.lines() {
        let line = line
            .trim_start()
            .trim_start_matches(['❯', '>', '›'])
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

struct TrustMenu<'a> {
    agent: Agent,
    screen: &'a str,
    outside_options: (&'a str, &'a str, &'a str),
    labels: [&'a str; 2],
    selected: usize,
}

impl<'a> TrustMenu<'a> {
    fn parse(agent: Agent, screen: &'a str, region: &'a str) -> Option<Self> {
        if !bounded_menu(region) {
            return None;
        }
        let (heading, footer, marker, body_prefix) = match agent {
            Agent::Claude => (
                "Accessing workspace:",
                "Enter to confirm · Esc to cancel",
                '❯',
                "Quick safety check: Is this a project you created or one you trust?",
            ),
            Agent::Codex => (
                "Folder access",
                "enter continue · esc back",
                '›',
                "Trust this folder? Codex can read, edit, and run files here,",
            ),
            _ => return None,
        };
        let mut lines = region.lines().filter(|line| !line.trim().is_empty());
        if lines.next_back()?.trim() != footer {
            return None;
        }
        let second = lines.next_back()?;
        let first = lines.next_back()?;
        if lines.next()?.trim() != heading {
            return None;
        }
        let path = lines.next()?.trim();
        if !Path::new(path).is_absolute() && !path.starts_with("~/") {
            return None;
        }
        let body = lines.map(str::trim).collect::<Vec<_>>().join(" ");
        if !body.starts_with(body_prefix)
            || (agent == Agent::Codex && !body.ends_with("Your trust decision will be saved."))
        {
            return None;
        }

        let mut selected = None;
        let mut labels = [first.trim(), second.trim()];
        for (index, label) in labels.iter_mut().enumerate() {
            if let Some(text) = label.strip_prefix(marker) {
                if selected.replace(index).is_some() {
                    return None;
                }
                *label = text.trim_start();
            }
        }
        if agent == Agent::Codex {
            labels[0] = labels[0].strip_prefix("1. ")?;
            labels[1] = labels[1].strip_prefix("2. ")?;
            if labels[0] != "Trust and continue" {
                return None;
            }
        } else if labels != CLAUDE_TRUST_LABELS {
            return None;
        }
        // Keep the complete detection screen outside the two option rows.
        let (before_second, after_options) = screen.rsplit_once(second)?;
        let (before_options, between_options) = before_second.rsplit_once(first)?;
        Some(Self {
            agent,
            screen,
            outside_options: (before_options, between_options, after_options),
            labels,
            selected: selected?,
        })
    }

    fn options(&self) -> Vec<serde_json::Value> {
        self.labels
            .iter()
            .enumerate()
            .map(|(index, label)| {
                let key = (index + 1).to_string();
                serde_json::json!({"key": key, "label": label})
            })
            .collect()
    }

    fn navigation_key(&self, selected: usize) -> Option<KeyCode> {
        match selected.cmp(&self.selected) {
            Ordering::Less => Some(KeyCode::Up),
            Ordering::Equal => None,
            Ordering::Greater => Some(KeyCode::Down),
        }
    }
}

fn send_prompt_key(runtime: &TerminalRuntime, seq: u64, key: KeyCode) -> io::Result<()> {
    let event = KeyEvent::new(key, KeyModifiers::NONE);
    let bytes = runtime.encode_terminal_key(event.into());
    if bytes.is_empty() {
        return Err(io::Error::other("option key encoded no bytes"));
    }
    // The writer-locked revision check pins the verified screen to this enqueue.
    runtime.send_if_content_unchanged(seq, Bytes::from(bytes))
}

fn confirm_trust_selection(
    runtime: &TerminalRuntime,
    menu: &TrustMenu<'_>,
    selected: usize,
    mut observed_seq: u64,
) -> io::Result<()> {
    let deadline = Instant::now() + SELECTION_TIMEOUT;
    let (seq, screen) = loop {
        let seq = runtime.content_seq();
        if seq != observed_seq && seq.is_multiple_of(2) {
            let screen = runtime.detection_text();
            if runtime.content_seq() == seq {
                if screen != menu.screen {
                    break (seq, screen);
                }
                observed_seq = seq;
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("trust menu selection did not redraw"));
        }
        std::thread::sleep(SELECTION_POLL_INTERVAL);
    };
    let after = TrustMenu::parse(menu.agent, &screen, after_last_horizontal_rule(&screen));
    if !after.is_some_and(|after| {
        after.outside_options == menu.outside_options
            && after.labels == menu.labels
            && after.selected == selected
    }) {
        return Err(io::Error::other("trust menu changed after navigation"));
    }
    if !runtime_hosts_agent(runtime, menu.agent) {
        return Err(io::Error::other(
            "foreground agent changed after navigation",
        ));
    }
    send_prompt_key(runtime, seq, KeyCode::Enter)
}

fn answer_trust_menu(
    runtime: &TerminalRuntime,
    menu: &TrustMenu<'_>,
    selected: usize,
    seq: u64,
) -> Result<(), (&'static str, io::Error)> {
    let Some(key) = menu.navigation_key(selected) else {
        return send_prompt_key(runtime, seq, KeyCode::Enter)
            .map_err(|err| ("native_prompt_not_sent", err));
    };
    send_prompt_key(runtime, seq, key).map_err(|err| ("native_prompt_not_sent", err))?;
    let confirmed = confirm_trust_selection(runtime, menu, selected, seq);
    confirmed.map_err(|err| ("native_prompt_partial", err))
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
        let startup = agent.launch_pending && agent.agent_session.is_none();
        let conversation = !agent.launch_pending && agent.agent_session.is_some();
        if agent.agent_status != AgentStatus::Blocked || !(startup || conversation) {
            return encode_error(
                id,
                "native_prompt_unavailable",
                "a blocked native conversation or pending launch is required",
            );
        }
        if startup && (params.target != agent.pane_id || params.terminal_id.is_none()) {
            return encode_error(
                id,
                "native_prompt_unavailable",
                "startup requires the exact pane ID as target and a terminal ID",
            );
        }
        if params
            .terminal_id
            .as_ref()
            .is_some_and(|terminal_id| terminal_id != &agent.terminal_id)
        {
            return encode_error(
                id,
                "native_prompt_changed",
                "target terminal binding changed; inspect again",
            );
        }
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some(terminal) = self.state.terminals.get(resolved.terminal_id.as_str()) else {
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
        if !runtime_hosts_agent(runtime, expected_agent) {
            return encode_error(id, "native_prompt_unavailable", "foreground agent changed");
        }
        let seq = runtime.content_seq();
        let full_screen = runtime.detection_text();
        if !seq.is_multiple_of(2) || runtime.content_seq() != seq {
            return encode_error(
                id,
                "native_prompt_changed",
                "terminal changed during capture",
            );
        }
        // Reuse Herdr's existing structural region helper. A long transcript
        // above the modal is not part of the displayed decision, but remains
        // covered by the full-screen fingerprint and final content guard.
        let screen = after_last_horizontal_rule(&full_screen);
        let trust_menu = if startup {
            TrustMenu::parse(expected_agent, &full_screen, screen)
        } else {
            None
        };
        let choices = if let Some(menu) = &trust_menu {
            menu.options()
        } else if let Some(choices) = numbered_options(screen) {
            choices
        } else {
            return encode_error(
                id,
                "native_prompt_unsupported",
                "a complete bounded numbered menu or supported startup trust menu is required",
            );
        };
        let binding = serde_json::json!({
            "terminal_id": agent.terminal_id,
            "pane_id": agent.pane_id,
            "agent": agent.agent,
            "native_session": agent.agent_session,
            "state_change_seq": agent.state_change_seq
        });
        let evidence = serde_json::json!([binding, full_screen]).to_string();
        let fingerprint = format!("{:x}", Sha256::digest(evidence.as_bytes()));
        let mut input_queued = false;
        if let Some(option) = &params.option {
            if params.expected_prompt.as_deref() != Some(fingerprint.as_str()) {
                return encode_error(
                    id,
                    "native_prompt_changed",
                    "prompt or native binding changed; inspect again",
                );
            }
            let Some(index) = choices
                .iter()
                .position(|item| item["key"].as_str() == Some(option.as_str()))
            else {
                return encode_error(
                    id,
                    "invalid_option",
                    "option is not an explicit key in this prompt",
                );
            };
            let result = if let Some(menu) = trust_menu {
                answer_trust_menu(runtime, &menu, index, seq)
            } else {
                let key = KeyCode::Char(char::from(option.as_bytes()[0]));
                send_prompt_key(runtime, seq, key).map_err(|err| ("native_prompt_not_sent", err))
            };
            if let Err((code, err)) = result {
                return encode_error(id, code, err.to_string());
            }
            input_queued = true;
        }
        encode_success(
            id,
            ResponseResult::NativePrompt {
                prompt: serde_json::json!({
                    "binding": binding,
                    "screen": screen,
                    "screen_region": "after_last_horizontal_rule",
                    "options": choices,
                    "fingerprint": fingerprint,
                    "startup": startup,
                    "input_queued": input_queued,
                    "acceptance": "unverified"
                }),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, detect::AgentState, workspace::Workspace};
    use tokio::sync::mpsc;

    const CLAUDE_TRUST: &str = concat!(
        "$ claude --settings /state/workers/worker1/claude-settings.json\r\n",
        "────────────────────────\r\n",
        "Accessing workspace:\r\n",
        "~/project\r\n",
        "Quick safety check: Is this a project you created or one you trust?\r\n",
        "(Like your own code, a well-known open source project, or work from your team).\r\n",
        "If not, take a moment to review what's in this folder first.\r\n",
        "Claude Code'll be able to read, edit, and execute files here. Security guide\r\n",
        "❯ No, exit\r\n",
        "  Yes, I trust this folder\r\n",
        "Enter to confirm · Esc to cancel",
    );
    const CODEX_TRUST: &str = concat!(
        "Folder access\r\n",
        "~/project\r\n",
        "Trust this folder? Codex can read, edit, and run files here, subject to your\r\n",
        "permission settings. Folder settings can run code automatically, even without\r\n",
        "a model request. Continue only if you trust these files. Your trust decision\r\n",
        "will be saved.\r\n",
        "› 1. Trust and continue\r\n",
        "  2. Back to Agent Command Center\r\n",
        "enter continue · esc back",
    );

    fn startup_fixture(
        agent: Agent,
        screen: &str,
    ) -> (App, AgentPromptAnswerParams, mpsc::Receiver<Bytes>) {
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("startup")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.begin_managed_agent(
            "prompt-fixture".into(),
            agent,
            Instant::now(),
            Duration::from_secs(3),
            Duration::from_secs(30),
        );
        terminal.set_detected_state(Some(agent), AgentState::Blocked);
        let (runtime, input) = TerminalRuntime::test_with_channel(100, 24);
        runtime.test_process_pty_bytes(screen.as_bytes());
        app.state.insert_test_runtime(pane_id, runtime);
        let read = AgentPromptAnswerParams {
            target: app.public_pane_id(0, pane_id).unwrap(),
            terminal_id: Some(terminal_id.to_string()),
            option: None,
            expected_prompt: None,
        };
        (app, read, input)
    }

    fn request(app: &mut App, params: AgentPromptAnswerParams) -> serde_json::Value {
        serde_json::from_str(&app.handle_agent_prompt_answer("prompt".into(), params)).unwrap()
    }

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
            terminal_id: None,
            option: None,
            expected_prompt: None,
        };
        let response = app.handle_agent_prompt_answer("read".into(), read.clone());
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["result"]["prompt"]["startup"], false);
        let fingerprint = value["result"]["prompt"]["fingerprint"]
            .as_str()
            .unwrap_or_else(|| panic!("prompt fingerprint missing: {value}"))
            .to_string();
        assert!(input.try_recv().is_err());
        let answer = AgentPromptAnswerParams {
            option: Some("2".into()),
            expected_prompt: Some(fingerprint),
            ..read
        };
        let invalid = AgentPromptAnswerParams {
            option: Some("9".into()),
            ..answer.clone()
        };
        let response = app.handle_agent_prompt_answer("invalid".into(), invalid);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["error"]["code"], "invalid_option");
        assert!(input.try_recv().is_err());
        let response = app.handle_agent_prompt_answer("send".into(), answer.clone());
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["result"]["prompt"]["input_queued"], true);
        assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"2"));
        assert!(input.try_recv().is_err());
        app.lookup_runtime_sender(0, pane_id)
            .unwrap()
            .test_process_pty_bytes(b"\r\nchanged prompt");
        let response = app.handle_agent_prompt_answer("changed".into(), answer.clone());
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["error"]["code"], "native_prompt_changed");
        assert!(input.try_recv().is_err());
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Claude), AgentState::Idle);
        let response = app.handle_agent_prompt_answer("idle".into(), answer);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["error"]["code"], "native_prompt_unavailable");
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn codex_startup_trust_requires_exact_binding_and_confirms_current_selection() {
        let (mut app, read, mut input) = startup_fixture(Agent::Codex, CODEX_TRUST);
        let unbound = AgentPromptAnswerParams {
            terminal_id: None,
            ..read.clone()
        };
        let value = request(&mut app, unbound);
        assert_eq!(value["error"]["code"], "native_prompt_unavailable");

        let alias = AgentPromptAnswerParams {
            target: "prompt-fixture".into(),
            ..read.clone()
        };
        let value = request(&mut app, alias);
        assert_eq!(value["error"]["code"], "native_prompt_unavailable");

        let value = request(&mut app, read.clone());
        let prompt = &value["result"]["prompt"];
        assert_eq!(prompt["startup"], true);
        assert!(prompt["binding"]["native_session"].is_null());
        assert_eq!(
            prompt["options"],
            serde_json::json!([
                {"key": "1", "label": "Trust and continue"},
                {"key": "2", "label": "Back to Agent Command Center"}
            ])
        );
        let answer = AgentPromptAnswerParams {
            option: Some("1".into()),
            expected_prompt: Some(prompt["fingerprint"].as_str().unwrap().into()),
            ..read
        };
        let unbound = AgentPromptAnswerParams {
            terminal_id: None,
            ..answer.clone()
        };
        let value = request(&mut app, unbound);
        assert_eq!(value["error"]["code"], "native_prompt_unavailable");
        let wrong_terminal = AgentPromptAnswerParams {
            terminal_id: Some("wrong-terminal".into()),
            ..answer.clone()
        };
        let value = request(&mut app, wrong_terminal);
        assert_eq!(value["error"]["code"], "native_prompt_changed");
        assert!(input.try_recv().is_err());

        let navigate = AgentPromptAnswerParams {
            option: Some("2".into()),
            ..answer.clone()
        };
        let value = request(&mut app, navigate);
        assert_eq!(value["error"]["code"], "native_prompt_partial");
        assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"\x1b[B"));
        assert!(input.try_recv().is_err());

        let value = request(&mut app, answer);
        assert_eq!(value["result"]["prompt"]["startup"], true);
        assert_eq!(value["result"]["prompt"]["input_queued"], true);
        assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"\r"));
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn codex_startup_menu_with_a_different_footer_sends_one_digit() {
        let screen = CODEX_TRUST.replace("esc back", "esc skip");
        let (mut app, read, mut input) = startup_fixture(Agent::Codex, &screen);
        let value = request(&mut app, read.clone());
        let prompt = &value["result"]["prompt"];
        let answer = AgentPromptAnswerParams {
            option: Some("2".into()),
            expected_prompt: Some(prompt["fingerprint"].as_str().unwrap().into()),
            ..read
        };

        let value = request(&mut app, answer);
        assert_eq!(value["result"]["prompt"]["input_queued"], true);
        assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"2"));
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn codex_startup_trust_confirms_option_two_only_after_the_expected_redraw() {
        let selected = CODEX_TRUST.replace(
            "› 1. Trust and continue\r\n  2. Back to Agent Command Center",
            "  1. Trust and continue\r\n› 2. Back to Agent Command Center",
        );
        for (redraw, confirmed) in [
            (selected.replace("~/project", "~/other-folder"), false),
            (selected.replace("Back to Agent Command Center", "Exit"), false),
            (selected, true),
        ] {
            let (runtime, mut input) = TerminalRuntime::test_with_channel(100, 24);
            runtime.test_process_pty_bytes(CODEX_TRUST.as_bytes());
            let seq = runtime.content_seq();
            let screen = runtime.detection_text();
            let region = after_last_horizontal_rule(&screen);
            let menu = TrustMenu::parse(Agent::Codex, &screen, region).unwrap();
            let key = menu.navigation_key(1).unwrap();
            send_prompt_key(&runtime, seq, key).unwrap();
            assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"\x1b[B"));

            let redraw = format!("\x1b[H\x1b[2J{redraw}");
            runtime.test_process_pty_bytes(redraw.as_bytes());
            let result = confirm_trust_selection(&runtime, &menu, 1, seq);
            if confirmed {
                result.unwrap();
                assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"\r"));
            } else {
                assert_eq!(
                    result.unwrap_err().to_string(),
                    "trust menu changed after navigation"
                );
            }
            assert!(input.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn startup_prompt_requires_blocked_pending_launch_without_session() {
        let (mut app, read, mut input) = startup_fixture(Agent::Codex, CODEX_TRUST);
        let terminal_id = read.terminal_id.as_deref().unwrap();
        let terminal = app.state.terminals.get_mut(terminal_id).unwrap();
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "herdr:codex".into(),
            agent: "codex".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("native-one").unwrap(),
        });
        let value = request(&mut app, read.clone());
        assert_eq!(value["error"]["code"], "native_prompt_unavailable");

        let terminal = app.state.terminals.get_mut(terminal_id).unwrap();
        terminal.persisted_agent_session = None;
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Working);
        let value = request(&mut app, read.clone());
        assert_eq!(value["error"]["code"], "native_prompt_unavailable");

        let terminal = app.state.terminals.get_mut(terminal_id).unwrap();
        terminal.clear_agent_name();
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Blocked);
        let value = request(&mut app, read);
        assert_eq!(value["error"]["code"], "native_prompt_unavailable");
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn claude_startup_prompt_previews_indexed_options_and_confirms_current_selection() {
        let (mut app, read, mut input) = startup_fixture(Agent::Claude, CLAUDE_TRUST);
        let value = request(&mut app, read.clone());
        let prompt = &value["result"]["prompt"];
        assert_eq!(prompt["startup"], true);
        assert_eq!(
            prompt["options"],
            serde_json::json!([
                {"key": "1", "label": "No, exit"},
                {"key": "2", "label": "Yes, I trust this folder"}
            ])
        );
        let answer = AgentPromptAnswerParams {
            option: Some("1".into()),
            expected_prompt: Some(prompt["fingerprint"].as_str().unwrap().into()),
            ..read
        };
        let navigate = AgentPromptAnswerParams {
            option: Some("2".into()),
            ..answer.clone()
        };
        let value = request(&mut app, navigate);
        assert_eq!(value["error"]["code"], "native_prompt_partial");
        assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"\x1b[B"));
        assert!(input.try_recv().is_err());

        let value = request(&mut app, answer);
        assert_eq!(value["result"]["prompt"]["input_queued"], true);
        assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"\r"));
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn claude_startup_prompt_confirms_only_the_expected_redraw_after_navigation() {
        let selected = CLAUDE_TRUST.replace(
            "❯ No, exit\r\n  Yes, I trust this folder",
            "  No, exit\r\n❯ Yes, I trust this folder",
        );
        for (redraw, confirmed) in [
            (selected.replace("~/project", "~/other-folder"), false),
            (selected.replace("$ claude", "$ different launch"), false),
            (selected, true),
        ] {
            let (runtime, mut input) = TerminalRuntime::test_with_channel(100, 24);
            runtime.test_process_pty_bytes(CLAUDE_TRUST.as_bytes());
            let seq = runtime.content_seq();
            let screen = runtime.detection_text();
            let region = after_last_horizontal_rule(&screen);
            let menu = TrustMenu::parse(Agent::Claude, &screen, region).unwrap();
            send_prompt_key(&runtime, seq, KeyCode::Down).unwrap();
            assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"\x1b[B"));

            let redraw = format!("\x1b[H\x1b[2J{redraw}");
            runtime.test_process_pty_bytes(redraw.as_bytes());
            let result = confirm_trust_selection(&runtime, &menu, 1, seq);
            if confirmed {
                result.unwrap();
                assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"\r"));
            } else {
                assert_eq!(
                    result.unwrap_err().to_string(),
                    "trust menu changed after navigation"
                );
            }
            assert!(input.try_recv().is_err());
        }
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
        let before_resize = runtime.content_seq();
        runtime.resize(25, 80, 0, 0);
        assert!(runtime
            .send_if_content_unchanged(before_resize, Bytes::from_static(b"1"))
            .is_err());
        assert!(input.try_recv().is_err());
        assert!(runtime
            .send_if_content_unchanged(runtime.content_seq(), Bytes::from_static(b"2"))
            .is_ok());
        assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"2"));
    }

    #[test]
    fn native_prompt_options_are_explicit_and_bounded() {
        let menu = numbered_options("Choose a mode?\n❯ 1. First mode\n  2. Second mode").unwrap();
        assert_eq!(
            menu[0],
            serde_json::json!({"key":"1", "label":"First mode"})
        );
        assert_eq!(menu[1]["key"], "2");
        assert!(numbered_options("Press enter to approve").is_none());
        assert!(numbered_options("1. First\n1. Repeated\n2. Second").is_none());
        assert!(numbered_options(&format!("{}\n1. A\n2. B", "x".repeat(4096))).is_none());
        assert!(numbered_options(&format!("{}1. A\n2. B", "x\n".repeat(32))).is_none());
        let transcript_and_dialog = format!(
            "{}\n────────────────────────\nChoose?\n1. A\n2. B",
            "history\n".repeat(50)
        );
        let region = after_last_horizontal_rule(&transcript_and_dialog);
        assert_eq!(region, "Choose?\n1. A\n2. B");
        assert!(numbered_options(region).is_some());
    }
}
