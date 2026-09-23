//! A terminal client for an external DSH ACP runtime. The child belongs to this
//! pane; no listener, broker, credentials, or copy of DSH is installed by Herdr.
use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyModifiers,
};
use crossterm::{execute, terminal};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const MAX_FRAME: usize = 1024 * 1024;
const MAX_TEXT: usize = 128 * 1024;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(45);

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
fn safe_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

#[derive(Default)]
struct Options {
    binary: String,
    catalog: bool,
    resume: Option<String>,
    journal: Option<PathBuf>,
    initial: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}
impl Options {
    fn parse(args: &[String]) -> io::Result<Self> {
        let mut options = Self {
            binary: "dsh".into(),
            ..Self::default()
        };
        let mut args = args.iter();
        while let Some(key) = args.next() {
            if key == "--catalog" {
                options.catalog = true;
                continue;
            }
            let value = args
                .next()
                .ok_or_else(|| invalid(format!("missing value for {key}")))?;
            if value.is_empty() || value.chars().any(char::is_control) {
                return Err(invalid("empty or control-character launch argument"));
            }
            match key.as_str() {
                "--dsh-bin" => options.binary = value.clone(),
                "--resume" => options.resume = Some(value.clone()),
                "--journal" => options.journal = Some(PathBuf::from(value)),
                "--initial-prompt" => options.initial = Some(value.clone()),
                "--model" => options.model = Some(value.clone()),
                "--effort" => options.effort = Some(value.clone()),
                _ => return Err(invalid(format!("unknown DSH driver option: {key}"))),
            }
        }
        if options.catalog
            && (options.resume.is_some() || options.initial.is_some() || options.journal.is_some())
        {
            return Err(invalid(
                "catalog inspection cannot resume, prompt, or write a journal",
            ));
        }
        if options.resume.is_some() && options.initial.is_some() {
            return Err(invalid("resume never replays an initializer"));
        }
        if options.initial.as_ref().is_some_and(|s| s.len() > MAX_TEXT) {
            return Err(invalid("initializer exceeds 128 KiB"));
        }
        if options.journal.as_ref().is_some_and(|p| !p.is_absolute()) {
            return Err(invalid("journal must be an absolute path"));
        }
        Ok(options)
    }
}

struct RawTerminal;
impl RawTerminal {
    fn open() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self;
        execute!(io::stdout(), EnableBracketedPaste)?;
        Ok(guard)
    }
}
impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = terminal::disable_raw_mode();
    }
}

struct Acp {
    child: Child,
    input: Option<ChildStdin>,
    frames: Receiver<io::Result<Value>>,
    sequence: u64,
}
impl Acp {
    fn start(binary: &str) -> io::Result<Self> {
        let mut child = Command::new(binary)
            .args(["--profile", "acp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let input = child.stdin.take();
        let output = child
            .stdout
            .take()
            .ok_or_else(|| invalid("missing ACP stdout"))?;
        let (tx, frames) = mpsc::sync_channel(64);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(output);
            loop {
                let mut line = Vec::new();
                let result = (&mut reader)
                    .take((MAX_FRAME + 1) as u64)
                    .read_until(b'\n', &mut line)
                    .and_then(|count| {
                        if count == 0 {
                            return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "DSH ACP disconnected",
                            ));
                        }
                        if count > MAX_FRAME || !line.ends_with(b"\n") {
                            return Err(invalid("oversized ACP frame"));
                        }
                        serde_json::from_slice(&line)
                            .map_err(|e| invalid(format!("invalid ACP frame: {e}")))
                    });
                let failed = result.is_err();
                if tx.send(result).is_err() || failed {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            input,
            frames,
            sequence: 0,
        })
    }
    fn write(&mut self, value: Value) -> io::Result<()> {
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| invalid("ACP input closed"))?;
        serde_json::to_writer(&mut *input, &value)?;
        input.write_all(b"\n")?;
        input.flush()
    }
    fn request(&mut self, method: &str, params: Value) -> io::Result<u64> {
        self.sequence += 1;
        self.write(json!({"jsonrpc":"2.0","id":self.sequence,"method":method,"params":params}))?;
        Ok(self.sequence)
    }
    fn notify(&mut self, method: &str, params: Value) -> io::Result<()> {
        self.write(json!({"jsonrpc":"2.0","method":method,"params":params}))
    }
}
impl Drop for Acp {
    fn drop(&mut self) {
        self.input.take();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Driver {
    acp: Acp,
    interactive: bool,
    pane: Option<String>,
    seq: u64,
    session: String,
    journal: Option<PathBuf>,
    snapshot: Value,
    permissions: VecDeque<Value>,
    reply: String,
    reply_truncated: bool,
    prompt: Option<(String, f64)>,
}
impl Driver {
    fn new(binary: &str) -> io::Result<Self> {
        Ok(Self {
            acp: Acp::start(binary)?,
            interactive: true,
            pane: std::env::var("HERDR_PANE_ID").ok(),
            seq: (now() * 1_000_000.0) as u64,
            session: String::new(),
            journal: None,
            snapshot: json!({"protocol":1,"source":"herdr:dsh","cwd":std::env::current_dir()?,
                "driver_pid":std::process::id(),"lifecycle":"unknown","background_count":null}),
            permissions: VecDeque::new(),
            reply: String::new(),
            reply_truncated: false,
            prompt: None,
        })
    }
    fn remember_selection(&mut self, options: &Value) {
        for (id, field) in [("model", "model"), ("reasoning_effort", "effort")] {
            self.snapshot[field] = options
                .as_array()
                .and_then(|v| v.iter().find(|v| v["id"] == id))
                .map(|v| v["currentValue"].clone())
                .unwrap_or(Value::Null);
        }
        self.snapshot["selection_at"] = json!(now());
    }
    fn report_session_start(&mut self, resumed: bool) -> io::Result<()> {
        if let Some(pane) = &self.pane {
            self.seq += 1;
            // Full lifecycle integrations must anchor a new process generation
            // before state reports can establish its native session identity.
            let request = serde_json::from_value(json!({"id":"dsh:session",
                "method":"pane.report_agent_session","params":{"pane_id":pane,
                "source":"herdr:dsh","agent":"dsh","agent_session_id":self.session,
                "session_start_source":if resumed { "resume" } else { "startup" },
                "seq":self.seq}}))?;
            let response = super::send_request(&request)?;
            if response.get("error").is_some() {
                return Err(invalid("Herdr rejected DSH session identity"));
            }
        }
        Ok(())
    }
    fn report(&mut self, state: &str) -> io::Result<()> {
        self.snapshot["lifecycle"] = json!(state);
        self.save()?;
        if let Some(pane) = &self.pane {
            self.seq += 1;
            let request =
                serde_json::from_value(json!({"id":"dsh:state","method":"pane.report_agent",
                "params":{"pane_id":pane,"source":"herdr:dsh","agent":"dsh","state":state,
                    "seq":self.seq,"agent_session_id":self.session}}))?;
            let response = super::send_request(&request)?;
            if response.get("error").is_some() {
                return Err(invalid("Herdr rejected DSH lifecycle report"));
            }
        }
        Ok(())
    }
    fn save(&mut self) -> io::Result<()> {
        let Some(path) = &self.journal else {
            return Ok(());
        };
        self.snapshot["observed_at"] = json!(now());
        self.snapshot["pane_id"] = json!(self.pane);
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        let mut file = crate::platform::create_private_state_file(&tmp)?;
        serde_json::to_writer(&mut file, &self.snapshot)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        crate::platform::replace_file(&tmp, path)?;
        Ok(())
    }
    fn response(&mut self, id: u64, timeout: Option<Duration>) -> io::Result<Value> {
        let started = Instant::now();
        loop {
            match self.acp.frames.recv_timeout(Duration::from_millis(25)) {
                Ok(frame) => {
                    let frame = frame?;
                    if frame.get("id").and_then(Value::as_u64) == Some(id)
                        && frame.get("method").is_none()
                    {
                        if let Some(error) = frame.get("error") {
                            return Err(invalid(format!(
                                "DSH request failed: {}",
                                safe_text(&error.to_string())
                            )));
                        }
                        return frame
                            .get("result")
                            .cloned()
                            .ok_or_else(|| invalid("missing ACP result"));
                    }
                    if self.interactive {
                        self.frame(frame)?;
                    } else if frame.get("id").is_some() {
                        return Err(invalid("catalog inspection cannot service agent requests"));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(invalid("DSH ACP reader stopped"))
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if timeout.is_some_and(|limit| started.elapsed() >= limit) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "ACP outcome uncertain; request was not retried",
                ));
            }
            if self.interactive && event::poll(Duration::ZERO)? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != event::KeyEventKind::Press {
                        continue;
                    }
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                    {
                        if !self.session.is_empty() {
                            self.acp
                                .notify("session/cancel", json!({"sessionId":self.session}))?;
                        }
                    } else if let KeyCode::Char(choice) = key.code {
                        self.permission_choice(choice)?;
                    }
                }
            }
        }
    }
    fn call(&mut self, method: &str, params: Value) -> io::Result<Value> {
        let id = self.acp.request(method, params)?;
        self.response(id, Some(CONTROL_TIMEOUT))
    }
    fn frame(&mut self, frame: Value) -> io::Result<()> {
        match frame.get("method").and_then(Value::as_str) {
            Some("session/update") => {
                let params = &frame["params"];
                if params["sessionId"].as_str() != Some(&self.session) {
                    return Err(invalid("ACP update changed session identity"));
                }
                let update = &params["update"];
                match update["sessionUpdate"].as_str() {
                    Some("agent_message_chunk") => {
                        if let Some(text) = update["content"]["text"].as_str() {
                            print!("{}", safe_text(text).replace('\n', "\r\n"));
                            io::stdout().flush()?;
                            if self.prompt.is_some() {
                                if self.reply.len() + text.len() <= MAX_TEXT {
                                    self.reply.push_str(text);
                                } else {
                                    self.reply_truncated = true;
                                }
                            }
                        }
                    }
                    Some("config_option_update") => {
                        self.remember_selection(&update["configOptions"]);
                        self.save()?;
                    }
                    Some("usage_update") => {
                        self.snapshot["context_used"] = update["used"].clone();
                        self.snapshot["context_limit"] = update["size"].clone();
                        self.snapshot["telemetry_at"] = json!(now());
                        self.save()?;
                    }
                    Some("tool_call") => {
                        if let Some(title) = update["title"].as_str() {
                            print!("\r\n[tool] {}\r\n", safe_text(title));
                        }
                    }
                    _ => {}
                }
            }
            Some("session/request_permission") => {
                if frame["params"]["sessionId"].as_str() != Some(&self.session) {
                    return Err(invalid("permission changed session identity"));
                }
                if self.permissions.len() >= 16 {
                    return Err(invalid("too many pending permission requests"));
                }
                self.permissions.push_back(frame);
                self.report("blocked")?;
                self.show_permission();
            }
            Some(_) if frame.get("id").is_some() => {
                self.acp.write(json!({"jsonrpc":"2.0","id":frame["id"],"error":{"code":-32601,"message":"Unsupported client method"}}))?;
            }
            _ => {}
        }
        Ok(())
    }
    fn show_permission(&self) {
        if let Some(request) = self.permissions.front() {
            println!(
                "\r\nPermission: {}\r\n1 Allow once   2 Reject\r",
                safe_text(&request["params"]["toolCall"].to_string())
            );
        }
    }
    fn permission_choice(&mut self, choice: char) -> io::Result<()> {
        if !matches!(choice, '1' | '2') {
            return Ok(());
        }
        let Some(request) = self.permissions.front() else {
            return Ok(());
        };
        let kind = if choice == '1' {
            "allow_once"
        } else {
            "reject_once"
        };
        let option = request["params"]["options"]
            .as_array()
            .and_then(|options| options.iter().find(|v| v["kind"] == kind));
        let outcome = match option.and_then(|o| o["optionId"].as_str()) {
            Some(id) => json!({"outcome":"selected","optionId":id}),
            None if choice == '2' => json!({"outcome":"cancelled"}),
            None => return Ok(()),
        };
        self.acp
            .write(json!({"jsonrpc":"2.0","id":request["id"],"result":{"outcome":outcome}}))?;
        self.permissions.pop_front();
        if self.permissions.is_empty() {
            self.report("working")?;
        } else {
            self.show_permission();
        }
        Ok(())
    }
    fn prompt(&mut self, prompt: String) -> io::Result<()> {
        if prompt.trim().is_empty() || prompt.len() > MAX_TEXT {
            return Err(invalid("prompt must contain 1 to 131072 bytes"));
        }
        self.reply.clear();
        self.reply_truncated = false;
        self.prompt = Some((prompt.clone(), now()));
        self.report("working")?;
        let id = self.acp.request(
            "session/prompt",
            json!({"sessionId":self.session,"prompt":[{"type":"text","text":prompt}]}),
        )?;
        let result = self.response(id, None)?;
        // Cancellation can settle a turn while permission UI is still pending.
        // Choices from that turn must never carry into the next prompt.
        self.permissions.clear();
        // The protocol response follows committed output and native idle. Until
        // this response, terminal text and a successful pipe write prove nothing.
        let (prompt, submitted_at) = self
            .prompt
            .take()
            .ok_or_else(|| invalid("missing prompt intent"))?;
        self.snapshot["last_user_text"] = json!(prompt);
        self.snapshot["last_user_at"] = json!(submitted_at);
        self.snapshot["last_assistant_text"] = if self.reply_truncated {
            Value::Null
        } else {
            json!(self.reply)
        };
        self.snapshot["last_assistant_at"] = json!(now());
        self.snapshot["stop_reason"] = result["stopReason"].clone();
        self.snapshot["turn_id"] = json!(id);
        self.report("idle")?;
        print!("\r\n");
        Ok(())
    }
    fn read_prompt(&mut self) -> io::Result<Option<String>> {
        let mut input = String::new();
        print!("dsh> ");
        io::stdout().flush()?;
        loop {
            while let Ok(frame) = self.acp.frames.try_recv() {
                self.frame(frame?)?;
            }
            if self.acp.child.try_wait()?.is_some() {
                return Err(invalid("DSH exited"));
            }
            if !event::poll(Duration::from_millis(100))? {
                continue;
            }
            match event::read()? {
                Event::Paste(text) => {
                    if input.len() + text.len() > MAX_TEXT {
                        return Err(invalid("paste exceeds 128 KiB"));
                    }
                    input.push_str(&text);
                    print!("{}", safe_text(&text).replace('\n', "\r\n"));
                }
                Event::Key(key) if key.kind == event::KeyEventKind::Press => match key.code {
                    KeyCode::Enter => {
                        print!("\r\n");
                        if input.trim() == "/exit" {
                            return Ok(None);
                        }
                        if !input.trim().is_empty() {
                            return Ok(Some(input));
                        }
                    }
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(None)
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        input.clear();
                        print!("\r\ndsh> ");
                    }
                    KeyCode::Backspace => {
                        if input.pop().is_some() {
                            print!("\x08 \x08");
                        }
                    }
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        if input.len() + c.len_utf8() > MAX_TEXT {
                            return Err(invalid("prompt exceeds 128 KiB"));
                        }
                        input.push(c);
                        print!("{c}");
                    }
                    _ => {}
                },
                _ => {}
            }
            io::stdout().flush()?;
        }
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        self.snapshot["lifecycle"] = json!("exited");
        let _ = self.save();
        if let Some(pane) = &self.pane {
            let request =
                serde_json::from_value(json!({"id":"dsh:release","method":"pane.release_agent",
                "params":{"pane_id":pane,"source":"herdr:dsh","agent":"dsh","seq":self.seq+1}}));
            if let Ok(request) = request {
                let _ = super::send_request(&request);
            }
        }
    }
}

fn configured_value(options: &Value, id: &str, requested: &str) -> io::Result<String> {
    fn collect(value: &Value, values: &mut Vec<String>) {
        if let Some(items) = value.as_array() {
            for item in items {
                if let Some(value) = item["value"].as_str() {
                    values.push(value.into());
                }
                collect(&item["options"], values);
            }
        }
    }
    let option = options
        .as_array()
        .and_then(|v| v.iter().find(|v| v["id"] == id))
        .ok_or_else(|| invalid(format!("DSH does not advertise {id}")))?;
    let mut values = Vec::new();
    collect(&option["options"], &mut values);
    if values.iter().any(|v| v == requested) {
        return Ok(requested.into());
    }
    let matches: Vec<_> = values
        .into_iter()
        .filter(|v| {
            serde_json::from_str::<Vec<String>>(v)
                .ok()
                .is_some_and(|parts| parts.len() == 2 && parts[1] == requested)
        })
        .collect();
    if matches.len() == 1 {
        return Ok(matches[0].clone());
    }
    Err(invalid(format!(
        "unsupported or ambiguous DSH {id}; use an advertised configuration value"
    )))
}

fn confirm_selection(options: &Value, id: &str, expected: &str) -> io::Result<()> {
    let applied = options
        .as_array()
        .and_then(|items| items.iter().find(|item| item["id"] == id))
        .and_then(|item| item["currentValue"].as_str());
    if applied != Some(expected) {
        return Err(invalid("DSH did not acknowledge the requested selection"));
    }
    Ok(())
}

fn session_record(session: &str) -> PathBuf {
    crate::config::state_dir().join("dsh").join(format!(
        "{:x}.launch.json",
        Sha256::digest(session.as_bytes())
    ))
}

fn write_private_json(path: &std::path::Path, value: &Value) -> io::Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = crate::platform::create_private_state_file(&tmp)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    crate::platform::replace_file(&tmp, path)
}

// Inspection creates and closes one empty native session, without a terminal,
// prompt, Herdr binding, launch record, or journal. DSH owns its native storage.
fn catalog(options: &Options) -> io::Result<i32> {
    let mut driver = Driver::new(&options.binary)?;
    driver.interactive = false;
    driver.pane = None;
    let hello = driver.call(
        "initialize",
        json!({"protocolVersion":1,"clientCapabilities":{},
        "clientInfo":{"name":"herdr-catalog","version":env!("CARGO_PKG_VERSION")}}),
    )?;
    if hello["protocolVersion"] != 1
        || hello["agentCapabilities"]["sessionCapabilities"]
            .get("close")
            .is_none()
    {
        return Err(invalid("DSH ACP v1 with session/close is required"));
    }
    let mut result = driver.call(
        "session/new",
        json!({"cwd":std::env::current_dir()?,"mcpServers":[]}),
    )?;
    driver.session = result["sessionId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("DSH returned no session identity"))?
        .into();
    let selection = (|| -> io::Result<()> {
        for (id, requested) in [
            ("model", &options.model),
            ("reasoning_effort", &options.effort),
        ] {
            if let Some(requested) = requested {
                let value = configured_value(&result["configOptions"], id, requested)?;
                result = driver.call(
                    "session/set_config_option",
                    json!({"sessionId":driver.session,"configId":id,"value":value}),
                )?;
                confirm_selection(&result["configOptions"], id, &value)?;
            }
        }
        Ok(())
    })();
    let closed = driver.call("session/close", json!({"sessionId":driver.session}));
    selection?;
    closed?;
    println!(
        "{}",
        json!({"driver":"herdr:dsh","protocol":1,"config_options":result["configOptions"]})
    );
    Ok(0)
}

pub(super) fn run(args: &[String]) -> io::Result<i32> {
    if args == ["--describe"] {
        println!(
            "{}",
            json!({"driver":"herdr:dsh","protocol":1,"transport":"acp-stdio",
            "launch":true,"prompt":true,"resume":true,"catalog":true,"permissions":"interactive-once","compact":false,"update":false})
        );
        return Ok(0);
    }
    let mut options = Options::parse(args)?;
    if options.catalog {
        return catalog(&options);
    }
    // A restored pane retains its original executable and journal, but never
    // an initial prompt. DSH itself verifies that the session is inactive.
    if let Some(session) = &options.resume {
        let record_path = session_record(session);
        if record_path.exists() {
            let raw = fs::read(&record_path)?;
            if raw.len() > 65536 {
                return Err(invalid("oversized DSH launch record"));
            }
            let record: Value = serde_json::from_slice(&raw)?;
            if record["session_id"] != *session || record["cwd"] != json!(std::env::current_dir()?)
            {
                return Err(invalid("DSH restore binding changed"));
            }
            if options.journal.is_none() {
                options.journal = record["journal"].as_str().map(PathBuf::from);
            }
            if options.binary == "dsh" {
                options.binary = record["binary"].as_str().unwrap_or("dsh").into();
            }
        }
    }
    let _terminal = RawTerminal::open()?;
    let mut driver = Driver::new(&options.binary)?;
    let hello = driver.call(
        "initialize",
        json!({"protocolVersion":1,"clientCapabilities":{},
        "clientInfo":{"name":"herdr","version":env!("CARGO_PKG_VERSION")}}),
    )?;
    if hello["protocolVersion"] != 1
        || hello["agentCapabilities"]["sessionCapabilities"]
            .get("close")
            .is_none()
    {
        return Err(invalid("DSH ACP v1 with session/close is required"));
    }
    let cwd = std::env::current_dir()?;
    let mut params = json!({"cwd":cwd,"mcpServers":[]});
    let method = if let Some(session) = &options.resume {
        if hello["agentCapabilities"]["sessionCapabilities"]
            .get("resume")
            .is_none()
        {
            return Err(invalid("DSH does not support resume"));
        }
        params["sessionId"] = json!(session);
        "session/resume"
    } else {
        "session/new"
    };
    let mut result = driver.call(method, params)?;
    driver.session = result["sessionId"]
        .as_str()
        .map(str::to_owned)
        .or(options.resume)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("DSH returned no session identity"))?;
    driver.snapshot["native_id"] = json!(driver.session);
    driver.snapshot["agent_info"] = hello["agentInfo"].clone();
    let path = options.journal.unwrap_or_else(|| {
        crate::config::state_dir().join("dsh").join(format!(
            "{:x}.json",
            Sha256::digest(driver.session.as_bytes())
        ))
    });
    fs::create_dir_all(
        path.parent()
            .ok_or_else(|| invalid("journal has no parent"))?,
    )?;
    let record = session_record(&driver.session);
    fs::create_dir_all(
        record
            .parent()
            .ok_or_else(|| invalid("launch record has no parent"))?,
    )?;
    write_private_json(
        &record,
        &json!({"protocol":1,"session_id":driver.session,
        "cwd":cwd,"binary":options.binary,"journal":path}),
    )?;
    driver.journal = Some(path);
    for (id, requested) in [
        ("model", options.model),
        ("reasoning_effort", options.effort),
    ] {
        if let Some(requested) = requested {
            let value = configured_value(&result["configOptions"], id, &requested)?;
            result = driver.call(
                "session/set_config_option",
                json!({"sessionId":driver.session,"configId":id,"value":value}),
            )?;
            confirm_selection(&result["configOptions"], id, &value)?;
        }
    }
    driver.remember_selection(&result["configOptions"]);
    println!(
        "DSH session {}\r\nUse /exit to close; Ctrl-C cancels active work.\r",
        safe_text(&driver.session)
    );
    driver.report_session_start(method == "session/resume")?;
    driver.report("idle")?;
    if let Some(prompt) = options.initial {
        driver.prompt(prompt)?;
    }
    while let Some(prompt) = driver.read_prompt()? {
        driver.prompt(prompt)?;
    }
    driver.call("session/close", json!({"sessionId":driver.session}))?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resume_cannot_replay_initialization() {
        assert!(Options::parse(&[
            "--resume".into(),
            "s".into(),
            "--initial-prompt".into(),
            "p".into()
        ])
        .is_err());
        assert!(Options::parse(&["--dsh-bin".into(), "dsh\nother".into()]).is_err());
        assert!(Options::parse(&["--journal".into(), "relative".into()]).is_err());
    }
    #[test]
    fn catalog_cannot_prompt_resume_or_write_worker_journal() {
        for key in ["--initial-prompt", "--resume", "--journal"] {
            assert!(Options::parse(&["--catalog".into(), key.into(), "/example".into()]).is_err());
        }
        assert!(Options::parse(&["--catalog".into()]).unwrap().catalog);
    }
    #[test]
    fn model_alias_requires_unique_advertised_route() {
        let options = json!([{"id":"model","options":[{"options":[{"value":"[\"a\",\"m\"]"},{"value":"[\"b\",\"m\"]"}]}]}]);
        assert!(configured_value(&options, "model", "m").is_err());
        assert_eq!(
            configured_value(&options, "model", "[\"a\",\"m\"]").unwrap(),
            "[\"a\",\"m\"]"
        );
        assert!(configured_value(&options, "model", "missing").is_err());
    }
    #[test]
    fn selection_acknowledgement_cannot_silently_substitute() {
        assert!(confirm_selection(&json!([]), "model", "a").is_err());
        assert!(
            confirm_selection(&json!([{"id":"model","currentValue":"b"}]), "model", "a").is_err()
        );
        assert!(
            confirm_selection(&json!([{"id":"model","currentValue":"a"}]), "model", "a").is_ok()
        );
    }
    #[test]
    fn untrusted_output_cannot_emit_terminal_controls() {
        assert_eq!(safe_text("hi\x1b[31m\x07\nthere"), "hi[31m\nthere");
    }
}
