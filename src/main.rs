//! jarvisctl: Enterprise-grade orchestrator for CLI/TUI worker apps using a native PTY runtime
//!
//! Features:
//! - Namespaces for isolating agent groups
//! - Agents running your CLI worker inside a native PTY runtime
//! - Inspect: detailed process info (with optional nsenter shell)
//! - Run: spawn new native session with N agents
//! - Attach/Exec: connect to live sessions
//! - Tell: send text into a running agent
//! - Delete/List: manage native sessions

use clap::{Parser, Subcommand, ValueEnum, ValueHint};
use std::{ffi::OsStr, path::PathBuf, process::ExitCode};
use sysinfo::{Pid, System};
use thiserror::Error;
use tracing::{error, info, instrument};

use tracing_subscriber::{EnvFilter, FmtSubscriber};

mod agent;
mod board;
mod codex;
mod dispatch;
mod native;
mod ticket;
mod tui;

use agent::spawn_agent;
use codex::{CodexLaunchOptions, enrich_native_sessions, launch_codex_ticket};
use dispatch::{DispatchOptions, run_dispatch_loop};
use native::{
    NativeSessionMetadata, RuntimeContextMetadata, attach_native, collect_native_sessions,
    delete_native_session, interrupt_native, serve_native_session, spawn_native_session,
    tell_native,
};
use tui::{run_dashboard, view_agent};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum SessionBackend {
    Native,
}

#[derive(Error, Debug)]
pub enum JarvisError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("session runtime returned non-zero exit status: {0}")]
    NonZero(i32),

    #[error("Process {0} not found")]
    ProcessNotFound(u32),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// CLI tool to inspect and control worker sessions
#[derive(Parser, Debug)]
#[command(
    name = "jarvisctl",
    version,
    about = "Orchestrate CLI/TUI workers with a native PTY runtime"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run a single agent in a new TUI window
    Single {
        /// Agent name
        #[arg(long)]
        name: String,

        /// Command and arguments to run
        #[arg(required = true, last = true)]
        command: Vec<String>,
    },
    /// Inspect running processes by name or PID
    Inspect {
        /// Filter by process name
        #[arg(short, long)]
        name: Option<String>,

        /// Filter by PID
        #[arg(short, long)]
        pid: Option<u32>,

        /// Exec into the process namespace via nsenter
        #[arg(long)]
        exec_shell: bool,
    },

    /// Run a worker in a new namespace
    Run {
        /// Deprecated compatibility flag; native is the only backend
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        /// Namespace name
        #[arg(long)]
        namespace: String,

        /// Number of agents
        #[arg(long, default_value_t = 1)]
        agents: usize,

        /// Working directory for each agent
        #[arg(long, value_hint = ValueHint::DirPath)]
        working_directory: Option<String>,

        /// Command and args to run per agent
        #[arg(required = true, last = true, value_hint = ValueHint::CommandString)]
        command: Vec<String>,
    },

    /// Launch an interactive Codex session from a ticket note
    Codex {
        /// Deprecated compatibility flag; native is the only backend
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        /// Ticket or task note with YAML frontmatter
        #[arg(long, value_hint = ValueHint::FilePath)]
        task_note: PathBuf,

        /// Override the runtime namespace
        #[arg(long)]
        namespace: Option<String>,

        /// Number of Codex agents to create
        #[arg(long, default_value_t = 1)]
        agents: usize,

        /// Agent to inject the prompt into
        #[arg(long, default_value = "agent0")]
        agent: String,

        /// Force a new Codex conversation instead of reusing the latest session for this ticket
        #[arg(long, default_value_t = false, conflicts_with = "resume_session_id")]
        fresh: bool,

        /// Explicit Codex session id to resume instead of reusing the latest ticket session
        #[arg(long)]
        resume_session_id: Option<String>,

        /// Override the working directory instead of repo_path from the note
        #[arg(long, value_hint = ValueHint::DirPath)]
        working_directory: Option<PathBuf>,

        /// Use an explicit prompt file instead of rendering from the task note
        #[arg(long, value_hint = ValueHint::FilePath)]
        prompt_file: Option<PathBuf>,

        /// Additional operator message to send with the launch or resume prompt
        #[arg(long)]
        message: Option<String>,

        /// Image(s) to attach to the initial or resumed Codex prompt
        #[arg(long, value_hint = ValueHint::FilePath)]
        image: Vec<PathBuf>,

        /// Wait this long before injecting the prompt
        #[arg(long, default_value_t = 1500)]
        startup_delay_ms: u64,

        /// Codex command override, defaults to `codex`
        #[arg(last = true, value_hint = ValueHint::CommandString)]
        command: Vec<String>,
    },

    /// Watch Obsidian boards and dispatch Codex runs from ticket transitions
    Dispatch {
        /// Deprecated compatibility flag; native is the only backend
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        /// Vault root used to resolve board links and default boards
        #[arg(long, value_hint = ValueHint::DirPath, default_value = "/home/rootster/documents/codex")]
        vault_path: PathBuf,

        /// Board file to scan; may be repeated. Defaults to the dispatch board and project boards in the vault.
        #[arg(long, value_hint = ValueHint::FilePath)]
        board: Vec<PathBuf>,

        /// Scan once and exit instead of looping as a daemon
        #[arg(long, default_value_t = false)]
        once: bool,

        /// Evaluate transitions without launching Codex or writing board/ticket changes
        #[arg(long, default_value_t = false)]
        dry_run: bool,

        /// Polling interval in seconds when not using --once
        #[arg(long, default_value_t = 3)]
        interval_seconds: u64,

        /// Override the dispatch state file
        #[arg(long, value_hint = ValueHint::FilePath)]
        state_file: Option<PathBuf>,

        /// Agent to inject the prompt into
        #[arg(long, default_value = "agent0")]
        agent: String,

        /// Number of Codex agents to create
        #[arg(long, default_value_t = 1)]
        agents: usize,

        /// Wait this long before injecting the prompt
        #[arg(long, default_value_t = 1500)]
        startup_delay_ms: u64,

        /// Codex command override, defaults to `codex`
        #[arg(last = true, value_hint = ValueHint::CommandString)]
        command: Vec<String>,
    },

    /// Open a ratatui session dashboard
    Dashboard {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        #[arg(long, default_value_t = 1000)]
        refresh_ms: u64,
    },

    /// Attach to a running namespace
    Attach {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        #[arg(long)]
        namespace: String,
    },

    /// Kill a namespace
    Delete {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        #[arg(long)]
        namespace: String,
    },

    /// List namespaces and agents
    List {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        #[arg(long)]
        namespace: Option<String>,

        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Attach to a specific agent in a namespace
    Exec {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        #[arg(long)]
        namespace: String,

        #[arg(long)]
        agent: String,
    },

    /// Send file or text to a running agent's TUI
    Tell {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        #[arg(long)]
        namespace: String,
        #[arg(long)]
        agent: String,
        #[arg(long, value_hint = ValueHint::FilePath, conflicts_with = "text")]
        file: Option<String>,
        #[arg(long, conflicts_with = "file")]
        text: Option<String>,
        #[arg(long, default_value_t = false)]
        no_enter: bool,
    },

    /// Send Ctrl+C to a running agent
    Interrupt {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        #[arg(long)]
        namespace: String,

        #[arg(long, default_value = "agent0")]
        agent: String,
    },

    #[command(hide = true)]
    NativeSessionServe {
        #[arg(long, value_hint = ValueHint::FilePath)]
        manifest: PathBuf,
    },
}

#[instrument]
fn main() -> ExitCode {
    // Initialize structured logging with environment override
    let filter = EnvFilter::from_default_env();
    let subscriber = FmtSubscriber::builder()
        .with_env_filter(filter)
        .with_file(true)
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let cli = Cli::parse();

    if let Err(e) = dispatch(cli) {
        error!("{}", e);
        return ExitCode::from(1);
    }
    ExitCode::from(0)
}

fn dispatch(cli: Cli) -> Result<(), JarvisError> {
    match cli.command.unwrap_or(Command::Dashboard {
        backend: SessionBackend::Native,
        refresh_ms: 1000,
    }) {
        Command::Single { name, command } => {
            let agent = spawn_agent(&name, &command).map_err(|e| {
                error!("❌ Failed to spawn agent: {e}");
                JarvisError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e.to_string(),
                ))
            })?;

            view_agent(&agent.name, agent.output.clone()).map_err(|e| {
                error!("❌ Failed to render TUI: {e}");
                JarvisError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e.to_string(),
                ))
            })
        }

        Command::Inspect {
            name,
            pid,
            exec_shell,
        } => inspect(name, pid, exec_shell),

        Command::Run {
            backend,
            namespace,
            agents,
            working_directory,
            command,
        } => run_session(backend, &namespace, agents, &working_directory, &command),
        Command::Codex {
            backend,
            task_note,
            namespace,
            agents,
            agent,
            fresh,
            resume_session_id,
            working_directory,
            prompt_file,
            message,
            image,
            startup_delay_ms,
            command,
        } => launch_and_print_codex(CodexLaunchOptions {
            backend,
            task_note,
            namespace,
            agents,
            agent,
            fresh_session: fresh,
            resume_session_id,
            working_directory,
            prompt_file,
            operator_message: message,
            images: image,
            startup_delay_ms,
            command,
        }),
        Command::Dispatch {
            backend,
            vault_path,
            board,
            once,
            dry_run,
            interval_seconds,
            state_file,
            agent,
            agents,
            startup_delay_ms,
            command,
        } => run_dispatch_loop(DispatchOptions {
            backend,
            vault_path,
            boards: board,
            interval_seconds,
            once,
            dry_run,
            state_file,
            agent,
            agents,
            startup_delay_ms,
            command,
        })
        .map_err(JarvisError::from),
        Command::Dashboard {
            backend,
            refresh_ms,
        } => run_dashboard(backend, refresh_ms).map_err(JarvisError::from),

        Command::Attach { backend, namespace } => attach_session(backend, &namespace),
        Command::Delete { backend, namespace } => delete_session(backend, &namespace),
        Command::List {
            backend,
            namespace,
            json,
        } => list_sessions(backend, namespace, json),
        Command::Exec {
            backend,
            namespace,
            agent,
        } => exec_agent(backend, &namespace, &agent),
        Command::Tell {
            backend,
            namespace,
            agent,
            file,
            text,
            no_enter,
        } => tell(
            backend,
            &namespace,
            &agent,
            file.as_deref(),
            text.as_deref(),
            !no_enter,
        ),
        Command::Interrupt {
            backend,
            namespace,
            agent,
        } => interrupt_agent(backend, &namespace, &agent),
        Command::NativeSessionServe { manifest } => {
            serve_native_session(manifest).map_err(JarvisError::from)
        }
    }
}

#[instrument(err)]
fn inspect(name: Option<String>, pid: Option<u32>, exec_shell: bool) -> Result<(), JarvisError> {
    let mut sys = System::new_all();
    sys.refresh_all();

    match (name, pid) {
        (Some(name), _) => {
            let procs: Vec<_> = sys.processes_by_name(OsStr::new(&name)).collect();
            if procs.is_empty() {
                return Err(JarvisError::ProcessNotFound(0));
            }
            for p in procs {
                print_process_info(p);
                if exec_shell {
                    return enter_shell(p.pid().as_u32());
                }
            }
        }
        (None, Some(pid_u32)) => {
            let pid = Pid::from(pid_u32 as usize);
            if let Some(p) = sys.process(pid) {
                print_process_info(p);
                if exec_shell {
                    return enter_shell(p.pid().as_u32());
                }
            } else {
                return Err(JarvisError::ProcessNotFound(pid_u32));
            }
        }
        _ => {
            println!("⚠️ Provide either --name or --pid (see --help).");
        }
    }
    Ok(())
}

#[instrument(err)]
fn run_session(
    backend: SessionBackend,
    namespace: &str,
    agents: usize,
    working_dir: &Option<String>,
    cmd: &[String],
) -> Result<(), JarvisError> {
    let joined = shell_words::join(cmd);
    run_session_shell(backend, namespace, agents, working_dir, &joined)
}

pub(crate) fn run_session_shell(
    backend: SessionBackend,
    namespace: &str,
    agents: usize,
    working_dir: &Option<String>,
    joined: &str,
) -> Result<(), JarvisError> {
    run_session_shell_with_context(backend, namespace, agents, working_dir, joined, None)
}

pub(crate) fn run_session_shell_with_context(
    backend: SessionBackend,
    namespace: &str,
    agents: usize,
    working_dir: &Option<String>,
    joined: &str,
    context: Option<RuntimeContextMetadata>,
) -> Result<(), JarvisError> {
    let _ = backend;
    spawn_native_session(namespace, agents, working_dir.as_deref(), joined, context)
        .map_err(JarvisError::from)?;

    println!(
        "✅ Started {} agent(s) in '{}' using the native runtime. Attach: jarvisctl attach --namespace {}",
        agents, namespace, namespace
    );
    info!(
        "Started native session '{}' with {} agent(s)",
        namespace, agents
    );
    Ok(())
}

#[instrument(err)]
fn list_sessions(
    backend: SessionBackend,
    namespace: Option<String>,
    json: bool,
) -> Result<(), JarvisError> {
    let _ = backend;
    let mut sessions = collect_native_sessions().map_err(JarvisError::from)?;
    enrich_native_sessions(&mut sessions).map_err(JarvisError::from)?;

    if let Some(namespace) = namespace.as_deref() {
        sessions.retain(|session| session.namespace == namespace);
        if sessions.is_empty() {
            return Err(JarvisError::Other(anyhow::anyhow!(
                "native session '{}' does not exist",
                namespace
            )));
        }
    }

    if json {
        if namespace.is_some() {
            println!(
                "{}",
                serde_json::to_string_pretty(&sessions[0]).map_err(anyhow::Error::from)?
            );
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&sessions).map_err(anyhow::Error::from)?
            );
        }
        return Ok(());
    }

    print_runtime_sessions(&sessions);
    Ok(())
}

fn print_runtime_sessions(sessions: &[NativeSessionMetadata]) {
    if sessions.is_empty() {
        println!("NAMESPACES:\n(none)");
        println!("AGENTS:\n(none)");
        return;
    }

    println!("NAMESPACES:");
    for session in sessions {
        let mut summary = format!(
            "{}: {} agents (created {}) [native]",
            session.namespace,
            session.agents.len(),
            session.created_at_epoch_ms
        );
        if let Some(context) = session.context.as_ref() {
            if let Some(task_title) = context.task_title.as_deref() {
                summary.push_str(&format!(" -> {}", task_title));
            } else if let Some(task_note) = context.task_note.as_deref() {
                summary.push_str(&format!(" -> {}", task_note));
            }
        }
        println!("{}", summary);
    }

    println!("\nAGENTS:");
    for session in sessions {
        for agent in &session.agents {
            let mut summary = format!(
                "{} {} pid={} running={}",
                session.namespace, agent.name, agent.pid, agent.running
            );
            if let Some(context) = session.context.as_ref() {
                if let Some(session_id) = context.codex_session_id.as_deref() {
                    summary.push_str(&format!(" session={}", session_id));
                }
            }
            println!("{}", summary);
        }
    }
}

#[instrument(err)]
fn exec_agent(backend: SessionBackend, namespace: &str, agent: &str) -> Result<(), JarvisError> {
    let _ = backend;
    attach_native(namespace, agent).map_err(JarvisError::from)
}

#[instrument(err)]
fn tell(
    backend: SessionBackend,
    namespace: &str,
    agent: &str,
    file: Option<&str>,
    text: Option<&str>,
    press_enter: bool,
) -> Result<(), JarvisError> {
    let contents = match (file, text.map(str::trim).filter(|value| !value.is_empty())) {
        (Some(file), None) => std::fs::read_to_string(file)?,
        (None, Some(text)) => text.to_string(),
        (Some(_), Some(_)) => {
            return Err(JarvisError::Other(anyhow::anyhow!(
                "--file and --text cannot be used together"
            )));
        }
        (None, None) => {
            return Err(JarvisError::Other(anyhow::anyhow!(
                "provide either --file or --text to tell"
            )));
        }
    };
    let _ = backend;
    tell_native(namespace, agent, &contents, press_enter).map_err(JarvisError::from)?;

    if let Some(file) = file {
        println!(
            "✅ Sent '{}' to '{}':'{}' via the native runtime",
            file, namespace, agent
        );
    } else {
        println!(
            "✅ Sent text to '{}':'{}' via the native runtime",
            namespace, agent
        );
    }
    Ok(())
}

fn launch_and_print_codex(options: CodexLaunchOptions) -> Result<(), JarvisError> {
    let record = launch_codex_ticket(options)?;

    println!("✅ Codex session launched.");
    println!("   Namespace:   {}", record.namespace);
    println!("   Agent:       {}", record.agent);
    println!("   Runtime:     native");
    println!("   Repo:        {}", record.repo_path);
    println!("   Task note:   {}", record.task_note);
    println!("   Launch mode: {}", record.launch_mode);
    if let Some(codex_session_id) = &record.codex_session_id {
        println!("   Session ID:  {}", codex_session_id);
    }
    println!("   Finish:      {}", record.finish_mode);
    println!("   Prompt:      {}", record.prompt_file);
    println!("   Record:      {}", record.record_file);

    if !record.readiness_warnings.is_empty() {
        println!("\nWarnings:");
        for warning in &record.readiness_warnings {
            println!(" - {}", warning);
        }
    }

    println!(
        "\nAttach with: jarvisctl attach --namespace {}",
        record.namespace
    );
    Ok(())
}

#[instrument(err)]
fn attach_session(backend: SessionBackend, namespace: &str) -> Result<(), JarvisError> {
    let _ = backend;
    attach_native(namespace, "agent0").map_err(JarvisError::from)
}

#[instrument(err)]
fn delete_session(backend: SessionBackend, namespace: &str) -> Result<(), JarvisError> {
    let _ = backend;
    delete_native_session(namespace).map_err(JarvisError::from)
}

#[instrument(err)]
fn interrupt_agent(
    backend: SessionBackend,
    namespace: &str,
    agent: &str,
) -> Result<(), JarvisError> {
    let _ = backend;
    interrupt_native(namespace, agent).map_err(JarvisError::from)
}

fn enter_shell(target_pid: u32) -> Result<(), JarvisError> {
    let shell = if std::path::Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "/bin/sh"
    };
    let pid_str = target_pid.to_string();
    let status = std::process::Command::new("sudo")
        .args(["nsenter", "-t", &pid_str, "-a", shell])
        .status()?;
    let code = status.code().unwrap_or(1);
    std::process::exit(code);
}

fn print_process_info(p: &sysinfo::Process) {
    println!("PID:             {}", p.pid());
    println!("Name:            {}", p.name().to_string_lossy());
    println!("Status:          {:?}", p.status());
    println!("CPU:             {:.2}%", p.cpu_usage());
    println!("Memory RSS:      {} KB", p.memory());
    println!("Virtual Mem:     {} KB", p.virtual_memory());
    println!("Start (epoch):   {}", p.start_time());
    println!("Run time (sec):  {}", p.run_time());
    // println!("Exe path:        {}", p.exe().unwrap("no display"));
    println!("Cmd line:        {:?}", p.cmd());
    println!("Parent PID:      {:?}", p.parent());
    println!("------------------------------------");
}
