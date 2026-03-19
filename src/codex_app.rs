use crate::native::{
    NativeAgentMetadata, NativeSessionMetadata, RuntimeContextMetadata, RuntimeFeedEntry,
    RuntimeSubagentMetadata,
};
use crate::tui::view_agent;
use anyhow::{Context, anyhow, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SESSION_DIR_NAME: &str = "sessions";
const MANIFEST_DIR_NAME: &str = "manifests";
const SOCKET_FILE_NAME: &str = "control.sock";
const METADATA_FILE_NAME: &str = "metadata.json";
const EVENTS_FILE_NAME: &str = "events.log";
const LOG_LIMIT_BYTES: usize = 512 * 1024;
const FEED_LIMIT: usize = 18;
const SUBAGENT_LIMIT: usize = 24;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexAppLaunchManifest {
    pub namespace: String,
    pub working_directory: Option<String>,
    pub shell_command: String,
    pub startup_prompt: String,
    pub images: Vec<String>,
    pub resume_session_id: Option<String>,
    pub created_at_epoch_ms: u128,
    pub context: RuntimeContextMetadata,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
enum ClientMessage {
    Metadata,
    Attach { agent: String },
    SendText { text: String },
    Interrupt,
    KillSession,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
enum ServerMessage {
    Ok,
    Error { message: String },
    Metadata(NativeSessionMetadata),
    Attached { namespace: String, agent: String },
    Output { data_base64: String },
    Exited { agent: String },
}

struct AppSessionState {
    metadata: NativeSessionMetadata,
    seen_log_items: HashSet<String>,
    active_agent_messages: BTreeMap<String, String>,
    active_command_outputs: BTreeMap<String, String>,
}

struct CodexAppSession {
    namespace: String,
    session_dir: PathBuf,
    state: Mutex<AppSessionState>,
    writer: Mutex<ChildStdin>,
    child: Mutex<Child>,
    pending: Mutex<HashMap<u64, mpsc::Sender<anyhow::Result<Value>>>>,
    next_request_id: AtomicU64,
    log: Mutex<VecDeque<u8>>,
    log_file: Mutex<File>,
    subscribers: Mutex<Vec<mpsc::Sender<Vec<u8>>>>,
    shutdown_requested: AtomicBool,
}

impl CodexAppSession {
    fn metadata(&self) -> NativeSessionMetadata {
        self.state.lock().unwrap().metadata.clone()
    }

    fn metadata_path(&self) -> PathBuf {
        self.session_dir.join(METADATA_FILE_NAME)
    }

    fn set_agent_running(&self, running: bool) -> anyhow::Result<()> {
        self.mutate_state(|state| {
            if let Some(agent) = state.metadata.agents.get_mut(0) {
                agent.running = running;
            }
        })
    }

    fn mutate_state<F>(&self, mutator: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut AppSessionState),
    {
        let raw = {
            let mut state = self.state.lock().unwrap();
            mutator(&mut state);
            serde_json::to_string_pretty(&state.metadata)
                .context("failed to encode codex app session metadata")?
        };
        fs::write(self.metadata_path(), raw).with_context(|| {
            format!(
                "failed to write codex app session metadata '{}'",
                self.metadata_path().display()
            )
        })
    }

    fn append_output(&self, chunk: &[u8]) -> anyhow::Result<()> {
        {
            let mut log = self.log.lock().unwrap();
            for byte in chunk {
                log.push_back(*byte);
            }
            while log.len() > LOG_LIMIT_BYTES {
                log.pop_front();
            }
        }

        {
            let mut file = self.log_file.lock().unwrap();
            file.write_all(chunk)
                .context("failed to append to codex app events log")?;
            file.flush()
                .context("failed to flush codex app events log")?;
        }

        let mut subscribers = self.subscribers.lock().unwrap();
        subscribers.retain(|sender| sender.send(chunk.to_vec()).is_ok());
        Ok(())
    }

    fn append_text_line(&self, line: impl AsRef<str>) -> anyhow::Result<()> {
        let mut rendered = line.as_ref().to_string();
        if !rendered.ends_with('\n') {
            rendered.push('\n');
        }
        self.append_output(rendered.as_bytes())
    }

    fn subscribe(&self) -> (Vec<u8>, mpsc::Receiver<Vec<u8>>) {
        let backlog = {
            let log = self.log.lock().unwrap();
            log.iter().copied().collect::<Vec<_>>()
        };
        let (tx, rx) = mpsc::channel();
        self.subscribers.lock().unwrap().push(tx);
        (backlog, rx)
    }

    fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let request = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        let write_result = self.send_json(&request);
        if let Err(error) = write_result {
            self.pending.lock().unwrap().remove(&id);
            return Err(error);
        }

        rx.recv_timeout(Duration::from_secs(30))
            .map_err(|_| anyhow!("timed out waiting for app-server response to {}", method))?
    }

    fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        let request = json!({
            "method": method,
            "params": params,
        });
        self.send_json(&request)
    }

    fn send_json(&self, value: &Value) -> anyhow::Result<()> {
        let raw = serde_json::to_string(value).context("failed to encode app-server payload")?;
        let mut writer = self.writer.lock().unwrap();
        writer
            .write_all(raw.as_bytes())
            .context("failed to write app-server payload")?;
        writer
            .write_all(b"\n")
            .context("failed to terminate app-server payload")?;
        writer.flush().context("failed to flush app-server payload")
    }

    fn handle_response(&self, value: Value) -> anyhow::Result<()> {
        let Some(id) = value.get("id").and_then(Value::as_u64) else {
            return Ok(());
        };
        let Some(sender) = self.pending.lock().unwrap().remove(&id) else {
            return Ok(());
        };

        if let Some(error) = value.get("error") {
            let message = format_rpc_error(error);
            let _ = sender.send(Err(anyhow!(message.clone())));
            self.record_error(&message)?;
            return Ok(());
        }

        let result = value.get("result").cloned().unwrap_or(Value::Null);
        let _ = sender.send(Ok(result));
        Ok(())
    }

    fn handle_notification(&self, method: &str, params: &Value) -> anyhow::Result<()> {
        match method {
            "error" => {
                let message = format_rpc_error(params);
                self.record_error(&message)?;
            }
            "thread/started" => {
                if let Some(thread) = params.get("thread") {
                    self.apply_thread(thread)?;
                    self.upsert_runtime_event(
                        format!(
                            "thread:{}",
                            thread_id_from_value(thread).unwrap_or_else(|| "main".to_string())
                        ),
                        "thread",
                        "Thread ready",
                        thread
                            .get("path")
                            .and_then(Value::as_str)
                            .map(|value| truncate_line(value, 180)),
                        thread.get("status").and_then(thread_status_from_value),
                        None,
                    )?;
                    self.append_text_line(format!(
                        "[thread] started {}",
                        thread_id_from_value(thread).unwrap_or_else(|| "-".to_string())
                    ))?;
                }
            }
            "thread/status/changed" => {
                let status = params
                    .get("status")
                    .and_then(thread_status_from_value)
                    .unwrap_or_else(|| "unknown".to_string());
                self.mutate_state(|state| {
                    let context = state.metadata.context.get_or_insert_with(Default::default);
                    context.thread_status = Some(status.clone());
                    context.last_activity = Some(format!("thread {}", status));
                })?;
                self.upsert_runtime_event(
                    format!("thread-status:{status}"),
                    "thread",
                    "Thread status changed",
                    None,
                    Some(status),
                    None,
                )?;
            }
            "turn/started" => {
                if let Some(turn) = params.get("turn") {
                    self.apply_turn(turn)?;
                    self.upsert_runtime_event(
                        format!(
                            "turn:{}",
                            turn.get("id").and_then(Value::as_str).unwrap_or("current")
                        ),
                        "turn",
                        "Turn started",
                        None,
                        turn.get("status")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                        None,
                    )?;
                }
            }
            "turn/completed" => {
                if let Some(turn) = params.get("turn") {
                    self.apply_turn(turn)?;
                    let status = turn
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("completed");
                    self.upsert_runtime_event(
                        format!(
                            "turn:{}",
                            turn.get("id").and_then(Value::as_str).unwrap_or("current")
                        ),
                        "turn",
                        "Turn completed",
                        None,
                        Some(status.to_string()),
                        None,
                    )?;
                    self.append_text_line(format!("[turn] {}", status))?;
                }
            }
            "item/started" => {
                if let Some(item) = params.get("item") {
                    self.apply_started_item(item)?;
                }
            }
            "item/agentMessage/delta" => {
                let item_id = params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .unwrap_or("agent")
                    .to_string();
                let delta = params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if delta.is_empty() {
                    return Ok(());
                }
                let header = self.mark_item_started(&item_id, "[assistant]\n")?;
                if header {
                    self.append_output(b"\n")?;
                }
                self.append_output(delta.as_bytes())?;
                let mut preview = None;
                self.mutate_state(|state| {
                    let current = state
                        .active_agent_messages
                        .entry(item_id.clone())
                        .or_default();
                    current.push_str(delta);
                    let context = state.metadata.context.get_or_insert_with(Default::default);
                    let rendered = truncate_line(current, 240);
                    context.live_message = Some(rendered.clone());
                    context.last_activity = Some("assistant streaming".to_string());
                    preview = Some(rendered);
                })?;
                self.upsert_runtime_event(
                    format!("item:{item_id}"),
                    "assistant",
                    "Assistant streaming",
                    preview,
                    Some("inProgress".to_string()),
                    Some("agent0".to_string()),
                )?;
            }
            "item/commandExecution/outputDelta" => {
                let item_id = params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .unwrap_or("command")
                    .to_string();
                let delta = params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if delta.is_empty() {
                    return Ok(());
                }
                let _ = self.mark_item_started(&item_id, "[command]\n")?;
                self.append_output(delta.as_bytes())?;
                self.mutate_state(|state| {
                    let current = state
                        .active_command_outputs
                        .entry(item_id.clone())
                        .or_default();
                    current.push_str(delta);
                    let context = state.metadata.context.get_or_insert_with(Default::default);
                    context.last_activity = Some("command output".to_string());
                    upsert_recent_event(
                        &mut context.recent_events,
                        RuntimeFeedEntry {
                            id: format!("item:{item_id}"),
                            kind: "command".to_string(),
                            title: "Command output".to_string(),
                            timestamp_epoch_ms: now_epoch_ms(),
                            actor: Some("agent0".to_string()),
                            detail: Some(truncate_line(current, 220)),
                            status: Some("inProgress".to_string()),
                        },
                    );
                })?;
            }
            "item/completed" => {
                if let Some(item) = params.get("item") {
                    self.apply_completed_item(item)?;
                }
            }
            _ => {}
        }

        Ok(())
    }

    fn mark_item_started(&self, item_id: &str, header: &str) -> anyhow::Result<bool> {
        let should_write = {
            let mut state = self.state.lock().unwrap();
            if state.seen_log_items.insert(item_id.to_string()) {
                true
            } else {
                false
            }
        };
        if should_write {
            self.append_text_line(header)?;
        }
        Ok(should_write)
    }

    fn upsert_runtime_event(
        &self,
        id: impl Into<String>,
        kind: impl Into<String>,
        title: impl Into<String>,
        detail: Option<String>,
        status: Option<String>,
        actor: Option<String>,
    ) -> anyhow::Result<()> {
        let event = RuntimeFeedEntry {
            id: id.into(),
            kind: kind.into(),
            title: title.into(),
            timestamp_epoch_ms: now_epoch_ms(),
            actor,
            detail,
            status,
        };
        self.mutate_state(|state| {
            let context = state.metadata.context.get_or_insert_with(Default::default);
            upsert_recent_event(&mut context.recent_events, event.clone());
        })
    }

    fn apply_started_item(&self, item: &Value) -> anyhow::Result<()> {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        match item_type {
            "commandExecution" => {
                let item_id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let command = item
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("command");
                self.upsert_runtime_event(
                    format!("item:{item_id}"),
                    "command",
                    "Command started",
                    Some(truncate_line(command, 160)),
                    Some("inProgress".to_string()),
                    Some("agent0".to_string()),
                )?;
            }
            "collabAgentToolCall" => {
                self.mutate_state(|state| {
                    let context = state.metadata.context.get_or_insert_with(Default::default);
                    apply_subagent_item(context, item);
                    upsert_recent_event(
                        &mut context.recent_events,
                        build_subagent_event(item, "Subagent activity"),
                    );
                    context.last_activity = Some(describe_subagent_activity(item));
                })?;
            }
            _ => {}
        }
        Ok(())
    }

    fn apply_thread(&self, thread: &Value) -> anyhow::Result<()> {
        let thread_id = thread_id_from_value(thread);
        let transcript_path = thread
            .get("path")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let status = thread
            .get("status")
            .and_then(thread_status_from_value)
            .unwrap_or_else(|| "idle".to_string());
        let cwd = thread
            .get("cwd")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        self.mutate_state(|state| {
            if let Some(path) = cwd {
                state.metadata.working_directory = Some(path);
            }
            let context = state.metadata.context.get_or_insert_with(Default::default);
            if let Some(id) = thread_id.clone() {
                context.codex_session_id = Some(id.clone());
                context.thread_id = Some(id);
            }
            if let Some(path) = transcript_path.clone() {
                context.transcript_path = Some(path);
            }
            context.thread_status = Some(status.clone());
            context.last_activity = Some(format!("thread {}", status));
        })
    }

    fn apply_turn(&self, turn: &Value) -> anyhow::Result<()> {
        let turn_id = turn
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let turn_status = turn
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("inProgress")
            .to_string();

        self.mutate_state(|state| {
            let context = state.metadata.context.get_or_insert_with(Default::default);
            context.turn_id = turn_id.clone();
            context.turn_status = Some(turn_status.clone());
            context.last_activity = Some(format!("turn {}", turn_status));
        })
    }

    fn apply_completed_item(&self, item: &Value) -> anyhow::Result<()> {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        match item_type {
            "agentMessage" => {
                let item_id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let text = item.get("text").and_then(Value::as_str).unwrap_or_default();
                self.mutate_state(|state| {
                    state.active_agent_messages.remove(&item_id);
                    let context = state.metadata.context.get_or_insert_with(Default::default);
                    if !text.is_empty() {
                        context.live_message = Some(truncate_line(text, 240));
                    }
                    context.last_activity = Some("assistant message completed".to_string());
                    upsert_recent_event(
                        &mut context.recent_events,
                        RuntimeFeedEntry {
                            id: format!("item:{item_id}"),
                            kind: "assistant".to_string(),
                            title: "Assistant response".to_string(),
                            timestamp_epoch_ms: now_epoch_ms(),
                            actor: Some("agent0".to_string()),
                            detail: (!text.is_empty()).then(|| truncate_line(text, 260)),
                            status: Some("completed".to_string()),
                        },
                    );
                })?;
                if !text.is_empty() {
                    self.append_output(b"\n")?;
                }
            }
            "commandExecution" => {
                let item_id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let command = item
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("command");
                let status = item
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("completed");
                self.mutate_state(|state| {
                    state.active_command_outputs.remove(&item_id);
                    let context = state.metadata.context.get_or_insert_with(Default::default);
                    context.last_activity =
                        Some(format!("{} ({})", truncate_line(command, 80), status));
                    upsert_recent_event(
                        &mut context.recent_events,
                        RuntimeFeedEntry {
                            id: format!("item:{item_id}"),
                            kind: "command".to_string(),
                            title: "Command completed".to_string(),
                            timestamp_epoch_ms: now_epoch_ms(),
                            actor: Some("agent0".to_string()),
                            detail: Some(truncate_line(command, 200)),
                            status: Some(status.to_string()),
                        },
                    );
                })?;
                self.append_text_line(format!("\n[command completed] {} ({})", command, status))?;
            }
            "plan" => {
                let item_id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("plan")
                    .to_string();
                let text = item.get("text").and_then(Value::as_str).unwrap_or_default();
                self.mutate_state(|state| {
                    let context = state.metadata.context.get_or_insert_with(Default::default);
                    context.last_activity = Some("plan updated".to_string());
                    upsert_recent_event(
                        &mut context.recent_events,
                        RuntimeFeedEntry {
                            id: format!("item:{item_id}"),
                            kind: "plan".to_string(),
                            title: "Plan updated".to_string(),
                            timestamp_epoch_ms: now_epoch_ms(),
                            actor: Some("agent0".to_string()),
                            detail: (!text.is_empty()).then(|| truncate_line(text, 220)),
                            status: Some("completed".to_string()),
                        },
                    );
                })?;
                if !text.is_empty() {
                    self.append_text_line(format!("[plan] {}", truncate_line(text, 160)))?;
                }
            }
            "collabAgentToolCall" => {
                self.mutate_state(|state| {
                    let context = state.metadata.context.get_or_insert_with(Default::default);
                    apply_subagent_item(context, item);
                    upsert_recent_event(
                        &mut context.recent_events,
                        build_subagent_event(item, "Subagent activity"),
                    );
                    context.last_activity = Some(describe_subagent_activity(item));
                })?;
            }
            _ => {}
        }
        Ok(())
    }

    fn record_error(&self, message: &str) -> anyhow::Result<()> {
        self.mutate_state(|state| {
            let context = state.metadata.context.get_or_insert_with(Default::default);
            context.last_error = Some(message.to_string());
            context.last_activity = Some("error".to_string());
            upsert_recent_event(
                &mut context.recent_events,
                RuntimeFeedEntry {
                    id: format!("error:{}", now_epoch_ms()),
                    kind: "error".to_string(),
                    title: "Runtime error".to_string(),
                    timestamp_epoch_ms: now_epoch_ms(),
                    actor: Some("jarvisctl".to_string()),
                    detail: Some(truncate_line(message, 260)),
                    status: Some("failed".to_string()),
                },
            );
        })?;
        self.append_text_line(format!("[error] {}", message))
    }

    fn initialize_and_start(&self, manifest: &CodexAppLaunchManifest) -> anyhow::Result<()> {
        let _ = self.call(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "jarvisctl",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {
                    "experimentalApi": false,
                }
            }),
        )?;
        self.notify("initialized", json!({}))?;

        let thread_response = if let Some(session_id) = manifest.resume_session_id.as_deref() {
            self.call(
                "thread/resume",
                json!({
                    "threadId": session_id,
                    "cwd": manifest.working_directory,
                }),
            )?
        } else {
            self.call(
                "thread/start",
                json!({
                    "cwd": manifest.working_directory,
                }),
            )?
        };

        if let Some(thread) = thread_response.get("thread") {
            self.apply_thread(thread)?;
        }

        let thread_id = {
            let metadata = self.metadata();
            metadata
                .context
                .as_ref()
                .and_then(|context| context.thread_id.clone())
                .or_else(|| {
                    metadata
                        .context
                        .as_ref()
                        .and_then(|context| context.codex_session_id.clone())
                })
                .ok_or_else(|| anyhow!("thread/start did not return a thread id"))?
        };

        let turn_response = self.call(
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": build_user_inputs(&manifest.startup_prompt, &manifest.images),
                "cwd": manifest.working_directory,
            }),
        )?;
        if let Some(turn) = turn_response.get("turn") {
            self.apply_turn(turn)?;
        }
        self.append_text_line(format!(
            "[session] {} {}",
            if manifest.resume_session_id.is_some() {
                "resumed"
            } else {
                "started"
            },
            thread_id
        ))?;
        Ok(())
    }

    fn send_operator_message(&self, text: &str) -> anyhow::Result<()> {
        let (thread_id, turn_id, active) = {
            let metadata = self.metadata();
            let context = metadata
                .context
                .as_ref()
                .ok_or_else(|| anyhow!("codex app session has no thread context"))?;
            (
                context
                    .thread_id
                    .clone()
                    .or_else(|| context.codex_session_id.clone())
                    .ok_or_else(|| anyhow!("codex app session has no thread id"))?,
                context.turn_id.clone(),
                context.turn_status.as_deref() == Some("inProgress"),
            )
        };

        let input = build_text_input(text);
        if active {
            let expected_turn_id =
                turn_id.ok_or_else(|| anyhow!("codex app session has no active turn id"))?;
            let response = self.call(
                "turn/steer",
                json!({
                    "threadId": thread_id,
                    "expectedTurnId": expected_turn_id,
                    "input": input,
                }),
            )?;
            if let Some(turn) = response.get("turn") {
                self.apply_turn(turn)?;
            }
            self.append_text_line(format!("[operator] steer: {}", text.trim()))?;
            self.upsert_runtime_event(
                format!("operator:{}", now_epoch_ms()),
                "operator",
                "Operator steer",
                Some(truncate_line(text.trim(), 220)),
                Some("inProgress".to_string()),
                Some("operator".to_string()),
            )?;
        } else {
            let response = self.call(
                "turn/start",
                json!({
                    "threadId": thread_id,
                    "input": input,
                }),
            )?;
            if let Some(turn) = response.get("turn") {
                self.apply_turn(turn)?;
            }
            self.append_text_line(format!("[operator] new turn: {}", text.trim()))?;
            self.upsert_runtime_event(
                format!("operator:{}", now_epoch_ms()),
                "operator",
                "Operator follow-up",
                Some(truncate_line(text.trim(), 220)),
                Some("queued".to_string()),
                Some("operator".to_string()),
            )?;
        }
        Ok(())
    }

    fn interrupt_turn(&self) -> anyhow::Result<()> {
        let (thread_id, turn_id, active) = {
            let metadata = self.metadata();
            let Some(context) = metadata.context.as_ref() else {
                bail!("codex app session has no thread context");
            };
            (
                context
                    .thread_id
                    .clone()
                    .or_else(|| context.codex_session_id.clone())
                    .ok_or_else(|| anyhow!("codex app session has no thread id"))?,
                context.turn_id.clone(),
                context.turn_status.as_deref() == Some("inProgress"),
            )
        };

        ensure!(active, "codex app session has no active turn to interrupt");
        let turn_id = turn_id.ok_or_else(|| anyhow!("codex app session has no active turn id"))?;
        let response = self.call(
            "turn/interrupt",
            json!({
                "threadId": thread_id,
                "turnId": turn_id,
            }),
        )?;
        if let Some(turn) = response.get("turn") {
            self.apply_turn(turn)?;
        }
        self.append_text_line("[operator] interrupt requested")?;
        self.upsert_runtime_event(
            format!("interrupt:{}", now_epoch_ms()),
            "operator",
            "Interrupt requested",
            None,
            Some("requested".to_string()),
            Some("operator".to_string()),
        )?;
        Ok(())
    }

    fn shutdown(&self) {
        if self.shutdown_requested.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = {
            let mut child = self.child.lock().unwrap();
            child.kill()
        };
    }
}

pub fn spawn_codex_app_session(
    manifest: CodexAppLaunchManifest,
) -> anyhow::Result<NativeSessionMetadata> {
    ensure!(
        !manifest.namespace.trim().is_empty(),
        "namespace must not be empty for codex app runtime"
    );

    let manifest_dir = codex_app_root()?.join(MANIFEST_DIR_NAME);
    fs::create_dir_all(&manifest_dir)
        .with_context(|| format!("failed to create '{}'", manifest_dir.display()))?;
    let manifest_path = manifest_dir.join(format!(
        "{}-{}.json",
        sanitize_namespace(&manifest.namespace),
        manifest.created_at_epoch_ms
    ));
    let manifest_raw =
        serde_json::to_string_pretty(&manifest).context("failed to encode codex app manifest")?;
    fs::write(&manifest_path, manifest_raw)
        .with_context(|| format!("failed to write '{}'", manifest_path.display()))?;

    let current_exe = env::current_exe().context("failed to resolve current jarvisctl path")?;
    let mut command = Command::new("setsid");
    command
        .arg(current_exe)
        .arg("codex-app-session-serve")
        .arg("--manifest")
        .arg(&manifest_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
        .spawn()
        .context("failed to spawn codex app session server")?;

    wait_for_codex_app_session(&manifest.namespace)
}

pub fn serve_codex_app_session(manifest_path: PathBuf) -> anyhow::Result<()> {
    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read '{}'", manifest_path.display()))?;
    let manifest: CodexAppLaunchManifest =
        serde_json::from_str(&raw).context("failed to parse codex app manifest")?;
    let _ = fs::remove_file(&manifest_path);

    let session_dir = codex_app_root()?
        .join(SESSION_DIR_NAME)
        .join(sanitize_namespace(&manifest.namespace));
    fs::create_dir_all(&session_dir)
        .with_context(|| format!("failed to create '{}'", session_dir.display()))?;
    let socket_path = session_dir.join(SOCKET_FILE_NAME);
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    let command_parts = shell_words::split(&manifest.shell_command)
        .with_context(|| format!("failed to parse shell command '{}'", manifest.shell_command))?;
    ensure!(
        !command_parts.is_empty(),
        "codex app shell command must not be empty"
    );
    let mut command = Command::new(&command_parts[0]);
    command.args(&command_parts[1..]);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    if let Some(dir) = manifest.working_directory.as_deref() {
        command.current_dir(dir);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn '{}'", manifest.shell_command))?;
    let child_pid = child.id();
    let stdin = child
        .stdin
        .take()
        .context("failed to capture app-server stdin")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to capture app-server stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("failed to capture app-server stderr")?;

    let events_path = session_dir.join(EVENTS_FILE_NAME);
    let log_file = File::options()
        .create(true)
        .append(true)
        .open(&events_path)
        .with_context(|| format!("failed to open '{}'", events_path.display()))?;

    let metadata = NativeSessionMetadata {
        namespace: manifest.namespace.clone(),
        backend: "codex-app".to_string(),
        created_at_epoch_ms: manifest.created_at_epoch_ms,
        working_directory: manifest.working_directory.clone(),
        shell_command: manifest.shell_command.clone(),
        context: Some(RuntimeContextMetadata {
            event_log_path: Some(events_path.display().to_string()),
            recent_events: vec![RuntimeFeedEntry {
                id: format!("session:{}:launch", manifest.namespace),
                kind: "session".to_string(),
                title: "Session launching".to_string(),
                timestamp_epoch_ms: now_epoch_ms(),
                actor: Some("jarvisctl".to_string()),
                detail: Some(truncate_line(&manifest.startup_prompt, 220)),
                status: Some("launching".to_string()),
            }],
            ..manifest.context.clone()
        }),
        agents: vec![NativeAgentMetadata {
            name: "agent0".to_string(),
            pid: child_pid,
            running: true,
        }],
    };
    let session = Arc::new(CodexAppSession {
        namespace: manifest.namespace.clone(),
        session_dir: session_dir.clone(),
        state: Mutex::new(AppSessionState {
            metadata,
            seen_log_items: HashSet::new(),
            active_agent_messages: BTreeMap::new(),
            active_command_outputs: BTreeMap::new(),
        }),
        writer: Mutex::new(stdin),
        child: Mutex::new(child),
        pending: Mutex::new(HashMap::new()),
        next_request_id: AtomicU64::new(1),
        log: Mutex::new(VecDeque::new()),
        log_file: Mutex::new(log_file),
        subscribers: Mutex::new(Vec::new()),
        shutdown_requested: AtomicBool::new(false),
    });
    session.mutate_state(|_| {})?;
    session.append_text_line(format!("[session] launching {}", manifest.namespace))?;

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind '{}'", socket_path.display()))?;
    listener
        .set_nonblocking(true)
        .context("failed to configure codex app listener")?;

    let startup_session = Arc::clone(&session);
    let startup_manifest = manifest;
    thread::spawn(move || {
        if let Err(error) = startup_session.initialize_and_start(&startup_manifest) {
            let _ = startup_session.record_error(&error.to_string());
        }
    });

    let stdout_session = Arc::clone(&session);
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            let read = match reader.read_line(&mut line) {
                Ok(read) => read,
                Err(error) => {
                    let _ = stdout_session.record_error(&format!("stdout read failed: {}", error));
                    break;
                }
            };
            if read == 0 {
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(trimmed) {
                Ok(value) => value,
                Err(error) => {
                    let _ = stdout_session.record_error(&format!(
                        "failed to decode app-server message: {} ({})",
                        error, trimmed
                    ));
                    continue;
                }
            };
            if value.get("id").is_some() {
                let _ = stdout_session.handle_response(value);
                continue;
            }
            let method = value
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let params = value.get("params").unwrap_or(&Value::Null);
            let _ = stdout_session.handle_notification(method, params);
        }
        let _ = stdout_session.set_agent_running(false);
        let _ = stdout_session.append_text_line("[session] app-server stdout closed");
    });

    let stderr_session = Arc::clone(&session);
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            let read = match reader.read_line(&mut line) {
                Ok(read) => read,
                Err(_) => break,
            };
            if read == 0 {
                break;
            }
            let rendered = line.trim_end();
            if rendered.is_empty() {
                continue;
            }
            let _ = stderr_session.append_text_line(format!("[stderr] {}", rendered));
        }
    });

    let wait_session = Arc::clone(&session);
    thread::spawn(move || {
        loop {
            if wait_session.shutdown_requested.load(Ordering::SeqCst) {
                break;
            }
            let status = {
                let mut child = wait_session.child.lock().unwrap();
                child.try_wait().ok().flatten()
            };
            if let Some(status) = status {
                let _ = wait_session.set_agent_running(false);
                let _ = wait_session.append_text_line(format!("[session] child exited {}", status));
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    });

    while !session.shutdown_requested.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let session = Arc::clone(&session);
                thread::spawn(move || {
                    let _ = handle_client(stream, session);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to accept '{}'", socket_path.display()));
            }
        }
    }

    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(session.metadata_path());
    let _ = fs::remove_file(events_path);
    let _ = fs::remove_dir(&session_dir);
    Ok(())
}

pub fn collect_codex_app_sessions() -> anyhow::Result<Vec<NativeSessionMetadata>> {
    let sessions_dir = codex_app_root()?.join(SESSION_DIR_NAME);
    if !sessions_dir.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    for entry in fs::read_dir(&sessions_dir)
        .with_context(|| format!("failed to read '{}'", sessions_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        match codex_app_session_metadata(&name)? {
            Some(metadata) => sessions.push(metadata),
            None => {}
        }
    }

    sessions.sort_by(|left, right| left.namespace.cmp(&right.namespace));
    Ok(sessions)
}

pub fn codex_app_session_metadata(
    namespace: &str,
) -> anyhow::Result<Option<NativeSessionMetadata>> {
    let socket_path = socket_path_for(namespace)?;
    if !socket_path.exists() {
        return read_session_metadata_file(namespace);
    }

    let stream = match UnixStream::connect(&socket_path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            return read_session_metadata_file(namespace);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to connect '{}'", socket_path.display()));
        }
    };

    let mut writer = stream
        .try_clone()
        .context("failed to clone codex app metadata stream")?;
    send_message(&mut writer, &ClientMessage::Metadata)?;
    let mut reader = BufReader::new(stream);
    match read_server_message(&mut reader)? {
        ServerMessage::Metadata(metadata) => Ok(Some(metadata)),
        ServerMessage::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!(
            "unexpected metadata response for codex app session '{}': {:?}",
            namespace,
            other
        )),
    }
}

fn read_session_metadata_file(namespace: &str) -> anyhow::Result<Option<NativeSessionMetadata>> {
    let path = session_dir_for(namespace)?.join(METADATA_FILE_NAME);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    let metadata = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse '{}'", path.display()))?;
    Ok(Some(metadata))
}

pub fn delete_codex_app_session(namespace: &str) -> anyhow::Result<()> {
    match request(namespace, &ClientMessage::KillSession) {
        Ok(ServerMessage::Ok) => wait_for_codex_app_shutdown(namespace),
        Ok(ServerMessage::Error { message }) => Err(anyhow!(message)),
        Ok(other) => Err(anyhow!(
            "unexpected response while deleting codex app session '{}': {:?}",
            namespace,
            other
        )),
        Err(error) => wait_for_codex_app_shutdown(namespace)
            .or(Err(error))
            .context("failed to delete codex app session cleanly"),
    }
}

pub fn tell_codex_app(namespace: &str, contents: &str) -> anyhow::Result<()> {
    match request(
        namespace,
        &ClientMessage::SendText {
            text: contents.to_string(),
        },
    )? {
        ServerMessage::Ok => Ok(()),
        ServerMessage::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!(
            "unexpected response while telling codex app session '{}': {:?}",
            namespace,
            other
        )),
    }
}

pub fn interrupt_codex_app(namespace: &str) -> anyhow::Result<()> {
    match request(namespace, &ClientMessage::Interrupt)? {
        ServerMessage::Ok => Ok(()),
        ServerMessage::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!(
            "unexpected response while interrupting codex app session '{}': {:?}",
            namespace,
            other
        )),
    }
}

pub fn attach_codex_app(namespace: &str) -> anyhow::Result<()> {
    let socket_path = socket_path_for(namespace)?;
    let mut stream = UnixStream::connect(&socket_path)
        .with_context(|| format!("failed to connect '{}'", socket_path.display()))?;
    send_message(
        &mut stream,
        &ClientMessage::Attach {
            agent: "agent0".to_string(),
        },
    )?;

    let output = Arc::new(Mutex::new(Vec::<String>::new()));
    let output_thread = Arc::clone(&output);
    let namespace_owned = namespace.to_string();
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        loop {
            let message = match read_server_message(&mut reader) {
                Ok(message) => message,
                Err(error) => {
                    output_thread
                        .lock()
                        .unwrap()
                        .push(format!("[attach] {}", error));
                    break;
                }
            };
            match message {
                ServerMessage::Attached { .. } => {}
                ServerMessage::Output { data_base64 } => {
                    let bytes = match BASE64.decode(data_base64) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            output_thread
                                .lock()
                                .unwrap()
                                .push(format!("[attach] decode failed: {}", error));
                            continue;
                        }
                    };
                    let text = String::from_utf8_lossy(&bytes);
                    for line in text.lines() {
                        output_thread.lock().unwrap().push(line.to_string());
                    }
                }
                ServerMessage::Exited { .. } => {
                    output_thread
                        .lock()
                        .unwrap()
                        .push(format!("[attach] {} closed", namespace_owned));
                    break;
                }
                ServerMessage::Error { message } => {
                    output_thread
                        .lock()
                        .unwrap()
                        .push(format!("[attach] {}", message));
                    break;
                }
                ServerMessage::Ok | ServerMessage::Metadata(..) => {}
            }
        }
    });

    view_agent(&format!("{}:agent0", namespace), output)
}

fn handle_client(stream: UnixStream, session: Arc<CodexAppSession>) -> anyhow::Result<()> {
    let mut writer = stream
        .try_clone()
        .context("failed to clone codex app client stream")?;
    let mut reader = BufReader::new(stream);
    let request = read_client_message(&mut reader)?;

    match request {
        ClientMessage::Metadata => {
            send_server_message(&mut writer, &ServerMessage::Metadata(session.metadata()))?;
        }
        ClientMessage::Attach { .. } => {
            let (backlog, rx) = session.subscribe();
            send_server_message(
                &mut writer,
                &ServerMessage::Attached {
                    namespace: session.namespace.clone(),
                    agent: "agent0".to_string(),
                },
            )?;
            if !backlog.is_empty() {
                send_output(&mut writer, &backlog)?;
            }
            while let Ok(chunk) = rx.recv() {
                if chunk.is_empty() {
                    continue;
                }
                if send_output(&mut writer, &chunk).is_err() {
                    break;
                }
            }
            let _ = send_server_message(
                &mut writer,
                &ServerMessage::Exited {
                    agent: "agent0".to_string(),
                },
            );
        }
        ClientMessage::SendText { text } => {
            session.send_operator_message(&text)?;
            send_server_message(&mut writer, &ServerMessage::Ok)?;
        }
        ClientMessage::Interrupt => {
            session.interrupt_turn()?;
            send_server_message(&mut writer, &ServerMessage::Ok)?;
        }
        ClientMessage::KillSession => {
            send_server_message(&mut writer, &ServerMessage::Ok)?;
            session.shutdown();
        }
    }

    Ok(())
}

fn request(namespace: &str, message: &ClientMessage) -> anyhow::Result<ServerMessage> {
    let socket_path = socket_path_for(namespace)?;
    let mut stream = UnixStream::connect(&socket_path)
        .with_context(|| format!("failed to connect '{}'", socket_path.display()))?;
    send_message(&mut stream, message)?;
    let mut reader = BufReader::new(stream);
    read_server_message(&mut reader)
}

fn request_metadata_direct(namespace: &str) -> anyhow::Result<NativeSessionMetadata> {
    match request(namespace, &ClientMessage::Metadata)? {
        ServerMessage::Metadata(metadata) => Ok(metadata),
        ServerMessage::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!(
            "unexpected metadata response while waiting for codex app session '{}': {:?}",
            namespace,
            other
        )),
    }
}

fn send_message(stream: &mut UnixStream, message: &ClientMessage) -> anyhow::Result<()> {
    let raw =
        serde_json::to_string(message).context("failed to encode codex app client message")?;
    stream
        .write_all(raw.as_bytes())
        .context("failed to write codex app client message")?;
    stream
        .write_all(b"\n")
        .context("failed to terminate codex app client message")?;
    stream
        .flush()
        .context("failed to flush codex app client message")
}

fn send_server_message(stream: &mut UnixStream, message: &ServerMessage) -> anyhow::Result<()> {
    let raw =
        serde_json::to_string(message).context("failed to encode codex app server message")?;
    stream
        .write_all(raw.as_bytes())
        .context("failed to write codex app server message")?;
    stream
        .write_all(b"\n")
        .context("failed to terminate codex app server message")?;
    stream
        .flush()
        .context("failed to flush codex app server message")
}

fn send_output(stream: &mut UnixStream, bytes: &[u8]) -> anyhow::Result<()> {
    send_server_message(
        stream,
        &ServerMessage::Output {
            data_base64: BASE64.encode(bytes),
        },
    )
}

fn read_client_message(reader: &mut impl BufRead) -> anyhow::Result<ClientMessage> {
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .context("failed to read codex app client message")?;
        if read == 0 {
            bail!("codex app client connection closed");
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        return serde_json::from_str(trimmed).with_context(|| {
            format!(
                "failed to decode codex app client message from '{}'",
                trimmed
            )
        });
    }
}

fn read_server_message(reader: &mut impl BufRead) -> anyhow::Result<ServerMessage> {
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .context("failed to read codex app server message")?;
        if read == 0 {
            bail!("codex app server connection closed");
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        return serde_json::from_str(trimmed).with_context(|| {
            format!(
                "failed to decode codex app server message from '{}'",
                trimmed
            )
        });
    }
}

fn wait_for_codex_app_session(namespace: &str) -> anyhow::Result<NativeSessionMetadata> {
    let deadline = SystemTime::now() + Duration::from_secs(10);
    let mut successful_probes = 0_u8;
    loop {
        if let Ok(metadata) = request_metadata_direct(namespace) {
            let ready = metadata
                .context
                .as_ref()
                .and_then(|context| context.codex_session_id.as_deref())
                .is_some();
            if ready {
                successful_probes += 1;
                if successful_probes >= 3 {
                    return Ok(metadata);
                }
                thread::sleep(Duration::from_millis(150));
                continue;
            }
        }
        successful_probes = 0;
        if SystemTime::now() >= deadline {
            bail!("codex app session '{}' did not become ready", namespace);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_codex_app_shutdown(namespace: &str) -> anyhow::Result<()> {
    let deadline = SystemTime::now() + Duration::from_secs(5);
    let socket_path = socket_path_for(namespace)?;
    loop {
        if !socket_path.exists() {
            return Ok(());
        }
        match UnixStream::connect(&socket_path) {
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                cleanup_stale_session(namespace)?;
                return Ok(());
            }
            Err(_) => {}
        }
        if SystemTime::now() >= deadline {
            bail!(
                "codex app session '{}' did not shut down cleanly",
                namespace
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn format_rpc_error(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown app-server error");
    if let Some(code) = error.get("code").and_then(Value::as_i64) {
        format!("{} ({})", message, code)
    } else {
        message.to_string()
    }
}

fn thread_status_from_value(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => object
            .get("type")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        Value::String(raw) => Some(raw.clone()),
        _ => None,
    }
}

fn thread_id_from_value(thread: &Value) -> Option<String> {
    thread
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn build_user_inputs(prompt: &str, images: &[String]) -> Vec<Value> {
    let mut inputs = Vec::new();
    for image in images {
        inputs.push(json!({
            "type": "localImage",
            "path": image,
        }));
    }
    inputs.extend(build_text_input(prompt));
    inputs
}

fn build_text_input(text: &str) -> Vec<Value> {
    vec![json!({
        "type": "text",
        "text": text,
        "text_elements": [],
    })]
}

fn truncate_line(raw: &str, limit: usize) -> String {
    let normalized = raw.replace('\n', " ").trim().to_string();
    if normalized.chars().count() <= limit {
        return normalized;
    }
    let mut rendered = normalized
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    rendered.push('…');
    rendered
}

fn now_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn upsert_recent_event(events: &mut Vec<RuntimeFeedEntry>, event: RuntimeFeedEntry) {
    if let Some(index) = events.iter().position(|existing| existing.id == event.id) {
        events.remove(index);
    }
    events.push(event);
    let overflow = events.len().saturating_sub(FEED_LIMIT);
    if overflow > 0 {
        events.drain(0..overflow);
    }
}

fn apply_subagent_item(context: &mut RuntimeContextMetadata, item: &Value) {
    let sender_thread_id = item
        .get("senderThreadId")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let tool = item
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("spawnAgent")
        .to_string();
    let call_status = item
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("inProgress")
        .to_string();
    let model = item
        .get("model")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let reasoning_effort = item.get("reasoningEffort").and_then(reasoning_effort_value);
    let prompt_preview = item
        .get("prompt")
        .and_then(Value::as_str)
        .map(|value| truncate_line(value, 200));
    let receiver_ids = item
        .get("receiverThreadIds")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let agent_states = item.get("agentsStates").and_then(Value::as_object);

    for receiver_thread_id in receiver_ids {
        let (status, latest_message) =
            match agent_states.and_then(|states| states.get(&receiver_thread_id)) {
                Some(Value::Object(state)) => (
                    state
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or_else(|| map_tool_call_status_to_subagent_status(&call_status))
                        .to_string(),
                    state
                        .get("message")
                        .and_then(Value::as_str)
                        .map(|value| truncate_line(value, 180)),
                ),
                _ => (
                    map_tool_call_status_to_subagent_status(&call_status).to_string(),
                    None,
                ),
            };
        upsert_subagent(
            &mut context.subagents,
            RuntimeSubagentMetadata {
                thread_id: receiver_thread_id,
                tool: tool.clone(),
                status,
                updated_at_epoch_ms: now_epoch_ms(),
                parent_thread_id: sender_thread_id.clone(),
                model: model.clone(),
                reasoning_effort: reasoning_effort.clone(),
                prompt_preview: prompt_preview.clone(),
                latest_message,
            },
        );
    }
}

fn upsert_subagent(
    subagents: &mut Vec<RuntimeSubagentMetadata>,
    subagent: RuntimeSubagentMetadata,
) {
    if let Some(index) = subagents
        .iter()
        .position(|existing| existing.thread_id == subagent.thread_id)
    {
        subagents.remove(index);
    }
    subagents.push(subagent);
    subagents.sort_by(|left, right| {
        left.updated_at_epoch_ms
            .cmp(&right.updated_at_epoch_ms)
            .then_with(|| left.thread_id.cmp(&right.thread_id))
    });
    let overflow = subagents.len().saturating_sub(SUBAGENT_LIMIT);
    if overflow > 0 {
        subagents.drain(0..overflow);
    }
}

fn build_subagent_event(item: &Value, default_title: &str) -> RuntimeFeedEntry {
    let tool = item
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("spawnAgent");
    let status = item
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("inProgress")
        .to_string();
    let receiver_count = item
        .get("receiverThreadIds")
        .and_then(Value::as_array)
        .map(|values| values.len())
        .unwrap_or(0);
    let detail = item
        .get("prompt")
        .and_then(Value::as_str)
        .map(|value| truncate_line(value, 220))
        .or_else(|| build_subagent_state_summary(item));
    RuntimeFeedEntry {
        id: format!(
            "item:{}",
            item.get("id").and_then(Value::as_str).unwrap_or("subagent")
        ),
        kind: "subagent".to_string(),
        title: subagent_title(tool, receiver_count, default_title),
        timestamp_epoch_ms: now_epoch_ms(),
        actor: item
            .get("senderThreadId")
            .and_then(Value::as_str)
            .map(short_thread_id),
        detail,
        status: Some(status),
    }
}

fn build_subagent_state_summary(item: &Value) -> Option<String> {
    let states = item.get("agentsStates").and_then(Value::as_object)?;
    let mut entries = states
        .iter()
        .filter_map(|(thread_id, state)| {
            let status = state.get("status")?.as_str()?;
            let message = state
                .get("message")
                .and_then(Value::as_str)
                .map(|value| truncate_line(value, 72));
            Some(match message {
                Some(message) => format!("{} {} · {}", short_thread_id(thread_id), status, message),
                None => format!("{} {}", short_thread_id(thread_id), status),
            })
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return None;
    }
    entries.sort();
    Some(entries.join(" | "))
}

fn subagent_title(tool: &str, receiver_count: usize, default_title: &str) -> String {
    let count_suffix = if receiver_count > 0 {
        format!(" ({receiver_count})")
    } else {
        String::new()
    };
    match tool {
        "spawnAgent" => format!("Spawned subagent{count_suffix}"),
        "sendInput" => format!("Updated subagent{count_suffix}"),
        "resumeAgent" => format!("Resumed subagent{count_suffix}"),
        "wait" => format!("Waiting on subagent{count_suffix}"),
        "closeAgent" => format!("Closed subagent{count_suffix}"),
        _ => default_title.to_string(),
    }
}

fn describe_subagent_activity(item: &Value) -> String {
    let tool = item
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("subagent");
    let status = item
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("inProgress");
    format!("{tool} {status}")
}

fn map_tool_call_status_to_subagent_status(status: &str) -> &str {
    match status {
        "completed" => "completed",
        "failed" => "errored",
        _ => "running",
    }
}

fn reasoning_effort_value(value: &Value) -> Option<String> {
    match value {
        Value::String(raw) => Some(raw.clone()),
        Value::Object(object) => object
            .get("type")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        _ => None,
    }
}

fn short_thread_id(raw: &str) -> String {
    let mut pieces = raw.split('-');
    pieces
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or(raw)
        .to_string()
}

fn cleanup_stale_session(namespace: &str) -> anyhow::Result<()> {
    let session_dir = session_dir_for(namespace)?;
    let socket_path = session_dir.join(SOCKET_FILE_NAME);
    let metadata_path = session_dir.join(METADATA_FILE_NAME);
    let events_path = session_dir.join(EVENTS_FILE_NAME);

    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }
    if metadata_path.exists() {
        let _ = fs::remove_file(&metadata_path);
    }
    if events_path.exists() {
        let _ = fs::remove_file(&events_path);
    }
    if session_dir.exists() {
        let _ = fs::remove_dir(&session_dir);
    }
    Ok(())
}

fn codex_app_root() -> anyhow::Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".jarvis").join("codex-app"))
}

fn socket_path_for(namespace: &str) -> anyhow::Result<PathBuf> {
    Ok(session_dir_for(namespace)?.join(SOCKET_FILE_NAME))
}

fn session_dir_for(namespace: &str) -> anyhow::Result<PathBuf> {
    Ok(codex_app_root()?
        .join(SESSION_DIR_NAME)
        .join(sanitize_namespace(namespace)))
}

fn sanitize_namespace(namespace: &str) -> String {
    namespace
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}
