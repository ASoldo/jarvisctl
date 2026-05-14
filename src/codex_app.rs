use crate::native::{
    NativeAgentMetadata, NativeSessionMetadata, RuntimeContextMetadata, RuntimeFeedEntry,
    RuntimeSubagentAction, RuntimeSubagentMetadata,
};
use crate::tui::view_agent;
use anyhow::{Context, anyhow, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
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
const CONTROL_QUEUE_DIR_NAME: &str = ".jarvisctl-control-queue";
const CONTROL_QUEUE_REQUESTS_DIR_NAME: &str = "requests";
const CONTROL_QUEUE_RESPONSES_DIR_NAME: &str = "responses";
const TCP_CONTROL_PORT_ENV: &str = "JARVISCTL_CODEX_APP_TCP_PORT";
const TCP_CONTROL_HOST_ENV: &str = "JARVISCTL_CODEX_APP_TCP_HOST";
const LOG_LIMIT_BYTES: usize = 512 * 1024;
const FEED_LIMIT: usize = 18;
const SUBAGENT_LIMIT: usize = 24;
const SUBAGENT_ACTION_LIMIT: usize = 6;
static CONTROL_QUEUE_REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexAppLaunchManifest {
    pub namespace: String,
    pub working_directory: Option<String>,
    pub shell_command: String,
    pub startup_prompt: String,
    pub images: Vec<String>,
    #[serde(default)]
    pub protocol: CodexAppProtocolConfig,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    pub resume_session_id: Option<String>,
    pub created_at_epoch_ms: u128,
    pub context: RuntimeContextMetadata,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodexAppProtocolConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approvals_reviewer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personality: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_start_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_additional_writable_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environments: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_token_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_features: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_features: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub thread_config: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub turn_config: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, ValueEnum, Default)]
#[serde(rename_all = "snake_case")]
pub enum CodexAppInputMode {
    #[default]
    Auto,
    Steer,
    Queue,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
enum ClientMessage {
    Metadata,
    ReadThread {
        include_turns: bool,
    },
    Attach {
        agent: String,
    },
    SendText {
        text: String,
        mode: CodexAppInputMode,
    },
    Interrupt,
    KillSession,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
enum ServerMessage {
    Ok,
    Error { message: String },
    Metadata(NativeSessionMetadata),
    ThreadHistory(Value),
    Attached { namespace: String, agent: String },
    Output { data_base64: String },
    Exited { agent: String },
}

#[derive(Debug, Serialize, Deserialize)]
struct ControlQueueRequestEnvelope {
    id: String,
    created_at_epoch_ms: u128,
    message: ClientMessage,
}

#[derive(Debug, Serialize, Deserialize)]
struct ControlQueueResponseEnvelope {
    message: ServerMessage,
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
    protocol: CodexAppProtocolConfig,
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
            "thread/goal/updated" => {
                if let Some(goal) = params.get("goal") {
                    self.apply_goal(goal)?;
                }
            }
            "thread/goal/cleared" => {
                self.mutate_state(|state| {
                    let context = state.metadata.context.get_or_insert_with(Default::default);
                    context.goal_objective = None;
                    context.goal_status = None;
                    context.last_activity = Some("goal cleared".to_string());
                })?;
                self.upsert_runtime_event(
                    format!("goal-cleared:{}", now_epoch_ms()),
                    "goal",
                    "Goal cleared",
                    None,
                    Some("cleared".to_string()),
                    Some("codex".to_string()),
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
                    if status == "failed" {
                        let detail = self.infer_failed_turn_detail()?.unwrap_or_else(|| {
                            "Codex turn failed before app-server returned a detailed error"
                                .to_string()
                        });
                        self.record_error(&detail)?;
                    }
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
                    let rendered = truncate_block(current, 640);
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
                            detail: Some(truncate_block(current, 1200)),
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
            "remoteControl/status/changed" => {
                let status = params
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let environment_id = params
                    .get("environmentId")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                self.mutate_state(|state| {
                    let context = state.metadata.context.get_or_insert_with(Default::default);
                    context.remote_control_status = Some(status.clone());
                    context.remote_environment_id = environment_id.clone();
                    context.last_activity = Some(match environment_id.as_deref() {
                        Some(id) => format!("remote-control {status} ({id})"),
                        None => format!("remote-control {status}"),
                    });
                })?;
                self.upsert_runtime_event(
                    "remote-control".to_string(),
                    "remote-control",
                    "Remote control status",
                    environment_id,
                    Some(status),
                    Some("codex".to_string()),
                )?;
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
                self.append_text_line(format!("[branch] {}", subagent_console_line(item)))?;
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

    fn apply_goal(&self, goal: &Value) -> anyhow::Result<()> {
        let objective = goal
            .get("objective")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let status = goal
            .get("status")
            .and_then(thread_status_from_value)
            .or_else(|| {
                goal.get("status")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| "active".to_string());
        self.mutate_state(|state| {
            let context = state.metadata.context.get_or_insert_with(Default::default);
            context.goal_objective = objective.clone();
            context.goal_status = Some(status.clone());
            context.last_activity = Some(format!("goal {}", status));
        })?;
        self.upsert_runtime_event(
            "goal".to_string(),
            "goal",
            "Goal updated",
            objective.map(|value| truncate_block(&value, 2400)),
            Some(status),
            Some("codex".to_string()),
        )
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
                        context.live_message = Some(truncate_block(text, 720));
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
                            detail: (!text.is_empty()).then(|| truncate_block(text, 4096)),
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
                            detail: Some(truncate_line(command, 420)),
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
                            detail: (!text.is_empty()).then(|| truncate_block(text, 2400)),
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
                self.append_text_line(format!("[branch] {}", subagent_console_line(item)))?;
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
                    detail: Some(truncate_block(message, 2400)),
                    status: Some("failed".to_string()),
                },
            );
        })?;
        self.append_text_line(format!("[error] {}", message))
    }

    fn infer_failed_turn_detail(&self) -> anyhow::Result<Option<String>> {
        let transcript_path = {
            let state = self.state.lock().unwrap();
            state
                .metadata
                .context
                .as_ref()
                .and_then(|context| context.transcript_path.clone())
        };
        let Some(transcript_path) = transcript_path else {
            return Ok(None);
        };
        let Ok(file) = File::open(&transcript_path) else {
            return Ok(None);
        };

        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let payload = value.get("payload").unwrap_or(&Value::Null);
            let is_token_count = payload.get("type").and_then(Value::as_str) == Some("token_count");
            if !is_token_count {
                continue;
            }
            let credits = payload
                .get("rate_limits")
                .and_then(|limits| limits.get("credits"));
            let balance = credits
                .and_then(|credits| credits.get("balance"))
                .and_then(Value::as_str);
            let has_credits = credits
                .and_then(|credits| credits.get("has_credits"))
                .and_then(Value::as_bool);
            if balance == Some("0") || has_credits == Some(false) {
                return Ok(Some(
                    "Codex turn failed before model output because quota or credits are unavailable; run /status for details"
                        .to_string(),
                ));
            }
        }

        Ok(None)
    }

    fn clear_last_error(&self) -> anyhow::Result<()> {
        self.mutate_state(|state| {
            if let Some(context) = state.metadata.context.as_mut() {
                context.last_error = None;
            }
        })
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
                    "experimentalApi": true,
                }
            }),
        )?;
        self.notify("initialized", json!({}))?;

        let thread_response = if let Some(session_id) = manifest.resume_session_id.as_deref() {
            self.call(
                "thread/resume",
                build_thread_resume_params(session_id, manifest),
            )?
        } else {
            self.call("thread/start", build_thread_start_params(manifest))?
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

        self.apply_thread_side_effects(&thread_id, manifest)?;

        let turn_response = self.call(
            "turn/start",
            build_turn_start_params(
                &thread_id,
                build_user_inputs(&manifest.startup_prompt, &manifest.images),
                manifest.working_directory.as_deref(),
                &manifest.protocol,
            ),
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
        thread::sleep(Duration::from_millis(750));
        if let Err(error) = self.record_apps_runtime_status() {
            let _ = self.record_error(&format!("failed to inspect apps MCP status: {}", error));
        }
        Ok(())
    }

    fn apply_thread_side_effects(
        &self,
        thread_id: &str,
        manifest: &CodexAppLaunchManifest,
    ) -> anyhow::Result<()> {
        if let Some(goal) = manifest.protocol.goal.as_deref() {
            let mut params = json!({
                "threadId": thread_id,
                "objective": goal,
            });
            if let Some(token_budget) = manifest.protocol.goal_token_budget {
                params["tokenBudget"] = json!(token_budget);
            }
            match self.call("thread/goal/set", params) {
                Ok(response) => {
                    if let Some(goal) = response.get("goal") {
                        self.apply_goal(goal)?;
                    } else {
                        self.mark_requested_goal(goal)?;
                    }
                }
                Err(error) => {
                    self.mark_requested_goal(goal)?;
                    self.upsert_runtime_event(
                        "goal-protocol".to_string(),
                        "goal",
                        "Goal requested",
                        Some(format!(
                            "app-server did not accept thread/goal/set: {}",
                            truncate_line(&error.to_string(), 180)
                        )),
                        Some("requested".to_string()),
                        Some("jarvisctl".to_string()),
                    )?;
                }
            }
        }

        Ok(())
    }

    fn mark_requested_goal(&self, goal: &str) -> anyhow::Result<()> {
        self.mutate_state(|state| {
            let context = state.metadata.context.get_or_insert_with(Default::default);
            context.goal_objective = Some(goal.to_string());
            context.goal_status = Some("requested".to_string());
        })
    }

    fn read_thread(&self, include_turns: bool) -> anyhow::Result<Value> {
        let thread_id = {
            let metadata = self.metadata();
            metadata
                .context
                .as_ref()
                .and_then(|context| {
                    context
                        .thread_id
                        .clone()
                        .or_else(|| context.codex_session_id.clone())
                })
                .ok_or_else(|| anyhow!("codex app session has no thread id"))?
        };

        self.call(
            "thread/read",
            json!({
                "threadId": thread_id,
                "includeTurns": include_turns,
            }),
        )
    }

    fn send_operator_message(&self, text: &str, mode: CodexAppInputMode) -> anyhow::Result<()> {
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
        if active && mode != CodexAppInputMode::Queue {
            let expected_turn_id =
                turn_id.ok_or_else(|| anyhow!("codex app session has no active turn id"))?;
            let response = match self.call(
                "turn/steer",
                json!({
                    "threadId": thread_id,
                    "expectedTurnId": expected_turn_id,
                    "input": input.clone(),
                }),
            ) {
                Ok(response) => response,
                Err(error) => {
                    let Some(found_turn_id) = extract_found_active_turn_id(&error.to_string())
                    else {
                        return Err(error);
                    };
                    self.mutate_state(|state| {
                        let context = state.metadata.context.get_or_insert_with(Default::default);
                        context.turn_id = Some(found_turn_id.clone());
                        context.turn_status = Some("inProgress".to_string());
                        context.last_activity = Some("turn inProgress".to_string());
                    })?;
                    self.call(
                        "turn/steer",
                        json!({
                            "threadId": thread_id,
                            "expectedTurnId": found_turn_id,
                            "input": input.clone(),
                        }),
                    )?
                }
            };
            if let Some(turn) = response.get("turn") {
                self.apply_turn(turn)?;
            }
            self.clear_last_error()?;
            self.append_text_line(format!("[operator] steer: {}", text.trim()))?;
            self.upsert_runtime_event(
                format!("operator:{}", now_epoch_ms()),
                "operator",
                "Operator steer",
                Some(truncate_block(text.trim(), 2400)),
                Some("inProgress".to_string()),
                Some("operator".to_string()),
            )?;
        } else {
            let response = self.call(
                "turn/start",
                build_turn_start_params(&thread_id, input, None, &self.protocol),
            )?;
            let turn = response.get("turn");
            if !active {
                if let Some(turn) = turn {
                    self.apply_turn(turn)?;
                }
                self.clear_last_error()?;
                let turn_status = turn
                    .and_then(|value| value.get("status"))
                    .and_then(Value::as_str)
                    .unwrap_or("inProgress")
                    .to_string();
                self.append_text_line(format!("[operator] new turn: {}", text.trim()))?;
                self.upsert_runtime_event(
                    format!("operator:{}", now_epoch_ms()),
                    "operator",
                    "Operator follow-up",
                    Some(truncate_block(text.trim(), 2400)),
                    Some(turn_status),
                    Some("operator".to_string()),
                )?;
                return Ok(());
            }

            let queued_turn_id = turn
                .and_then(|value| value.get("id"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let detail = match queued_turn_id.as_deref() {
                Some(turn_id) => format!(
                    "turn {} · {}",
                    short_thread_id(turn_id),
                    truncate_block(text.trim(), 2300)
                ),
                None => truncate_block(text.trim(), 2400),
            };
            self.append_text_line(format!("[operator] queued: {}", text.trim()))?;
            self.mutate_state(|state| {
                let context = state.metadata.context.get_or_insert_with(Default::default);
                context.last_error = None;
                context.last_activity = Some(match queued_turn_id.as_deref() {
                    Some(turn_id) => format!("queued follow-up {}", short_thread_id(turn_id)),
                    None => "queued follow-up".to_string(),
                });
            })?;
            self.upsert_runtime_event(
                format!("operator-queued:{}", now_epoch_ms()),
                "operator",
                "Operator follow-up queued",
                Some(detail),
                Some("queued".to_string()),
                Some("operator".to_string()),
            )?;
        }
        Ok(())
    }

    fn record_apps_runtime_status(&self) -> anyhow::Result<()> {
        if !codex_apps_probe_enabled(&self.metadata().shell_command) {
            return Ok(());
        }
        let response = self.call("mcpServerStatus/list", json!({}))?;
        let Some(servers) = response.get("data").and_then(Value::as_array) else {
            return Ok(());
        };
        let Some(codex_apps) = servers
            .iter()
            .find(|server| server.get("name").and_then(Value::as_str) == Some("codex_apps"))
        else {
            self.record_error(
                "codex_apps is enabled in Codex but was missing from mcpServerStatus/list after startup",
            )?;
            self.upsert_runtime_event(
                format!("mcp:{}", now_epoch_ms()),
                "mcp",
                "Apps MCP unavailable",
                Some(
                    "The built-in codex_apps server was not available after startup. This matches the intermittent ChatGPT apps handshake failures seen in Codex 0.116.0.".to_string(),
                ),
                Some("failed".to_string()),
                Some("codex_apps".to_string()),
            )?;
            return Ok(());
        };

        let auth_status = codex_apps
            .get("authStatus")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        self.mutate_state(|state| {
            let context = state.metadata.context.get_or_insert_with(Default::default);
            if auth_status == "unauthenticated" {
                context.last_activity = Some("apps auth unavailable".to_string());
            }
        })?;
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

fn codex_app_stdout_is_ignorable(line: &str) -> bool {
    matches!(
        line,
        "Debugger attached."
            | "The debugger will be deactivated again and closed"
            | "Waiting for the debugger to disconnect..."
    ) || line.starts_with("Debugger listening on ws://")
        || line.starts_with("For help, see: https://nodejs.org/en/docs/inspector")
        || line.starts_with(
            "OpenTelemetry eBPF Instrumentation has injected instrumentation via the NodeJS debugger",
        )
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

    wait_for_codex_app_session(&manifest.namespace).inspect_err(|_| {
        let _ = cleanup_stale_session(&manifest.namespace);
    })
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
    ensure_control_queue_dirs(&manifest.namespace, manifest.working_directory.as_deref())?;
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
    command.envs(&manifest.environment);
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
            codex_settings: protocol_settings_summary(&manifest.protocol),
            codex_features: manifest.protocol.enabled_features.clone(),
            codex_disabled_features: manifest.protocol.disabled_features.clone(),
            codex_environments: manifest.protocol.environments.clone(),
            memory_mode: manifest.protocol.memory_mode.clone(),
            goal_objective: manifest.protocol.goal.clone(),
            recent_events: vec![RuntimeFeedEntry {
                id: format!("session:{}:launch", manifest.namespace),
                kind: "session".to_string(),
                title: "Session launching".to_string(),
                timestamp_epoch_ms: now_epoch_ms(),
                actor: Some("jarvisctl".to_string()),
                detail: Some(truncate_block(&manifest.startup_prompt, 2400)),
                status: Some("launching".to_string()),
            }],
            ..manifest.context.clone()
        }),
        agents: vec![NativeAgentMetadata {
            name: "agent0".to_string(),
            pid: child_pid,
            running: true,
            exit_code: None,
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
        protocol: manifest.protocol.clone(),
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

    let tcp_listener = codex_app_tcp_listener()?;

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
                    if codex_app_stdout_is_ignorable(trimmed) {
                        let _ = stdout_session
                            .append_text_line(format!("[stdout ignored] {}", trimmed));
                        continue;
                    }
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

    let queue_session = Arc::clone(&session);
    thread::spawn(move || {
        while !queue_session.shutdown_requested.load(Ordering::SeqCst) {
            let _ = drain_control_queue(&queue_session);
            thread::sleep(Duration::from_millis(50));
        }
    });

    if let Some(tcp_listener) = tcp_listener {
        let tcp_session = Arc::clone(&session);
        thread::spawn(move || {
            while !tcp_session.shutdown_requested.load(Ordering::SeqCst) {
                match tcp_listener.accept() {
                    Ok((stream, _)) => {
                        let session = Arc::clone(&tcp_session);
                        thread::spawn(move || {
                            let _ = handle_tcp_client(stream, session);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => {
                        break;
                    }
                }
            }
        });
    }

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
    let working_directory = session.metadata().working_directory;
    let _ = fs::remove_dir_all(control_queue_root_for_namespace(
        &session.namespace,
        working_directory.as_deref(),
    ));
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
        match read_session_metadata_file(&name)? {
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
    if !session_dir_for(namespace)?.exists() {
        return Ok(None);
    }

    let socket_path = socket_path_for(namespace)?;
    let direct_metadata = (|| -> anyhow::Result<NativeSessionMetadata> {
        ensure!(
            socket_path.exists(),
            "failed to connect '{}'",
            socket_path.display()
        );
        let stream = UnixStream::connect(&socket_path)
            .with_context(|| format!("failed to connect '{}'", socket_path.display()))?;
        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
        let mut writer = stream
            .try_clone()
            .context("failed to clone codex app metadata stream")?;
        send_message(&mut writer, &ClientMessage::Metadata)?;
        let mut reader = BufReader::new(stream);
        match read_server_message(&mut reader)? {
            ServerMessage::Metadata(metadata) => Ok(metadata),
            ServerMessage::Error { message } => Err(anyhow!(message)),
            other => Err(anyhow!(
                "unexpected metadata response for codex app session '{}': {:?}",
                namespace,
                other
            )),
        }
    })();

    match direct_metadata {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) => match read_session_metadata_file(namespace)? {
            Some(metadata) => Ok(Some(metadata)),
            None => Err(error),
        },
    }
}

pub fn codex_app_session_dir_exists(namespace: &str) -> anyhow::Result<bool> {
    Ok(session_dir_for(namespace)?.exists())
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
        Err(error) => {
            if let Some(metadata) = read_session_metadata_file(namespace)? {
                let all_stopped = metadata.agents.iter().all(|agent| !agent.running);
                if all_stopped {
                    cleanup_stale_session(namespace)?;
                    return Ok(());
                }
            } else if session_dir_for(namespace)?.exists() {
                cleanup_stale_session(namespace)?;
                return Ok(());
            }
            wait_for_codex_app_shutdown(namespace)
                .or(Err(error))
                .context("failed to delete codex app session cleanly")
        }
    }
}

pub fn tell_codex_app(namespace: &str, contents: &str) -> anyhow::Result<()> {
    tell_codex_app_with_mode(namespace, contents, CodexAppInputMode::Auto)
}

pub fn tell_codex_app_with_mode(
    namespace: &str,
    contents: &str,
    mode: CodexAppInputMode,
) -> anyhow::Result<()> {
    match request(
        namespace,
        &ClientMessage::SendText {
            text: contents.to_string(),
            mode,
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

pub fn read_codex_app_thread(namespace: &str, include_turns: bool) -> anyhow::Result<Value> {
    match request(namespace, &ClientMessage::ReadThread { include_turns })? {
        ServerMessage::ThreadHistory(value) => Ok(value),
        ServerMessage::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!(
            "unexpected history response while reading codex app session '{}': {:?}",
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
                ServerMessage::Ok
                | ServerMessage::Metadata(..)
                | ServerMessage::ThreadHistory(..) => {}
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
    handle_client_stream(&mut reader, &mut writer, session)
}

fn handle_tcp_client(stream: TcpStream, session: Arc<CodexAppSession>) -> anyhow::Result<()> {
    let mut writer = stream
        .try_clone()
        .context("failed to clone codex app tcp client stream")?;
    let mut reader = BufReader::new(stream);
    handle_client_stream(&mut reader, &mut writer, session)
}

fn handle_client_stream(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    session: Arc<CodexAppSession>,
) -> anyhow::Result<()> {
    let request = read_client_message(reader)?;
    match request {
        ClientMessage::Attach { .. } => {
            let (backlog, rx) = session.subscribe();
            send_server_message(
                writer,
                &ServerMessage::Attached {
                    namespace: session.namespace.clone(),
                    agent: "agent0".to_string(),
                },
            )?;
            if !backlog.is_empty() {
                send_output(writer, &backlog)?;
            }
            while let Ok(chunk) = rx.recv() {
                if chunk.is_empty() {
                    continue;
                }
                if send_output(writer, &chunk).is_err() {
                    break;
                }
            }
            let _ = send_server_message(
                writer,
                &ServerMessage::Exited {
                    agent: "agent0".to_string(),
                },
            );
        }
        other => {
            let response = match handle_client_message(&session, other) {
                Ok(response) => response,
                Err(error) => ServerMessage::Error {
                    message: error.to_string(),
                },
            };
            send_server_message(writer, &response)?;
        }
    }
    Ok(())
}

fn handle_client_message(
    session: &Arc<CodexAppSession>,
    request: ClientMessage,
) -> anyhow::Result<ServerMessage> {
    match request {
        ClientMessage::Metadata => Ok(ServerMessage::Metadata(session.metadata())),
        ClientMessage::ReadThread { include_turns } => Ok(ServerMessage::ThreadHistory(
            session.read_thread(include_turns)?,
        )),
        ClientMessage::Attach { .. } => {
            bail!("attach is not supported over the filesystem control queue")
        }
        ClientMessage::SendText { text, mode } => {
            session.send_operator_message(&text, mode)?;
            Ok(ServerMessage::Ok)
        }
        ClientMessage::Interrupt => {
            session.interrupt_turn()?;
            Ok(ServerMessage::Ok)
        }
        ClientMessage::KillSession => {
            session.shutdown();
            Ok(ServerMessage::Ok)
        }
    }
}

fn request(namespace: &str, message: &ClientMessage) -> anyhow::Result<ServerMessage> {
    let socket_path = socket_path_for(namespace)?;
    let mut stream = match UnixStream::connect(&socket_path) {
        Ok(stream) => stream,
        Err(error) => {
            if !matches!(message, ClientMessage::Metadata)
                && let Some(response) = request_via_control_queue(namespace, message)?
            {
                return Ok(response);
            }
            return Err(error)
                .with_context(|| format!("failed to connect '{}'", socket_path.display()));
        }
    };
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

fn request_over_tcp_endpoint(
    host: &str,
    port: u16,
    message: &ClientMessage,
) -> anyhow::Result<ServerMessage> {
    let host = host.trim();
    ensure!(!host.is_empty(), "tcp host must not be empty");
    ensure!(port > 0, "tcp port must be greater than zero");
    let address = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&address)
        .with_context(|| format!("failed to connect codex app tcp control '{address}'"))?;
    send_message(&mut stream, message)?;
    let mut reader = BufReader::new(stream);
    read_server_message(&mut reader)
}

pub fn codex_app_session_metadata_tcp(
    host: &str,
    port: u16,
) -> anyhow::Result<NativeSessionMetadata> {
    match request_over_tcp_endpoint(host, port, &ClientMessage::Metadata)? {
        ServerMessage::Metadata(metadata) => Ok(metadata),
        ServerMessage::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!(
            "unexpected metadata response from codex app tcp control '{}:{}': {:?}",
            host,
            port,
            other
        )),
    }
}

pub fn tell_codex_app_with_mode_tcp(
    host: &str,
    port: u16,
    contents: &str,
    mode: CodexAppInputMode,
) -> anyhow::Result<()> {
    match request_over_tcp_endpoint(
        host,
        port,
        &ClientMessage::SendText {
            text: contents.to_string(),
            mode,
        },
    )? {
        ServerMessage::Ok => Ok(()),
        ServerMessage::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!(
            "unexpected tell response from codex app tcp control '{}:{}': {:?}",
            host,
            port,
            other
        )),
    }
}

pub fn interrupt_codex_app_tcp(host: &str, port: u16) -> anyhow::Result<()> {
    match request_over_tcp_endpoint(host, port, &ClientMessage::Interrupt)? {
        ServerMessage::Ok => Ok(()),
        ServerMessage::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!(
            "unexpected interrupt response from codex app tcp control '{}:{}': {:?}",
            host,
            port,
            other
        )),
    }
}

pub fn attach_codex_app_tcp(host: &str, port: u16, namespace: &str) -> anyhow::Result<()> {
    let address = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&address)
        .with_context(|| format!("failed to connect codex app tcp control '{}'", address))?;
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
                ServerMessage::Ok
                | ServerMessage::Metadata(..)
                | ServerMessage::ThreadHistory(..) => {}
            }
        }
    });

    view_agent(&format!("{}:agent0", namespace), output)
}

fn send_message(stream: &mut impl Write, message: &ClientMessage) -> anyhow::Result<()> {
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

fn send_server_message(stream: &mut impl Write, message: &ServerMessage) -> anyhow::Result<()> {
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

fn send_output(stream: &mut impl Write, bytes: &[u8]) -> anyhow::Result<()> {
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
    let deadline = SystemTime::now() + Duration::from_secs(30);
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
        if let Some(metadata) = read_session_metadata_file(namespace)? {
            let startup_failed = metadata.agents.iter().all(|agent| !agent.running)
                || metadata
                    .context
                    .as_ref()
                    .and_then(|context| context.last_error.as_deref())
                    .is_some();
            if startup_failed {
                let detail = metadata
                    .context
                    .as_ref()
                    .and_then(|context| context.last_error.clone())
                    .unwrap_or_else(|| "child exited before readiness".to_string());
                bail!(
                    "codex app session '{}' failed during startup: {}",
                    namespace,
                    detail
                );
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

fn codex_app_tcp_listener() -> anyhow::Result<Option<TcpListener>> {
    let Some(port) = env::var(TCP_CONTROL_PORT_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let port = port
        .parse::<u16>()
        .with_context(|| format!("failed to parse {TCP_CONTROL_PORT_ENV}='{port}'"))?;
    ensure!(port > 0, "{TCP_CONTROL_PORT_ENV} must be greater than zero");
    let host = env::var(TCP_CONTROL_HOST_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let address = format!("{host}:{port}");
    let listener = TcpListener::bind(&address)
        .with_context(|| format!("failed to bind codex app tcp control '{address}'"))?;
    listener
        .set_nonblocking(true)
        .context("failed to configure codex app tcp listener")?;
    Ok(Some(listener))
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

fn build_thread_start_params(manifest: &CodexAppLaunchManifest) -> Value {
    let mut params = serde_json::Map::new();
    insert_opt(&mut params, "cwd", manifest.working_directory.as_deref());
    insert_thread_protocol_params(
        &mut params,
        manifest.working_directory.as_deref(),
        &manifest.protocol,
    );
    merge_object_fields(&mut params, &manifest.protocol.thread_config);
    Value::Object(params)
}

fn build_thread_resume_params(thread_id: &str, manifest: &CodexAppLaunchManifest) -> Value {
    let mut params = serde_json::Map::new();
    params.insert("threadId".to_string(), json!(thread_id));
    insert_opt(&mut params, "cwd", manifest.working_directory.as_deref());
    insert_thread_protocol_params(
        &mut params,
        manifest.working_directory.as_deref(),
        &manifest.protocol,
    );
    merge_object_fields(&mut params, &manifest.protocol.thread_config);
    Value::Object(params)
}

fn build_turn_start_params(
    thread_id: &str,
    input: Vec<Value>,
    cwd: Option<&str>,
    protocol: &CodexAppProtocolConfig,
) -> Value {
    let mut params = serde_json::Map::new();
    params.insert("threadId".to_string(), json!(thread_id));
    params.insert("input".to_string(), Value::Array(input));
    insert_opt(&mut params, "cwd", cwd);
    insert_opt(&mut params, "model", protocol.model.as_deref());
    insert_opt(&mut params, "effort", protocol.reasoning_effort.as_deref());
    insert_opt(
        &mut params,
        "summary",
        protocol.reasoning_summary.as_deref(),
    );
    insert_opt(
        &mut params,
        "approvalPolicy",
        protocol.approval_policy.as_deref(),
    );
    insert_opt(
        &mut params,
        "approvalsReviewer",
        protocol.approvals_reviewer.as_deref(),
    );
    insert_opt(&mut params, "personality", protocol.personality.as_deref());
    insert_opt(&mut params, "serviceTier", protocol.service_tier.as_deref());
    if protocol.permission_profile.is_none()
        && let Some(sandbox_mode) = protocol.sandbox_mode.as_deref()
    {
        params.insert(
            "sandboxPolicy".to_string(),
            sandbox_policy_value(sandbox_mode, &protocol.permission_additional_writable_roots),
        );
    }
    if let Some(permission_profile) = permission_profile_value(protocol) {
        params.insert("permissions".to_string(), permission_profile);
    }
    if let Some(environments) = environments_value(cwd, protocol) {
        params.insert("environments".to_string(), environments);
    }
    merge_object_fields(&mut params, &protocol.turn_config);
    Value::Object(params)
}

fn insert_thread_protocol_params(
    params: &mut serde_json::Map<String, Value>,
    cwd: Option<&str>,
    protocol: &CodexAppProtocolConfig,
) {
    insert_opt(params, "model", protocol.model.as_deref());
    insert_opt(params, "modelProvider", protocol.model_provider.as_deref());
    insert_opt(
        params,
        "approvalPolicy",
        protocol.approval_policy.as_deref(),
    );
    insert_opt(
        params,
        "approvalsReviewer",
        protocol.approvals_reviewer.as_deref(),
    );
    if protocol.permission_profile.is_none() {
        insert_opt(params, "sandbox", protocol.sandbox_mode.as_deref());
    }
    insert_opt(params, "personality", protocol.personality.as_deref());
    insert_opt(params, "serviceName", protocol.service_name.as_deref());
    insert_opt(params, "serviceTier", protocol.service_tier.as_deref());
    insert_opt(params, "threadSource", protocol.thread_source.as_deref());
    insert_opt(
        params,
        "sessionStartSource",
        protocol.session_start_source.as_deref(),
    );
    insert_opt(
        params,
        "developerInstructions",
        protocol.developer_instructions.as_deref(),
    );
    insert_opt(
        params,
        "baseInstructions",
        protocol.base_instructions.as_deref(),
    );
    if let Some(ephemeral) = protocol.ephemeral {
        params.insert("ephemeral".to_string(), json!(ephemeral));
    }
    if let Some(config) = config_value(protocol) {
        params.insert("config".to_string(), config);
    }
    if let Some(permission_profile) = permission_profile_value(protocol) {
        params.insert("permissions".to_string(), permission_profile);
    }
    if let Some(environments) = environments_value(cwd, protocol) {
        params.insert("environments".to_string(), environments);
    }
}

fn config_value(protocol: &CodexAppProtocolConfig) -> Option<Value> {
    let mut config = serde_json::Map::new();
    insert_opt(
        &mut config,
        "model_reasoning_effort",
        protocol.reasoning_effort.as_deref(),
    );
    insert_opt(
        &mut config,
        "model_reasoning_summary",
        protocol.reasoning_summary.as_deref(),
    );
    if config.is_empty() {
        None
    } else {
        Some(Value::Object(config))
    }
}

fn sandbox_policy_value(sandbox_mode: &str, writable_roots: &[String]) -> Value {
    match sandbox_mode {
        "danger-full-access" => json!({ "type": "dangerFullAccess" }),
        "read-only" => json!({ "type": "readOnly" }),
        "workspace-write" => {
            let roots = writable_roots
                .iter()
                .map(|root| Value::String(root.clone()))
                .collect::<Vec<_>>();
            json!({
                "type": "workspaceWrite",
                "writableRoots": roots,
            })
        }
        _ => json!({ "type": "externalSandbox" }),
    }
}

fn permission_profile_value(protocol: &CodexAppProtocolConfig) -> Option<Value> {
    let profile = protocol.permission_profile.as_deref()?;
    let mut value = serde_json::Map::new();
    value.insert("type".to_string(), json!("profile"));
    value.insert("id".to_string(), json!(profile));
    if !protocol.permission_additional_writable_roots.is_empty() {
        value.insert(
            "modifications".to_string(),
            Value::Array(
                protocol
                    .permission_additional_writable_roots
                    .iter()
                    .map(|path| {
                        json!({
                            "type": "additionalWritableRoot",
                            "path": path,
                        })
                    })
                    .collect(),
            ),
        );
    }
    Some(Value::Object(value))
}

fn environments_value(cwd: Option<&str>, protocol: &CodexAppProtocolConfig) -> Option<Value> {
    if protocol.environments.is_empty() {
        return None;
    }
    let values = protocol
        .environments
        .iter()
        .map(|environment_id| match cwd {
            Some(cwd) => json!({
                "environmentId": environment_id,
                "cwd": cwd,
            }),
            None => json!({
                "environmentId": environment_id,
            }),
        })
        .collect::<Vec<_>>();
    Some(Value::Array(values))
}

fn insert_opt(params: &mut serde_json::Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        params.insert(key.to_string(), json!(value));
    }
}

fn merge_object_fields(
    params: &mut serde_json::Map<String, Value>,
    overrides: &BTreeMap<String, Value>,
) {
    for (key, value) in overrides {
        params.insert(key.clone(), value.clone());
    }
}

fn protocol_settings_summary(protocol: &CodexAppProtocolConfig) -> BTreeMap<String, String> {
    let mut settings = BTreeMap::new();
    for (key, value) in [
        ("model", protocol.model.as_deref()),
        ("model_provider", protocol.model_provider.as_deref()),
        ("reasoning_effort", protocol.reasoning_effort.as_deref()),
        ("reasoning_summary", protocol.reasoning_summary.as_deref()),
        ("sandbox_mode", protocol.sandbox_mode.as_deref()),
        ("approval_policy", protocol.approval_policy.as_deref()),
        ("approvals_reviewer", protocol.approvals_reviewer.as_deref()),
        ("personality", protocol.personality.as_deref()),
        ("service_name", protocol.service_name.as_deref()),
        ("service_tier", protocol.service_tier.as_deref()),
        ("thread_source", protocol.thread_source.as_deref()),
        (
            "session_start_source",
            protocol.session_start_source.as_deref(),
        ),
        ("permission_profile", protocol.permission_profile.as_deref()),
    ] {
        if let Some(value) = value {
            settings.insert(key.to_string(), value.to_string());
        }
    }
    if let Some(ephemeral) = protocol.ephemeral {
        settings.insert("ephemeral".to_string(), ephemeral.to_string());
    }
    if !protocol.permission_additional_writable_roots.is_empty() {
        settings.insert(
            "permission_additional_writable_roots".to_string(),
            protocol.permission_additional_writable_roots.join(","),
        );
    }
    settings
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

fn codex_apps_probe_enabled(command: &str) -> bool {
    let Ok(parts) = shell_words::split(command) else {
        return true;
    };
    let mut explicit = None;
    let mut index = 0usize;
    while index < parts.len() {
        match parts[index].as_str() {
            "--disable" if parts.get(index + 1).map(String::as_str) == Some("apps") => {
                explicit = Some(false);
                index += 2;
                continue;
            }
            "--enable" if parts.get(index + 1).map(String::as_str) == Some("apps") => {
                explicit = Some(true);
                index += 2;
                continue;
            }
            "-c" => {
                if let Some(value) = parts.get(index + 1) {
                    if value.contains("features.apps=false") {
                        explicit = Some(false);
                    } else if value.contains("features.apps=true") {
                        explicit = Some(true);
                    }
                }
                index += 2;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    explicit.unwrap_or(true)
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

fn truncate_block(raw: &str, limit: usize) -> String {
    let normalized = raw.trim().to_string();
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
        .map(ToOwned::to_owned);
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
        let action_timestamp = now_epoch_ms();
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
                        .map(ToOwned::to_owned),
                ),
                _ => (
                    map_tool_call_status_to_subagent_status(&call_status).to_string(),
                    None,
                ),
            };
        let action = build_subagent_action(
            &receiver_thread_id,
            &tool,
            &status,
            action_timestamp,
            prompt_preview.as_deref(),
            latest_message.as_deref(),
        );
        upsert_subagent(
            &mut context.subagents,
            RuntimeSubagentMetadata {
                thread_id: receiver_thread_id,
                tool: tool.clone(),
                status,
                updated_at_epoch_ms: action_timestamp,
                parent_thread_id: sender_thread_id.clone(),
                model: model.clone(),
                reasoning_effort: reasoning_effort.clone(),
                prompt_preview: prompt_preview.clone(),
                latest_message,
                recent_actions: vec![action],
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
        let existing = subagents.remove(index);
        let mut merged = existing.recent_actions;
        merge_subagent_actions(&mut merged, subagent.recent_actions);
        subagents.push(RuntimeSubagentMetadata {
            recent_actions: merged,
            ..subagent
        });
    } else {
        subagents.push(subagent);
    }
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

fn merge_subagent_actions(
    existing: &mut Vec<RuntimeSubagentAction>,
    incoming: Vec<RuntimeSubagentAction>,
) {
    for action in incoming {
        if let Some(index) = existing.iter().position(|entry| entry.id == action.id) {
            existing.remove(index);
        }
        existing.push(action);
    }
    existing.sort_by(|left, right| {
        left.timestamp_epoch_ms
            .cmp(&right.timestamp_epoch_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    let overflow = existing.len().saturating_sub(SUBAGENT_ACTION_LIMIT);
    if overflow > 0 {
        existing.drain(0..overflow);
    }
}

fn build_subagent_action(
    thread_id: &str,
    tool: &str,
    status: &str,
    timestamp_epoch_ms: u128,
    prompt_preview: Option<&str>,
    latest_message: Option<&str>,
) -> RuntimeSubagentAction {
    RuntimeSubagentAction {
        id: format!("{thread_id}:{tool}:{timestamp_epoch_ms}"),
        kind: tool.to_string(),
        title: subagent_action_title(tool),
        timestamp_epoch_ms,
        detail: latest_message
            .map(ToOwned::to_owned)
            .or_else(|| prompt_preview.map(|value| truncate_block(value, 2400))),
        status: Some(status.to_string()),
    }
}

fn subagent_action_title(tool: &str) -> String {
    match tool {
        "spawnAgent" => "Spawned branch".to_string(),
        "sendInput" => "Delivered follow-up".to_string(),
        "resumeAgent" => "Resumed branch".to_string(),
        "wait" => "Waiting on branch".to_string(),
        "closeAgent" => "Closed branch".to_string(),
        _ => tool.to_string(),
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
    let detail = build_subagent_state_summary(item).or_else(|| {
        item.get("prompt")
            .and_then(Value::as_str)
            .map(|value| truncate_block(value, 2400))
    });
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

fn subagent_console_line(item: &Value) -> String {
    let tool = item
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("spawnAgent");
    let status = item
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("inProgress");
    let receiver_count = item
        .get("receiverThreadIds")
        .and_then(Value::as_array)
        .map(|values| values.len())
        .unwrap_or(0);
    let summary = build_subagent_state_summary(item).or_else(|| {
        item.get("prompt")
            .and_then(Value::as_str)
            .map(|value| truncate_line(value, 140))
    });
    let headline = format!(
        "{} [{}]",
        subagent_title(tool, receiver_count, "Subagent activity"),
        status
    );
    match summary {
        Some(summary) => truncate_line(&format!("{headline} {summary}"), 220),
        None => headline,
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
    truncate_line(&subagent_console_line(item), 120)
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

pub fn cleanup_stale_session(namespace: &str) -> anyhow::Result<()> {
    let session_dir = session_dir_for(namespace)?;
    let socket_path = session_dir.join(SOCKET_FILE_NAME);
    let metadata_path = session_dir.join(METADATA_FILE_NAME);
    let events_path = session_dir.join(EVENTS_FILE_NAME);
    let working_directory = read_session_metadata_file(namespace)?
        .as_ref()
        .and_then(|metadata| metadata.working_directory.as_deref())
        .map(ToOwned::to_owned);

    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }
    if metadata_path.exists() {
        let _ = fs::remove_file(&metadata_path);
    }
    if events_path.exists() {
        let _ = fs::remove_file(&events_path);
    }
    let control_queue_root =
        control_queue_root_for_namespace(namespace, working_directory.as_deref());
    if control_queue_root.exists() {
        let _ = fs::remove_dir_all(&control_queue_root);
    }
    if session_dir.exists() {
        let _ = fs::remove_dir_all(&session_dir);
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

fn control_queue_base_dir(working_directory: Option<&str>) -> PathBuf {
    match working_directory.filter(|value| !value.trim().is_empty()) {
        Some(directory) => PathBuf::from(directory).join(CONTROL_QUEUE_DIR_NAME),
        None => env::temp_dir().join("jarvisctl-control-queue"),
    }
}

fn control_queue_root_for_namespace(namespace: &str, working_directory: Option<&str>) -> PathBuf {
    control_queue_base_dir(working_directory).join(sanitize_namespace(namespace))
}

fn control_queue_requests_dir_for_namespace(
    namespace: &str,
    working_directory: Option<&str>,
) -> PathBuf {
    control_queue_root_for_namespace(namespace, working_directory)
        .join(CONTROL_QUEUE_REQUESTS_DIR_NAME)
}

fn control_queue_responses_dir_for_namespace(
    namespace: &str,
    working_directory: Option<&str>,
) -> PathBuf {
    control_queue_root_for_namespace(namespace, working_directory)
        .join(CONTROL_QUEUE_RESPONSES_DIR_NAME)
}

fn ensure_control_queue_dirs(
    namespace: &str,
    working_directory: Option<&str>,
) -> anyhow::Result<()> {
    let requests_dir = control_queue_requests_dir_for_namespace(namespace, working_directory);
    let responses_dir = control_queue_responses_dir_for_namespace(namespace, working_directory);
    fs::create_dir_all(&requests_dir)
        .with_context(|| format!("failed to create '{}'", requests_dir.display()))?;
    fs::create_dir_all(&responses_dir)
        .with_context(|| format!("failed to create '{}'", responses_dir.display()))?;
    Ok(())
}

fn drain_control_queue(session: &Arc<CodexAppSession>) -> anyhow::Result<()> {
    let working_directory = session.metadata().working_directory;
    let requests_dir =
        control_queue_requests_dir_for_namespace(&session.namespace, working_directory.as_deref());
    if !requests_dir.exists() {
        return Ok(());
    }

    let mut request_paths = Vec::new();
    for entry in fs::read_dir(&requests_dir)
        .with_context(|| format!("failed to read '{}'", requests_dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            request_paths.push(entry.path());
        }
    }
    request_paths.sort();

    for request_path in request_paths {
        let _ = process_control_queue_request(session, &request_path);
        if session.shutdown_requested.load(Ordering::SeqCst) {
            break;
        }
    }

    Ok(())
}

fn process_control_queue_request(
    session: &Arc<CodexAppSession>,
    request_path: &Path,
) -> anyhow::Result<()> {
    let raw = fs::read_to_string(request_path)
        .with_context(|| format!("failed to read '{}'", request_path.display()))?;
    let envelope: ControlQueueRequestEnvelope = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse '{}'", request_path.display()))?;
    fs::remove_file(request_path)
        .with_context(|| format!("failed to remove '{}'", request_path.display()))?;

    let response = match handle_client_message(session, envelope.message) {
        Ok(message) => message,
        Err(error) => ServerMessage::Error {
            message: error.to_string(),
        },
    };
    let working_directory = session.metadata().working_directory;
    let response_path =
        control_queue_responses_dir_for_namespace(&session.namespace, working_directory.as_deref())
            .join(format!("{}.json", envelope.id));
    write_json_file(
        &response_path,
        &ControlQueueResponseEnvelope { message: response },
    )
}

fn request_via_control_queue(
    namespace: &str,
    message: &ClientMessage,
) -> anyhow::Result<Option<ServerMessage>> {
    let Some(metadata) = read_session_metadata_file(namespace)? else {
        return Ok(None);
    };
    if !metadata.agents.iter().any(|agent| agent.running) {
        return Ok(None);
    }

    ensure_control_queue_dirs(namespace, metadata.working_directory.as_deref())?;
    let request_id = format!(
        "{}-{}-{}",
        now_epoch_ms(),
        std::process::id(),
        CONTROL_QUEUE_REQUEST_SEQUENCE.fetch_add(1, Ordering::SeqCst)
    );
    let request_path =
        control_queue_requests_dir_for_namespace(namespace, metadata.working_directory.as_deref())
            .join(format!("{}.json", request_id));
    let response_path =
        control_queue_responses_dir_for_namespace(namespace, metadata.working_directory.as_deref())
            .join(format!("{}.json", request_id));
    write_json_file(
        &request_path,
        &ControlQueueRequestEnvelope {
            id: request_id.clone(),
            created_at_epoch_ms: now_epoch_ms(),
            message: message.clone(),
        },
    )?;

    let deadline = SystemTime::now() + control_queue_response_timeout(message);
    loop {
        if response_path.exists() {
            let raw = fs::read_to_string(&response_path)
                .with_context(|| format!("failed to read '{}'", response_path.display()))?;
            let envelope: ControlQueueResponseEnvelope = serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse '{}'", response_path.display()))?;
            let _ = fs::remove_file(&response_path);
            return Ok(Some(envelope.message));
        }
        if SystemTime::now() >= deadline {
            bail!(
                "timed out waiting for filesystem control response for '{}'",
                namespace
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn control_queue_response_timeout(message: &ClientMessage) -> Duration {
    let _ = message;
    Duration::from_secs(5)
}

fn write_json_file(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let raw = serde_json::to_vec(value).context("failed to encode control queue payload")?;
    fs::write(path, raw).map_err(|error| anyhow!("failed to write '{}': {}", path.display(), error))
}

fn extract_found_active_turn_id(message: &str) -> Option<String> {
    let marker = "but found `";
    let start = message.find(marker)? + marker.len();
    let remaining = &message[start..];
    let end = remaining.find('`')?;
    Some(remaining[..end].to_string())
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
