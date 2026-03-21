use anyhow::{Context, anyhow, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use crossterm::{
    cursor,
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, PopKeyboardEnhancementFlags,
    },
    execute, queue,
    terminal::{LeaveAlternateScreen, supports_keyboard_enhancement},
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::debug;

use crossterm::terminal::size as terminal_size;

const SESSION_DIR_NAME: &str = "sessions";
const MANIFEST_DIR_NAME: &str = "manifests";
const SOCKET_FILE_NAME: &str = "control.sock";
const METADATA_FILE_NAME: &str = "metadata.json";
const LOG_LIMIT_BYTES: usize = 512 * 1024;
const ALT_DETACH_BYTE: u8 = 0x1c;
const PRIMARY_DETACH_BYTE: u8 = 0x1d;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AttachViewport {
    cols: u16,
    rows: u16,
    content_rows: u16,
}

#[derive(Clone, Debug)]
enum FooterMode {
    Normal,
    Leader,
    Command(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AttachBindings {
    leader_byte: u8,
    leader_label: &'static str,
    leader_key_label: &'static str,
    literal_key: u8,
}

struct NativeAttachRawMode {
    fd: i32,
    original: libc::termios,
}

impl Drop for NativeAttachRawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NativeSessionManifest {
    namespace: String,
    agents: usize,
    working_directory: Option<String>,
    shell_command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context: Option<RuntimeContextMetadata>,
    initial_rows: Option<u16>,
    initial_cols: Option<u16>,
    created_at_epoch_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeFeedEntry {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub timestamp_epoch_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeSubagentAction {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub timestamp_epoch_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeSubagentMetadata {
    pub thread_id: String,
    pub tool: String,
    pub status: String,
    pub updated_at_epoch_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_actions: Vec<RuntimeSubagentAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeContextMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_log_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_maps: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_events: Vec<RuntimeFeedEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<RuntimeSubagentMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeSessionMetadata {
    pub namespace: String,
    pub backend: String,
    pub created_at_epoch_ms: u128,
    pub working_directory: Option<String>,
    pub shell_command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<RuntimeContextMetadata>,
    pub agents: Vec<NativeAgentMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeAgentMetadata {
    pub name: String,
    pub pid: u32,
    pub running: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
enum ClientMessage {
    Metadata,
    Attach {
        agent: String,
        rows: Option<u16>,
        cols: Option<u16>,
    },
    Interrupt {
        agent: String,
    },
    Input {
        agent: String,
        data_base64: String,
    },
    Resize {
        agent: String,
        rows: u16,
        cols: u16,
    },
    SendText {
        agent: String,
        text: String,
        press_enter: bool,
    },
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

struct NativeSession {
    namespace: String,
    created_at_epoch_ms: u128,
    working_directory: Option<String>,
    shell_command: String,
    context: Option<RuntimeContextMetadata>,
    session_dir: PathBuf,
    agents: BTreeMap<String, Arc<ManagedAgent>>,
    shutdown_requested: AtomicBool,
}

struct ManagedAgent {
    name: String,
    pid: u32,
    running: AtomicBool,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send>>,
    log: Mutex<VecDeque<u8>>,
    subscribers: Mutex<Vec<mpsc::Sender<Vec<u8>>>>,
}

impl NativeSession {
    fn metadata(&self) -> NativeSessionMetadata {
        NativeSessionMetadata {
            namespace: self.namespace.clone(),
            backend: "native".to_string(),
            created_at_epoch_ms: self.created_at_epoch_ms,
            working_directory: self.working_directory.clone(),
            shell_command: self.shell_command.clone(),
            context: self.context.clone(),
            agents: self
                .agents
                .values()
                .map(|agent| NativeAgentMetadata {
                    name: agent.name.clone(),
                    pid: agent.pid,
                    running: agent.is_running(),
                })
                .collect(),
        }
    }

    fn metadata_path(&self) -> PathBuf {
        self.session_dir.join(METADATA_FILE_NAME)
    }

    fn write_metadata(&self) -> anyhow::Result<()> {
        let raw = serde_json::to_string_pretty(&self.metadata())
            .context("failed to serialize native session metadata")?;
        fs::write(self.metadata_path(), raw).with_context(|| {
            format!(
                "failed to write native session metadata '{}'",
                self.metadata_path().display()
            )
        })
    }

    fn agent(&self, name: &str) -> anyhow::Result<Arc<ManagedAgent>> {
        self.agents.get(name).cloned().ok_or_else(|| {
            anyhow!(
                "native session '{}' has no agent '{}'",
                self.namespace,
                name
            )
        })
    }

    fn shutdown(&self) {
        if self.shutdown_requested.swap(true, Ordering::SeqCst) {
            return;
        }

        for agent in self.agents.values() {
            let _ = agent.kill();
        }

        let _ = fs::remove_file(self.session_dir.join(SOCKET_FILE_NAME));
        let _ = fs::remove_file(self.metadata_path());
        let _ = fs::remove_dir(&self.session_dir);
    }

    fn all_agents_exited(&self) -> bool {
        self.agents.values().all(|agent| !agent.is_running())
    }
}

impl ManagedAgent {
    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
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

    fn append_output(&self, chunk: &[u8]) {
        {
            let mut log = self.log.lock().unwrap();
            for byte in chunk {
                log.push_back(*byte);
            }
            while log.len() > LOG_LIMIT_BYTES {
                log.pop_front();
            }
        }

        let mut subscribers = self.subscribers.lock().unwrap();
        subscribers.retain(|sender| sender.send(chunk.to_vec()).is_ok());
    }

    fn send_input(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer
            .write_all(bytes)
            .context("failed to write bytes into native PTY")?;
        writer.flush().context("failed to flush native PTY writer")
    }

    fn send_text(&self, text: &str, press_enter: bool) -> anyhow::Result<()> {
        self.send_input(text.as_bytes())?;
        if press_enter {
            self.send_input(b"\r")?;
        }
        Ok(())
    }

    fn resize(&self, rows: u16, cols: u16) -> anyhow::Result<()> {
        let master = self.master.lock().unwrap();
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to resize native PTY")?;
        drop(master);
        self.signal(libc::SIGWINCH)
            .context("failed to signal native PTY resize")
    }

    fn kill(&self) -> anyhow::Result<()> {
        let mut child = self.child.lock().unwrap();
        child.kill().context("failed to kill native PTY child")
    }

    fn interrupt(&self) -> anyhow::Result<()> {
        self.signal(libc::SIGINT)
    }

    fn signal(&self, signal: i32) -> anyhow::Result<()> {
        {
            let master = self.master.lock().unwrap();
            if let Some(foreground_pgid) = master.process_group_leader() {
                let status = unsafe { libc::killpg(foreground_pgid, signal) };
                if status == 0 {
                    return Ok(());
                }
            }
        }

        let pid = i32::try_from(self.pid).context("native agent pid does not fit in i32")?;
        let pgid = unsafe { libc::getpgid(pid) };
        if pgid > 0 {
            let status = unsafe { libc::killpg(pgid, signal) };
            if status == 0 {
                return Ok(());
            }
        }
        let status = unsafe { libc::kill(pid, signal) };
        if status == 0 {
            return Ok(());
        }

        Err(anyhow!(
            "failed to send signal {} to native agent '{}' (pid {})",
            signal,
            self.name,
            self.pid
        ))
    }
}

pub fn spawn_native_session(
    namespace: &str,
    agents: usize,
    working_dir: Option<&str>,
    shell_command: &str,
    context: Option<RuntimeContextMetadata>,
) -> anyhow::Result<()> {
    ensure!(
        !namespace.trim().is_empty(),
        "namespace must not be empty for native backend"
    );
    ensure!(agents > 0, "native backend needs at least one agent");

    let manifest = NativeSessionManifest {
        namespace: namespace.to_string(),
        agents,
        working_directory: working_dir.map(ToOwned::to_owned),
        shell_command: shell_command.to_string(),
        context,
        initial_rows: current_attach_viewport().map(|viewport| viewport.content_rows),
        initial_cols: current_attach_viewport().map(|viewport| viewport.cols),
        created_at_epoch_ms: now_epoch_ms()?,
    };

    let manifest_dir = native_root()?.join(MANIFEST_DIR_NAME);
    fs::create_dir_all(&manifest_dir)
        .with_context(|| format!("failed to create '{}'", manifest_dir.display()))?;
    let manifest_path = manifest_dir.join(format!(
        "{}-{}.json",
        sanitize_namespace(namespace),
        manifest.created_at_epoch_ms
    ));
    let manifest_raw =
        serde_json::to_string_pretty(&manifest).context("failed to encode native manifest")?;
    fs::write(&manifest_path, manifest_raw)
        .with_context(|| format!("failed to write '{}'", manifest_path.display()))?;

    let current_exe = env::current_exe().context("failed to resolve current jarvisctl path")?;
    let mut command = Command::new(current_exe);
    command
        .arg("native-session-serve")
        .arg("--manifest")
        .arg(&manifest_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .spawn()
        .context("failed to spawn native session server")?;

    wait_for_native_session(namespace)
}

pub fn serve_native_session(manifest_path: PathBuf) -> anyhow::Result<()> {
    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read '{}'", manifest_path.display()))?;
    let manifest: NativeSessionManifest =
        serde_json::from_str(&raw).context("failed to parse native manifest")?;
    let _ = fs::remove_file(&manifest_path);

    let session_dir = native_root()?
        .join(SESSION_DIR_NAME)
        .join(sanitize_namespace(&manifest.namespace));
    fs::create_dir_all(&session_dir)
        .with_context(|| format!("failed to create '{}'", session_dir.display()))?;
    let socket_path = session_dir.join(SOCKET_FILE_NAME);
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    let mut agents = BTreeMap::new();
    for index in 0..manifest.agents {
        let agent_name = format!("agent{}", index);
        let agent = spawn_managed_agent(
            &agent_name,
            manifest.working_directory.as_deref(),
            &manifest.shell_command,
            manifest.initial_rows.zip(manifest.initial_cols),
        )?;
        agents.insert(agent_name, agent);
    }

    let session = Arc::new(NativeSession {
        namespace: manifest.namespace,
        created_at_epoch_ms: manifest.created_at_epoch_ms,
        working_directory: manifest.working_directory,
        shell_command: manifest.shell_command,
        context: manifest.context,
        session_dir: session_dir.clone(),
        agents,
        shutdown_requested: AtomicBool::new(false),
    });
    session.write_metadata()?;

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind '{}'", socket_path.display()))?;
    listener
        .set_nonblocking(true)
        .context("failed to configure native session listener")?;

    while !session.shutdown_requested.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let session = Arc::clone(&session);
                thread::spawn(move || {
                    let _ = handle_client(stream, session);
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to accept '{}'", socket_path.display()));
            }
        }

        if session.all_agents_exited() {
            session.shutdown_requested.store(true, Ordering::SeqCst);
        }
    }

    session.shutdown();
    Ok(())
}

pub fn collect_native_sessions() -> anyhow::Result<Vec<NativeSessionMetadata>> {
    let sessions_dir = native_root()?.join(SESSION_DIR_NAME);
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
        match native_session_metadata(&name)? {
            Some(metadata) => sessions.push(metadata),
            None => {
                let _ = cleanup_stale_session(&name);
            }
        }
    }

    sessions.sort_by(|left, right| left.namespace.cmp(&right.namespace));
    Ok(sessions)
}

pub fn native_session_metadata(namespace: &str) -> anyhow::Result<Option<NativeSessionMetadata>> {
    let socket_path = socket_path_for(namespace)?;
    if !socket_path.exists() {
        let _ = cleanup_stale_session(namespace);
        return Ok(None);
    }

    let stream = match UnixStream::connect(&socket_path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            cleanup_stale_session(namespace)?;
            return Ok(None);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to connect '{}'", socket_path.display()));
        }
    };

    let mut writer = stream
        .try_clone()
        .context("failed to clone native metadata stream")?;
    send_message(&mut writer, &ClientMessage::Metadata)?;
    let mut reader = BufReader::new(stream);
    match read_server_message(&mut reader)? {
        ServerMessage::Metadata(metadata) => Ok(Some(metadata)),
        ServerMessage::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!(
            "unexpected metadata response for native session '{}': {:?}",
            namespace,
            other
        )),
    }
}

pub fn delete_native_session(namespace: &str) -> anyhow::Result<()> {
    match request(namespace, &ClientMessage::KillSession) {
        Ok(ServerMessage::Ok) => {
            wait_for_native_shutdown(namespace)?;
            Ok(())
        }
        Ok(ServerMessage::Error { message }) => Err(anyhow!(message)),
        Ok(other) => Err(anyhow!(
            "unexpected response while deleting native session '{}': {:?}",
            namespace,
            other
        )),
        Err(error) => wait_for_native_shutdown(namespace)
            .or(Err(error))
            .context("failed to delete native session cleanly"),
    }
}

pub fn tell_native(
    namespace: &str,
    agent: &str,
    contents: &str,
    press_enter: bool,
) -> anyhow::Result<()> {
    let response = request(
        namespace,
        &ClientMessage::SendText {
            agent: agent.to_string(),
            text: contents.to_string(),
            press_enter,
        },
    )?;
    match response {
        ServerMessage::Ok => Ok(()),
        ServerMessage::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!(
            "unexpected response while telling native agent '{}:{}': {:?}",
            namespace,
            agent,
            other
        )),
    }
}

pub fn interrupt_native(namespace: &str, agent: &str) -> anyhow::Result<()> {
    let response = request(
        namespace,
        &ClientMessage::Interrupt {
            agent: agent.to_string(),
        },
    )?;
    match response {
        ServerMessage::Ok => Ok(()),
        ServerMessage::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!(
            "unexpected response while interrupting native agent '{}:{}': {:?}",
            namespace,
            agent,
            other
        )),
    }
}

pub fn attach_native(namespace: &str, agent: &str) -> anyhow::Result<()> {
    let socket_path = socket_path_for(namespace)?;
    let mut stream = UnixStream::connect(&socket_path)
        .with_context(|| format!("failed to connect '{}'", socket_path.display()))?;
    let stdin_fd = io::stdin().as_raw_fd();
    let bindings = current_attach_bindings();
    let initial_viewport = current_attach_viewport();
    send_message(
        &mut stream,
        &ClientMessage::Attach {
            agent: agent.to_string(),
            rows: initial_viewport.map(|viewport| viewport.content_rows),
            cols: initial_viewport.map(|viewport| viewport.cols),
        },
    )?;

    let _raw_mode =
        enable_native_attach_raw_mode().context("failed to enable raw mode for native attach")?;
    let render_lock = Arc::new(Mutex::new(()));
    let footer_mode = Arc::new(Mutex::new(FooterMode::Normal));
    let mut viewport = initial_viewport;
    if let Some(current_viewport) = viewport {
        render_attach_footer(
            &render_lock,
            &footer_mode,
            bindings,
            namespace,
            agent,
            current_viewport,
        )
        .ok();
    }

    let mut read_socket = stream
        .try_clone()
        .context("failed to clone native attach stream")?;
    read_socket
        .set_nonblocking(true)
        .context("failed to configure native attach socket as nonblocking")?;
    let remote_done = Arc::new(AtomicBool::new(false));
    let remote_done_thread = Arc::clone(&remote_done);
    let client_detached = Arc::new(AtomicBool::new(false));
    let client_detached_thread = Arc::clone(&client_detached);
    let render_lock_thread = Arc::clone(&render_lock);
    let footer_mode_thread = Arc::clone(&footer_mode);
    let namespace_thread = namespace.to_string();
    let agent_thread = agent.to_string();
    let output_thread = thread::spawn(move || -> anyhow::Result<()> {
        let result = (|| -> anyhow::Result<()> {
            let mut stdout = io::stdout().lock();
            let mut line_buffer = Vec::new();
            loop {
                if client_detached_thread.load(Ordering::SeqCst) {
                    break;
                }

                let message =
                    match read_server_message_nonblocking(&mut read_socket, &mut line_buffer) {
                        Ok(Some(message)) => message,
                        Ok(None) => {
                            thread::sleep(Duration::from_millis(25));
                            continue;
                        }
                        Err(error) => {
                            if client_detached_thread.load(Ordering::SeqCst)
                                || error
                                    .to_string()
                                    .contains("native server connection closed")
                            {
                                break;
                            }
                            return Err(error);
                        }
                    };

                match message {
                    ServerMessage::Attached { .. } => {}
                    ServerMessage::Output { data_base64 } => {
                        let bytes = BASE64
                            .decode(data_base64)
                            .context("failed to decode native attach output")?;
                        let mode = footer_mode_thread.lock().unwrap().clone();
                        {
                            let _guard = render_lock_thread.lock().unwrap();
                            stdout
                                .write_all(&bytes)
                                .context("failed to write native attach output")?;
                            stdout
                                .flush()
                                .context("failed to flush native attach stdout")?;
                            if let Some(current_viewport) = current_attach_viewport() {
                                render_attach_footer_locked(
                                    &mut stdout,
                                    &mode,
                                    bindings,
                                    &namespace_thread,
                                    &agent_thread,
                                    current_viewport,
                                )?;
                            }
                        }
                    }
                    ServerMessage::Exited { .. } => break,
                    ServerMessage::Error { message } => bail!(message),
                    ServerMessage::Ok | ServerMessage::Metadata(..) => {}
                }
            }
            Ok(())
        })();
        remote_done_thread.store(true, Ordering::SeqCst);
        result
    });

    let attach_result = forward_stdin(
        namespace,
        agent,
        stdin_fd,
        &mut stream,
        &remote_done,
        &render_lock,
        &footer_mode,
        bindings,
        &mut viewport,
    );
    client_detached.store(true, Ordering::SeqCst);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    restore_attach_terminal(&render_lock, viewport).ok();

    match output_thread.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error),
        Err(_) => bail!("native attach output thread panicked"),
    }

    attach_result
}

fn forward_stdin(
    namespace: &str,
    agent: &str,
    stdin_fd: i32,
    stream: &mut UnixStream,
    remote_done: &AtomicBool,
    render_lock: &Arc<Mutex<()>>,
    footer_mode: &Arc<Mutex<FooterMode>>,
    bindings: AttachBindings,
    viewport: &mut Option<AttachViewport>,
) -> anyhow::Result<()> {
    let mut buffer = [0u8; 1024];
    let mut pending_escape = Vec::new();
    let attach_started = std::time::Instant::now();
    let mut resize_retry_offsets_ms = VecDeque::from([75_u64, 200, 500, 1000]);
    let mut poll_fd = libc::pollfd {
        fd: stdin_fd,
        events: libc::POLLIN,
        revents: 0,
    };

    while !remote_done.load(Ordering::SeqCst) {
        poll_fd.revents = 0;
        let poll_status = unsafe { libc::poll(&mut poll_fd, 1, 100) };
        if poll_status < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("failed to poll stdin for native attach");
        }
        refresh_attach_viewport(
            namespace,
            agent,
            stream,
            render_lock,
            footer_mode,
            bindings,
            viewport,
        )?;
        while let Some(next_retry_ms) = resize_retry_offsets_ms.front().copied() {
            if attach_started.elapsed() < Duration::from_millis(next_retry_ms) {
                break;
            }
            resize_retry_offsets_ms.pop_front();
            if let Some(current_viewport) = *viewport {
                let _ = send_resize_message(stream, agent, current_viewport);
            }
        }
        if poll_status == 0 {
            continue;
        }
        if poll_fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            break;
        }
        if poll_fd.revents & libc::POLLIN == 0 {
            continue;
        }

        let read = unsafe { libc::read(stdin_fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("failed to read bytes for native attach");
        }
        let read = read as usize;
        if read == 0 {
            break;
        }
        let mut input = Vec::with_capacity(pending_escape.len() + read);
        if !pending_escape.is_empty() {
            input.extend_from_slice(&pending_escape);
            pending_escape.clear();
        }
        input.extend_from_slice(&buffer[..read]);
        let action = process_attach_input(
            namespace,
            agent,
            stdin_fd,
            stream,
            remote_done,
            render_lock,
            footer_mode,
            bindings,
            &mut pending_escape,
            viewport,
            &input,
        )?;

        if matches!(action, AttachInputAction::Detach) {
            break;
        }
    }

    Ok(())
}

fn enable_native_attach_raw_mode() -> anyhow::Result<NativeAttachRawMode> {
    let fd = io::stdin().as_raw_fd();
    ensure!(
        unsafe { libc::isatty(fd) } == 1,
        "stdin is not a TTY; native attach requires an interactive terminal"
    );

    let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
    ensure!(
        unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } == 0,
        "failed to capture terminal attributes"
    );
    let original = unsafe { original.assume_init() };
    let mut raw = original;
    unsafe {
        libc::cfmakeraw(&mut raw);
    }
    ensure!(
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } == 0,
        "failed to switch terminal into raw mode"
    );

    Ok(NativeAttachRawMode { fd, original })
}

fn current_attach_bindings() -> AttachBindings {
    if let Some(raw) = env::var_os("JARVISCTL_LEADER") {
        if let Some(parsed) = parse_attach_bindings(&raw.to_string_lossy()) {
            return parsed;
        }
    }

    if env::var_os("TMUX").is_some() {
        return AttachBindings {
            leader_byte: 0x07,
            leader_label: "ctrl+g",
            leader_key_label: "g",
            literal_key: b'g',
        };
    }

    AttachBindings {
        leader_byte: 0x02,
        leader_label: "ctrl+b",
        leader_key_label: "b",
        literal_key: b'b',
    }
}

fn parse_attach_bindings(raw: &str) -> Option<AttachBindings> {
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "ctrl+a" | "ctrl-a" | "^a" => Some(AttachBindings {
            leader_byte: 0x01,
            leader_label: "ctrl+a",
            leader_key_label: "a",
            literal_key: b'a',
        }),
        "ctrl+b" | "ctrl-b" | "^b" => Some(AttachBindings {
            leader_byte: 0x02,
            leader_label: "ctrl+b",
            leader_key_label: "b",
            literal_key: b'b',
        }),
        "ctrl+g" | "ctrl-g" | "^g" => Some(AttachBindings {
            leader_byte: 0x07,
            leader_label: "ctrl+g",
            leader_key_label: "g",
            literal_key: b'g',
        }),
        _ => None,
    }
}

fn current_attach_viewport() -> Option<AttachViewport> {
    let (cols, rows) = terminal_size().ok()?;
    let content_rows = rows.saturating_sub(1).max(1);
    Some(AttachViewport {
        cols,
        rows,
        content_rows,
    })
}

fn refresh_attach_viewport(
    namespace: &str,
    agent: &str,
    stream: &mut UnixStream,
    render_lock: &Arc<Mutex<()>>,
    footer_mode: &Arc<Mutex<FooterMode>>,
    bindings: AttachBindings,
    viewport: &mut Option<AttachViewport>,
) -> anyhow::Result<()> {
    let Some(current) = current_attach_viewport() else {
        return Ok(());
    };
    if viewport.as_ref() == Some(&current) {
        return Ok(());
    }

    send_resize_message(stream, agent, current).with_context(|| {
        format!(
            "failed to update attach viewport for native agent '{}:{}'",
            namespace, agent
        )
    })?;
    render_attach_footer(
        render_lock,
        footer_mode,
        bindings,
        namespace,
        agent,
        current,
    )?;
    *viewport = Some(current);
    Ok(())
}

fn send_resize_message(
    stream: &mut UnixStream,
    agent: &str,
    viewport: AttachViewport,
) -> anyhow::Result<()> {
    send_message(
        stream,
        &ClientMessage::Resize {
            agent: agent.to_string(),
            rows: viewport.content_rows,
            cols: viewport.cols,
        },
    )
}

fn render_attach_footer(
    render_lock: &Arc<Mutex<()>>,
    footer_mode: &Arc<Mutex<FooterMode>>,
    bindings: AttachBindings,
    namespace: &str,
    agent: &str,
    viewport: AttachViewport,
) -> anyhow::Result<()> {
    let mode = footer_mode.lock().unwrap().clone();
    let _guard = render_lock.lock().unwrap();
    let mut stdout = io::stdout().lock();
    render_attach_footer_locked(&mut stdout, &mode, bindings, namespace, agent, viewport)
}

fn render_attach_footer_locked(
    stdout: &mut impl Write,
    footer_mode: &FooterMode,
    bindings: AttachBindings,
    namespace: &str,
    agent: &str,
    viewport: AttachViewport,
) -> anyhow::Result<()> {
    if viewport.rows == 0 || viewport.cols == 0 {
        return Ok(());
    }

    let footer_text = format_footer_text(
        footer_mode,
        bindings,
        namespace,
        agent,
        usize::from(viewport.cols),
    );
    write!(
        stdout,
        "\x1b7\x1b[1;{}r\x1b[{};1H\x1b[2K\x1b[48;5;25m\x1b[38;5;255m{}\x1b[0m\x1b8",
        viewport.content_rows, viewport.rows, footer_text
    )
    .context("failed to render native attach footer")?;
    stdout
        .flush()
        .context("failed to flush native attach footer")
}

fn restore_attach_terminal(
    render_lock: &Arc<Mutex<()>>,
    viewport: Option<AttachViewport>,
) -> anyhow::Result<()> {
    let _guard = render_lock.lock().unwrap();
    let mut stdout = io::stdout().lock();
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        queue!(stdout, PopKeyboardEnhancementFlags)
            .context("failed to reset enhanced keyboard mode after native attach")?;
    }
    execute!(
        stdout,
        DisableBracketedPaste,
        DisableFocusChange,
        DisableMouseCapture,
        LeaveAlternateScreen,
        cursor::Show
    )
    .context("failed to reset native attach terminal capabilities")?;
    if let Some(viewport) = viewport {
        write!(
            stdout,
            "\x1b[?1l\x1b>\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b[?2026l\x1b[r\x1b[0m\x1b[{};1H\x1b[2K\r",
            viewport.rows
        )
            .context("failed to restore native attach terminal layout")?;
    } else {
        write!(
            stdout,
            "\x1b[?1l\x1b>\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b[?2026l\x1b[r\x1b[0m\r"
        )
        .context("failed to restore native attach terminal")?;
    }
    stdout
        .flush()
        .context("failed to flush native attach terminal restore")
}

fn format_footer_text(
    footer_mode: &FooterMode,
    bindings: AttachBindings,
    namespace: &str,
    agent: &str,
    width: usize,
) -> String {
    if width == 0 {
        return String::new();
    }

    let content = match footer_mode {
        FooterMode::Normal => format!(
            " native session | ns:{} | ag:{} | {} d detach | {} :cmd | ctrl+c int | ctrl+] or F12 fast-detach ",
            namespace, agent, bindings.leader_label, bindings.leader_label
        ),
        FooterMode::Leader => {
            format!(
                " leader {} | d detach | : command | c interrupt | {} send literal {} ",
                bindings.leader_label, bindings.leader_key_label, bindings.leader_label
            )
        }
        FooterMode::Command(command) => format!(
            " :{}{}  Enter run  Esc cancel  ",
            command,
            if command.is_empty() { "_" } else { "" }
        ),
    };
    fit_footer_text(&content, width)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttachInputAction {
    Continue,
    Detach,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EscapeSequenceAction {
    Leader,
    Detach,
    PassThrough,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EscapeSequenceMatch {
    Complete {
        action: EscapeSequenceAction,
        consumed: usize,
    },
    Incomplete,
    NoMatch,
}

fn process_attach_input(
    namespace: &str,
    agent: &str,
    stdin_fd: i32,
    stream: &mut UnixStream,
    remote_done: &AtomicBool,
    render_lock: &Arc<Mutex<()>>,
    footer_mode: &Arc<Mutex<FooterMode>>,
    bindings: AttachBindings,
    pending_escape: &mut Vec<u8>,
    viewport: &mut Option<AttachViewport>,
    bytes: &[u8],
) -> anyhow::Result<AttachInputAction> {
    let mut passthrough = Vec::with_capacity(bytes.len());
    let mut index = 0usize;

    while index < bytes.len() {
        match bytes[index] {
            byte if byte == bindings.leader_byte => {
                debug!("native attach leader key received");
                flush_attach_passthrough(namespace, agent, stream, &mut passthrough)?;
                let mut pending = LocalKeySource::new(&bytes[index + 1..], stdin_fd);
                let action = run_local_leader(
                    namespace,
                    agent,
                    stream,
                    remote_done,
                    render_lock,
                    footer_mode,
                    bindings,
                    viewport,
                    &mut pending,
                )?;
                debug!(?action, "native attach leader action resolved");
                index += 1 + pending.consumed_pending();
                if matches!(action, AttachInputAction::Detach) {
                    return Ok(action);
                }
                continue;
            }
            ALT_DETACH_BYTE | PRIMARY_DETACH_BYTE => {
                debug!("native attach immediate detach key received");
                flush_attach_passthrough(namespace, agent, stream, &mut passthrough)?;
                update_footer_mode(
                    render_lock,
                    footer_mode,
                    namespace,
                    agent,
                    *viewport,
                    FooterMode::Normal,
                )?;
                return Ok(AttachInputAction::Detach);
            }
            0x1b => match match_local_escape_sequence(&bytes[index..], bindings) {
                EscapeSequenceMatch::Complete { action, consumed } => match action {
                    EscapeSequenceAction::PassThrough => {
                        passthrough.extend_from_slice(&bytes[index..index + consumed]);
                        index += consumed;
                        continue;
                    }
                    EscapeSequenceAction::Leader => {
                        debug!("native attach leader escape sequence received");
                        flush_attach_passthrough(namespace, agent, stream, &mut passthrough)?;
                        let mut pending = LocalKeySource::new(&bytes[index + consumed..], stdin_fd);
                        let action = run_local_leader(
                            namespace,
                            agent,
                            stream,
                            remote_done,
                            render_lock,
                            footer_mode,
                            bindings,
                            viewport,
                            &mut pending,
                        )?;
                        debug!(?action, "native attach leader escape resolved");
                        index += consumed + pending.consumed_pending();
                        if matches!(action, AttachInputAction::Detach) {
                            return Ok(action);
                        }
                        continue;
                    }
                    EscapeSequenceAction::Detach => {
                        debug!("native attach direct escape detach received");
                        flush_attach_passthrough(namespace, agent, stream, &mut passthrough)?;
                        update_footer_mode(
                            render_lock,
                            footer_mode,
                            namespace,
                            agent,
                            *viewport,
                            FooterMode::Normal,
                        )?;
                        return Ok(AttachInputAction::Detach);
                    }
                },
                EscapeSequenceMatch::Incomplete => {
                    pending_escape.extend_from_slice(&bytes[index..]);
                    break;
                }
                EscapeSequenceMatch::NoMatch => {
                    passthrough.push(bytes[index]);
                }
            },
            byte => passthrough.push(byte),
        }
        index += 1;
    }

    flush_attach_passthrough(namespace, agent, stream, &mut passthrough)?;
    Ok(AttachInputAction::Continue)
}

fn flush_attach_passthrough(
    namespace: &str,
    agent: &str,
    stream: &mut UnixStream,
    passthrough: &mut Vec<u8>,
) -> anyhow::Result<()> {
    if passthrough.is_empty() {
        return Ok(());
    }

    let encoded = BASE64.encode(passthrough.as_slice());
    send_message(
        stream,
        &ClientMessage::Input {
            agent: agent.to_string(),
            data_base64: encoded,
        },
    )
    .with_context(|| {
        format!(
            "failed to forward native attach input to '{}:{}'",
            namespace, agent
        )
    })?;
    passthrough.clear();
    Ok(())
}

fn match_local_escape_sequence(bytes: &[u8], bindings: AttachBindings) -> EscapeSequenceMatch {
    if bytes.len() < 2 || bytes[0] != 0x1b || bytes[1] != b'[' {
        return EscapeSequenceMatch::NoMatch;
    }

    match match_csi_u_sequence(bytes, bindings) {
        EscapeSequenceMatch::NoMatch => {}
        other => return other,
    }

    match match_tilde_escape_sequence(bytes) {
        EscapeSequenceMatch::NoMatch => EscapeSequenceMatch::NoMatch,
        other => other,
    }
}

fn match_csi_u_sequence(bytes: &[u8], bindings: AttachBindings) -> EscapeSequenceMatch {
    let mut cursor = 2usize;
    if cursor >= bytes.len() {
        return EscapeSequenceMatch::Incomplete;
    }

    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte == b'u' {
            let body = &bytes[2..cursor];
            let Some(action) = classify_csi_u_action(body, bindings) else {
                return EscapeSequenceMatch::Complete {
                    action: EscapeSequenceAction::PassThrough,
                    consumed: cursor + 1,
                };
            };
            return EscapeSequenceMatch::Complete {
                action,
                consumed: cursor + 1,
            };
        }
        if !(byte.is_ascii_digit() || byte == b';' || byte == b':') {
            return EscapeSequenceMatch::NoMatch;
        }
        cursor += 1;
    }

    EscapeSequenceMatch::Incomplete
}

fn classify_csi_u_action(body: &[u8], bindings: AttachBindings) -> Option<EscapeSequenceAction> {
    let text = std::str::from_utf8(body).ok()?;
    let mut parts = text.split(';');
    let codepoint = parts.next()?.parse::<u32>().ok()?;
    let modifiers_raw = parts.next().unwrap_or("1");
    let modifiers_text = modifiers_raw.split(':').next().unwrap_or("1");
    let modifiers = modifiers_text.parse::<u32>().ok()?;
    let ctrl_active = modifiers > 0 && ((modifiers - 1) & 0b100) != 0;
    if !ctrl_active {
        return None;
    }

    if codepoint == u32::from(bindings.literal_key) {
        return Some(EscapeSequenceAction::Leader);
    }
    if codepoint == 93 || codepoint == 92 {
        return Some(EscapeSequenceAction::Detach);
    }
    None
}

fn match_tilde_escape_sequence(bytes: &[u8]) -> EscapeSequenceMatch {
    let mut cursor = 2usize;
    if cursor >= bytes.len() {
        return EscapeSequenceMatch::Incomplete;
    }

    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte == b'~' {
            let body = &bytes[2..cursor];
            let text = match std::str::from_utf8(body) {
                Ok(text) => text,
                Err(_) => {
                    return EscapeSequenceMatch::Complete {
                        action: EscapeSequenceAction::PassThrough,
                        consumed: cursor + 1,
                    };
                }
            };
            let Some(first) = text.split(';').next() else {
                return EscapeSequenceMatch::Complete {
                    action: EscapeSequenceAction::PassThrough,
                    consumed: cursor + 1,
                };
            };
            let action = if first == "24" {
                EscapeSequenceAction::Detach
            } else {
                EscapeSequenceAction::PassThrough
            };
            return EscapeSequenceMatch::Complete {
                action,
                consumed: cursor + 1,
            };
        }
        if !(byte.is_ascii_digit() || byte == b';') {
            return EscapeSequenceMatch::NoMatch;
        }
        cursor += 1;
    }

    EscapeSequenceMatch::Incomplete
}

struct LocalKeySource<'a> {
    pending: &'a [u8],
    consumed_pending: usize,
    stdin_fd: i32,
}

impl<'a> LocalKeySource<'a> {
    fn new(pending: &'a [u8], stdin_fd: i32) -> Self {
        Self {
            pending,
            consumed_pending: 0,
            stdin_fd,
        }
    }

    fn consumed_pending(&self) -> usize {
        self.consumed_pending
    }

    fn next_byte(
        &mut self,
        remote_done: &AtomicBool,
        namespace: &str,
        agent: &str,
        stream: &mut UnixStream,
        render_lock: &Arc<Mutex<()>>,
        footer_mode: &Arc<Mutex<FooterMode>>,
        bindings: AttachBindings,
        viewport: &mut Option<AttachViewport>,
    ) -> anyhow::Result<Option<u8>> {
        if self.consumed_pending < self.pending.len() {
            let byte = self.pending[self.consumed_pending];
            self.consumed_pending += 1;
            return Ok(Some(byte));
        }

        let mut poll_fd = libc::pollfd {
            fd: self.stdin_fd,
            events: libc::POLLIN,
            revents: 0,
        };

        loop {
            if remote_done.load(Ordering::SeqCst) {
                return Ok(None);
            }
            refresh_attach_viewport(
                namespace,
                agent,
                stream,
                render_lock,
                footer_mode,
                bindings,
                viewport,
            )?;
            poll_fd.revents = 0;
            let poll_status = unsafe { libc::poll(&mut poll_fd, 1, 100) };
            if poll_status < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("failed to poll stdin for native leader input");
            }
            if poll_status == 0 {
                continue;
            }
            if poll_fd.revents & libc::POLLIN == 0 {
                if poll_fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    return Ok(None);
                }
                continue;
            }
            let mut byte = 0u8;
            let read = unsafe { libc::read(self.stdin_fd, (&mut byte as *mut u8).cast(), 1) };
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("failed to read leader byte from stdin");
            }
            if read == 0 {
                return Ok(None);
            }
            return Ok(Some(byte));
        }
    }
}

fn run_local_leader(
    namespace: &str,
    agent: &str,
    stream: &mut UnixStream,
    remote_done: &AtomicBool,
    render_lock: &Arc<Mutex<()>>,
    footer_mode: &Arc<Mutex<FooterMode>>,
    bindings: AttachBindings,
    viewport: &mut Option<AttachViewport>,
    key_source: &mut LocalKeySource<'_>,
) -> anyhow::Result<AttachInputAction> {
    update_footer_mode(
        render_lock,
        footer_mode,
        namespace,
        agent,
        *viewport,
        FooterMode::Leader,
    )?;

    let result = loop {
        let Some(byte) = key_source.next_byte(
            remote_done,
            namespace,
            agent,
            stream,
            render_lock,
            footer_mode,
            bindings,
            viewport,
        )?
        else {
            break AttachInputAction::Continue;
        };
        match byte {
            b'd' | b'D' => {
                debug!("native attach leader detach selected");
                break AttachInputAction::Detach;
            }
            b'c' | b'C' => {
                debug!("native attach leader interrupt selected");
                let _ = interrupt_native(namespace, agent);
                break AttachInputAction::Continue;
            }
            byte if byte.eq_ignore_ascii_case(&bindings.literal_key) => {
                debug!("native attach leader literal selected");
                let mut literal = vec![bindings.leader_byte];
                flush_attach_passthrough(namespace, agent, stream, &mut literal)?;
                break AttachInputAction::Continue;
            }
            b':' => {
                debug!("native attach leader command mode selected");
                break run_local_command(
                    namespace,
                    agent,
                    stream,
                    remote_done,
                    render_lock,
                    footer_mode,
                    bindings,
                    viewport,
                    key_source,
                )?;
            }
            0x1b => break AttachInputAction::Continue,
            _ => break AttachInputAction::Continue,
        }
    };

    update_footer_mode(
        render_lock,
        footer_mode,
        namespace,
        agent,
        *viewport,
        FooterMode::Normal,
    )?;
    Ok(result)
}

fn run_local_command(
    namespace: &str,
    agent: &str,
    stream: &mut UnixStream,
    remote_done: &AtomicBool,
    render_lock: &Arc<Mutex<()>>,
    footer_mode: &Arc<Mutex<FooterMode>>,
    bindings: AttachBindings,
    viewport: &mut Option<AttachViewport>,
    key_source: &mut LocalKeySource<'_>,
) -> anyhow::Result<AttachInputAction> {
    let mut command = String::new();
    update_footer_mode(
        render_lock,
        footer_mode,
        namespace,
        agent,
        *viewport,
        FooterMode::Command(command.clone()),
    )?;

    loop {
        let Some(byte) = key_source.next_byte(
            remote_done,
            namespace,
            agent,
            stream,
            render_lock,
            footer_mode,
            bindings,
            viewport,
        )?
        else {
            return Ok(AttachInputAction::Continue);
        };

        match byte {
            b'\r' | b'\n' => {
                if execute_attach_command(namespace, agent, &command)? {
                    return Ok(AttachInputAction::Detach);
                }
                return Ok(AttachInputAction::Continue);
            }
            0x1b => return Ok(AttachInputAction::Continue),
            0x08 | 0x7f => {
                command.pop();
            }
            value if value.is_ascii_graphic() || value == b' ' => {
                command.push(char::from(value));
            }
            _ => {}
        }

        update_footer_mode(
            render_lock,
            footer_mode,
            namespace,
            agent,
            *viewport,
            FooterMode::Command(command.clone()),
        )?;
    }
}

fn execute_attach_command(namespace: &str, agent: &str, command: &str) -> anyhow::Result<bool> {
    let normalized = command.trim().to_ascii_lowercase();
    debug!(command = %normalized, "native attach command executed");
    match normalized.as_str() {
        "" | "detach" | "d" => Ok(true),
        "interrupt" | "int" | "ctrl-c" => {
            let _ = interrupt_native(namespace, agent);
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn update_footer_mode(
    render_lock: &Arc<Mutex<()>>,
    footer_mode: &Arc<Mutex<FooterMode>>,
    namespace: &str,
    agent: &str,
    viewport: Option<AttachViewport>,
    next_mode: FooterMode,
) -> anyhow::Result<()> {
    let _ = (render_lock, namespace, agent, viewport);
    set_footer_mode(footer_mode, next_mode);
    Ok(())
}

fn set_footer_mode(footer_mode: &Arc<Mutex<FooterMode>>, next_mode: FooterMode) {
    let mut current = footer_mode.lock().unwrap();
    *current = next_mode;
}

fn fit_footer_text(content: &str, width: usize) -> String {
    let mut rendered = String::with_capacity(width);
    for ch in content.chars() {
        if rendered.chars().count() >= width {
            break;
        }
        rendered.push(ch);
    }

    let current_len = rendered.chars().count();
    if current_len < width {
        rendered.push_str(&" ".repeat(width - current_len));
        return rendered;
    }

    if width > 1 {
        let mut compact = rendered.chars().take(width - 1).collect::<String>();
        compact.push('>');
        return compact;
    }

    rendered
}

fn backlog_contains_alt_screen(backlog: &[u8]) -> bool {
    const ALT_SCREEN_MARKERS: [&[u8]; 3] = [b"\x1b[?1049h", b"\x1b[?1047h", b"\x1b[?47h"];
    ALT_SCREEN_MARKERS.iter().any(|marker| {
        backlog
            .windows(marker.len())
            .any(|window| window == *marker)
    })
}

fn handle_client(stream: UnixStream, session: Arc<NativeSession>) -> anyhow::Result<()> {
    let mut writer = stream
        .try_clone()
        .context("failed to clone native client stream")?;
    let mut reader = BufReader::new(stream);
    let request = read_client_message(&mut reader)?;

    match request {
        ClientMessage::Metadata => {
            send_server_message(&mut writer, &ServerMessage::Metadata(session.metadata()))?;
        }
        ClientMessage::SendText {
            agent,
            text,
            press_enter,
        } => {
            session.agent(&agent)?.send_text(&text, press_enter)?;
            send_server_message(&mut writer, &ServerMessage::Ok)?;
        }
        ClientMessage::Interrupt { agent } => {
            session.agent(&agent)?.interrupt()?;
            send_server_message(&mut writer, &ServerMessage::Ok)?;
        }
        ClientMessage::KillSession => {
            send_server_message(&mut writer, &ServerMessage::Ok)?;
            session.shutdown();
        }
        ClientMessage::Attach { agent, rows, cols } => {
            handle_attach(reader, writer, session, agent, rows, cols)?;
        }
        ClientMessage::Input { .. } | ClientMessage::Resize { .. } => {
            bail!(
                "native control connection must start with metadata, attach, send_text, or kill_session"
            )
        }
    }

    Ok(())
}

fn handle_attach(
    reader: BufReader<UnixStream>,
    mut writer: UnixStream,
    session: Arc<NativeSession>,
    agent_name: String,
    rows: Option<u16>,
    cols: Option<u16>,
) -> anyhow::Result<()> {
    let agent = session.agent(&agent_name)?;
    let (backlog, rx) = agent.subscribe();
    let initial_resize = rows.zip(cols);

    if let Some((rows, cols)) = initial_resize {
        let _ = agent.resize(rows, cols);
    }

    send_server_message(
        &mut writer,
        &ServerMessage::Attached {
            namespace: session.namespace.clone(),
            agent: agent_name.clone(),
        },
    )?;
    let alt_screen_backlog = backlog_contains_alt_screen(&backlog);
    if alt_screen_backlog {
        send_output(&mut writer, b"\x1b[2J\x1b[H")?;
    } else if !backlog.is_empty() {
        send_output(&mut writer, &backlog)?;
    }

    let input_stream = reader
        .get_ref()
        .try_clone()
        .context("failed to clone native attach reader stream")?;
    let input_agent = Arc::clone(&agent);
    let input_agent_name = agent_name.clone();
    let input_thread = thread::spawn(move || -> anyhow::Result<()> {
        let mut input_reader = BufReader::new(input_stream);
        loop {
            let message = match read_client_message(&mut input_reader) {
                Ok(message) => message,
                Err(_) => break,
            };
            match message {
                ClientMessage::Input { agent, data_base64 } if agent == input_agent_name => {
                    let bytes = BASE64
                        .decode(data_base64)
                        .context("failed to decode native attach input")?;
                    input_agent.send_input(&bytes)?;
                }
                ClientMessage::Resize { agent, rows, cols } if agent == input_agent_name => {
                    input_agent.resize(rows, cols)?;
                }
                ClientMessage::SendText {
                    agent,
                    text,
                    press_enter,
                } if agent == input_agent_name => {
                    input_agent.send_text(&text, press_enter)?;
                }
                ClientMessage::Attach { .. }
                | ClientMessage::Metadata
                | ClientMessage::KillSession => {
                    break;
                }
                _ => {}
            }
        }
        Ok(())
    });

    if let Some((rows, cols)) = initial_resize {
        let resize_agent = Arc::clone(&agent);
        thread::spawn(move || {
            let attach_started = std::time::Instant::now();
            for target_ms in [75_u64, 200, 500, 1000] {
                let target = Duration::from_millis(target_ms);
                if let Some(remaining) = target.checked_sub(attach_started.elapsed()) {
                    thread::sleep(remaining);
                }
                let _ = resize_agent.resize(rows, cols);
            }
        });
    }

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => send_output(&mut writer, &chunk)?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !agent.is_running() {
                    send_server_message(
                        &mut writer,
                        &ServerMessage::Exited {
                            agent: agent_name.clone(),
                        },
                    )?;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = input_thread.join();
    Ok(())
}

fn request_metadata(namespace: &str) -> anyhow::Result<NativeSessionMetadata> {
    match request(namespace, &ClientMessage::Metadata)? {
        ServerMessage::Metadata(session) => Ok(session),
        ServerMessage::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!(
            "unexpected metadata response for native session '{}': {:?}",
            namespace,
            other
        )),
    }
}

fn request(namespace: &str, message: &ClientMessage) -> anyhow::Result<ServerMessage> {
    let socket_path = socket_path_for(namespace)?;
    let mut stream = UnixStream::connect(&socket_path)
        .with_context(|| format!("failed to connect '{}'", socket_path.display()))?;
    send_message(&mut stream, message)?;
    let mut reader = BufReader::new(stream);
    read_server_message(&mut reader)
}

fn send_message(stream: &mut UnixStream, message: &ClientMessage) -> anyhow::Result<()> {
    let raw = serde_json::to_string(message).context("failed to encode native client message")?;
    stream
        .write_all(raw.as_bytes())
        .context("failed to write native client message")?;
    stream
        .write_all(b"\n")
        .context("failed to terminate native client message")?;
    stream
        .flush()
        .context("failed to flush native client message")
}

fn send_server_message(stream: &mut UnixStream, message: &ServerMessage) -> anyhow::Result<()> {
    let raw = serde_json::to_string(message).context("failed to encode native server message")?;
    stream
        .write_all(raw.as_bytes())
        .context("failed to write native server message")?;
    stream
        .write_all(b"\n")
        .context("failed to terminate native server message")?;
    stream
        .flush()
        .context("failed to flush native server message")
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
            .context("failed to read native client message")?;
        if read == 0 {
            bail!("native client connection closed");
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        return serde_json::from_str(trimmed)
            .with_context(|| format!("failed to decode native client message from '{}'", trimmed));
    }
}

fn read_server_message(reader: &mut impl BufRead) -> anyhow::Result<ServerMessage> {
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .context("failed to read native server message")?;
        if read == 0 {
            bail!("native server connection closed");
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        return serde_json::from_str(trimmed)
            .with_context(|| format!("failed to decode native server message from '{}'", trimmed));
    }
}

fn read_server_message_nonblocking(
    stream: &mut UnixStream,
    line_buffer: &mut Vec<u8>,
) -> anyhow::Result<Option<ServerMessage>> {
    if let Some(message) = drain_server_message_buffer(line_buffer)? {
        return Ok(Some(message));
    }

    let mut chunk = [0u8; 4096];
    match stream.read(&mut chunk) {
        Ok(0) => bail!("native server connection closed"),
        Ok(read) => {
            line_buffer.extend_from_slice(&chunk[..read]);
            drain_server_message_buffer(line_buffer)
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error).context("failed to read native server message"),
    }
}

fn drain_server_message_buffer(line_buffer: &mut Vec<u8>) -> anyhow::Result<Option<ServerMessage>> {
    loop {
        let Some(newline_pos) = line_buffer.iter().position(|byte| *byte == b'\n') else {
            return Ok(None);
        };
        let line = line_buffer.drain(..=newline_pos).collect::<Vec<_>>();
        let trimmed = String::from_utf8_lossy(&line).trim().to_string();
        if trimmed.is_empty() {
            continue;
        }
        return serde_json::from_str(&trimmed)
            .map(Some)
            .with_context(|| format!("failed to decode native server message from '{}'", trimmed));
    }
}

fn wait_for_native_session(namespace: &str) -> anyhow::Result<()> {
    let deadline = SystemTime::now() + Duration::from_secs(3);
    loop {
        if request_metadata(namespace).is_ok() {
            return Ok(());
        }
        if SystemTime::now() >= deadline {
            bail!("native session '{}' did not become ready", namespace);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_native_shutdown(namespace: &str) -> anyhow::Result<()> {
    let deadline = SystemTime::now() + Duration::from_secs(3);
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
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                cleanup_stale_session(namespace)?;
                return Ok(());
            }
            Err(_) => {}
        }
        if SystemTime::now() >= deadline {
            bail!("native session '{}' did not shut down cleanly", namespace);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_attach_context() -> (
        UnixStream,
        Arc<Mutex<()>>,
        Arc<Mutex<FooterMode>>,
        Option<AttachViewport>,
    ) {
        let (left, _right) = UnixStream::pair().expect("unix stream pair");
        (
            left,
            Arc::new(Mutex::new(())),
            Arc::new(Mutex::new(FooterMode::Normal)),
            current_attach_viewport(),
        )
    }

    fn pipe_with_bytes(bytes: &[u8]) -> i32 {
        let mut fds = [0; 2];
        let status = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(status, 0, "pipe");
        if !bytes.is_empty() {
            let written = unsafe { libc::write(fds[1], bytes.as_ptr().cast(), bytes.len()) };
            assert_eq!(written, bytes.len() as isize, "pipe write");
        }
        unsafe {
            libc::close(fds[1]);
        }
        fds[0]
    }

    #[test]
    fn leader_detach_single_buffer() {
        let (mut stream, render_lock, footer_mode, mut viewport) = test_attach_context();
        let stdin_fd = pipe_with_bytes(&[]);
        let remote_done = AtomicBool::new(false);
        let bindings = parse_attach_bindings("ctrl-b").expect("ctrl-b bindings");
        let mut pending_escape = Vec::new();
        let action = process_attach_input(
            "ns",
            "agent0",
            stdin_fd,
            &mut stream,
            &remote_done,
            &render_lock,
            &footer_mode,
            bindings,
            &mut pending_escape,
            &mut viewport,
            &[bindings.leader_byte, b'd'],
        )
        .expect("process attach input");
        unsafe {
            libc::close(stdin_fd);
        }
        assert_eq!(action, AttachInputAction::Detach);
    }

    #[test]
    fn leader_detach_split_buffers() {
        let (mut stream, render_lock, footer_mode, mut viewport) = test_attach_context();
        let stdin_fd = pipe_with_bytes(b"d");
        let remote_done = AtomicBool::new(false);
        let bindings = parse_attach_bindings("ctrl-b").expect("ctrl-b bindings");
        let mut pending_escape = Vec::new();
        let action = process_attach_input(
            "ns",
            "agent0",
            stdin_fd,
            &mut stream,
            &remote_done,
            &render_lock,
            &footer_mode,
            bindings,
            &mut pending_escape,
            &mut viewport,
            &[bindings.leader_byte],
        )
        .expect("process attach input");
        unsafe {
            libc::close(stdin_fd);
        }
        assert_eq!(action, AttachInputAction::Detach);
    }

    #[test]
    fn leader_parser_supports_ctrl_g() {
        let bindings = parse_attach_bindings("ctrl-g").expect("ctrl-g bindings");
        assert_eq!(bindings.leader_byte, 0x07);
        assert_eq!(bindings.leader_label, "ctrl+g");
        assert_eq!(bindings.literal_key, b'g');
    }

    #[test]
    fn csi_u_detach_ctrl_right_bracket() {
        let (mut stream, render_lock, footer_mode, mut viewport) = test_attach_context();
        let stdin_fd = pipe_with_bytes(&[]);
        let remote_done = AtomicBool::new(false);
        let bindings = parse_attach_bindings("ctrl-b").expect("ctrl-b bindings");
        let mut pending_escape = Vec::new();
        let action = process_attach_input(
            "ns",
            "agent0",
            stdin_fd,
            &mut stream,
            &remote_done,
            &render_lock,
            &footer_mode,
            bindings,
            &mut pending_escape,
            &mut viewport,
            b"\x1b[93;5u",
        )
        .expect("process attach input");
        unsafe {
            libc::close(stdin_fd);
        }
        assert_eq!(action, AttachInputAction::Detach);
    }

    #[test]
    fn csi_u_leader_detach_ctrl_b_then_d() {
        let (mut stream, render_lock, footer_mode, mut viewport) = test_attach_context();
        let stdin_fd = pipe_with_bytes(&[]);
        let remote_done = AtomicBool::new(false);
        let bindings = parse_attach_bindings("ctrl-b").expect("ctrl-b bindings");
        let mut pending_escape = Vec::new();
        let action = process_attach_input(
            "ns",
            "agent0",
            stdin_fd,
            &mut stream,
            &remote_done,
            &render_lock,
            &footer_mode,
            bindings,
            &mut pending_escape,
            &mut viewport,
            b"\x1b[98;5ud",
        )
        .expect("process attach input");
        unsafe {
            libc::close(stdin_fd);
        }
        assert_eq!(action, AttachInputAction::Detach);
    }
}

fn cleanup_stale_session(namespace: &str) -> anyhow::Result<()> {
    let session_dir = native_root()?
        .join(SESSION_DIR_NAME)
        .join(sanitize_namespace(namespace));
    let socket_path = session_dir.join(SOCKET_FILE_NAME);
    let metadata_path = session_dir.join(METADATA_FILE_NAME);

    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }
    if metadata_path.exists() {
        let _ = fs::remove_file(&metadata_path);
    }
    if session_dir.exists() {
        let _ = fs::remove_dir(&session_dir);
    }

    Ok(())
}

fn spawn_managed_agent(
    agent_name: &str,
    working_dir: Option<&str>,
    shell_command: &str,
    initial_size: Option<(u16, u16)>,
) -> anyhow::Result<Arc<ManagedAgent>> {
    let pty_system = native_pty_system();
    let (rows, cols) = initial_size.unwrap_or((24, 80));
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to allocate native PTY")?;

    let shell_script = if let Some(dir) = working_dir {
        format!(
            "cd {} && {}",
            shell_words::join(&[dir.to_string()]),
            shell_command
        )
    } else {
        shell_command.to_string()
    };

    let mut command = CommandBuilder::new("bash");
    command.arg("-lc");
    command.arg(shell_script);

    let child = pair
        .slave
        .spawn_command(command)
        .context("failed to spawn native PTY command")?;
    let pid = child.process_id().unwrap_or_default();
    let reader = pair
        .master
        .try_clone_reader()
        .context("failed to clone native PTY reader")?;
    let writer = pair
        .master
        .take_writer()
        .context("failed to take native PTY writer")?;

    let agent = Arc::new(ManagedAgent {
        name: agent_name.to_string(),
        pid,
        running: AtomicBool::new(true),
        master: Mutex::new(pair.master),
        writer: Mutex::new(writer),
        child: Mutex::new(child),
        log: Mutex::new(VecDeque::new()),
        subscribers: Mutex::new(Vec::new()),
    });

    let output_agent = Arc::clone(&agent);
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut buffer = [0u8; 8192];
        loop {
            let read = match reader.read(&mut buffer) {
                Ok(read) => read,
                Err(_) => break,
            };
            if read == 0 {
                break;
            }
            output_agent.append_output(&buffer[..read]);
        }
        output_agent.running.store(false, Ordering::SeqCst);
    });

    Ok(agent)
}

fn native_root() -> anyhow::Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".jarvis").join("native"))
}

fn socket_path_for(namespace: &str) -> anyhow::Result<PathBuf> {
    Ok(native_root()?
        .join(SESSION_DIR_NAME)
        .join(sanitize_namespace(namespace))
        .join(SOCKET_FILE_NAME))
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

fn now_epoch_ms() -> anyhow::Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?
        .as_millis())
}
