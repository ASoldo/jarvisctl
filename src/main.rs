//! jarvisctl: Enterprise-grade orchestrator for CLI/TUI worker apps using tmux
//!
//! Features:
//! - Namespaces (tmux sessions) for isolating agent groups
//! - Agents (tmux windows) running your CLI worker
//! - Inspect: detailed process info (with optional nsenter shell)
//! - Run: spawn new tmux session with N agents
//! - Attach/Exec: connect to sessions/windows
//! - Tell: paste file/text into a running agent
//! - Delete/List: manage tmux sessions/windows

use clap::{Parser, Subcommand, ValueHint};
use std::{ffi::OsStr, process::ExitCode};
use sysinfo::{Pid, System};
use thiserror::Error;
use tracing::{error, info, instrument};

use tracing_subscriber::{EnvFilter, FmtSubscriber};

#[derive(Error, Debug)]
pub enum JarvisError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("TMUX returned non-zero exit status: {0}")]
    NonZero(i32),

    #[error("Process {0} not found")]
    ProcessNotFound(u32),
}

/// CLI tool to inspect and control worker sessions
#[derive(Parser, Debug)]
#[command(
    name = "jarvisctl",
    version,
    about = "Orchestrate CLI/TUI workers with tmux"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
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

    /// Run a worker in a new tmux namespace
    Run {
        /// Namespace (tmux session) name
        #[arg(long)]
        namespace: String,

        /// Number of agents (windows)
        #[arg(long, default_value_t = 1)]
        agents: usize,

        /// Working directory for each agent
        #[arg(long, value_hint = ValueHint::DirPath)]
        working_directory: Option<String>,

        /// Command and args to run per agent
        #[arg(required = true, last = true, value_hint = ValueHint::CommandString)]
        command: Vec<String>,
    },

    /// Attach to a running namespace
    Attach {
        #[arg(long)]
        namespace: String,
    },

    /// Kill a tmux namespace
    Delete {
        #[arg(long)]
        namespace: String,
    },

    /// List tmux sessions and windows
    List {
        #[arg(long)]
        namespace: Option<String>,
    },

    /// Attach to a specific agent in a namespace
    Exec {
        #[arg(long)]
        namespace: String,

        #[arg(long)]
        agent: String,
    },

    /// Send file or text to a running agent's TUI
    Tell {
        #[arg(long)]
        namespace: String,
        #[arg(long)]
        agent: String,
        #[arg(long, value_hint = ValueHint::FilePath)]
        file: String,
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
    match cli.command {
        Command::Inspect {
            name,
            pid,
            exec_shell,
        } => inspect(name, pid, exec_shell),

        Command::Run {
            namespace,
            agents,
            working_directory,
            command,
        } => run_session(&namespace, agents, &working_directory, &command),

        Command::Attach { namespace } => run_tmux(&["attach", "-t", &namespace]),
        Command::Delete { namespace } => run_tmux(&["kill-session", "-t", &namespace]),
        Command::List { namespace } => list_sessions(namespace),
        Command::Exec { namespace, agent } => exec_agent(&namespace, &agent),
        Command::Tell {
            namespace,
            agent,
            file,
        } => tell(&namespace, &agent, &file),
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
    namespace: &str,
    agents: usize,
    working_dir: &Option<String>,
    cmd: &[String],
) -> Result<(), JarvisError> {
    let joined = shell_words::join(cmd);

    for i in 0..agents {
        let window = format!("agent{}", i);
        let full_command = if let Some(dir) = working_dir {
            format!("bash -lc 'cd {} && {}'", dir, joined)
        } else {
            format!("bash -lc '{}'", joined)
        };

        let args = if i == 0 {
            vec![
                "new-session",
                "-d",
                "-s",
                namespace,
                "-n",
                &window,
                &full_command,
            ]
        } else {
            vec!["new-window", "-t", namespace, "-n", &window, &full_command]
        };

        run_tmux(&args)?;

        if i == 0 {
            run_tmux(&["set-option", "-t", namespace, "@jarvisctl", "1"])?;
        }

        info!("Started window {} in {}", window, namespace);
    }

    println!(
        "✅ Started {} agent(s) in '{}'. Attach: jarvisctl attach --namespace {}",
        agents, namespace, namespace
    );

    Ok(())
}

#[instrument(err)]
fn list_sessions(namespace: Option<String>) -> Result<(), JarvisError> {
    if let Some(ns) = namespace {
        let out = capture_tmux(&["list-windows", "-t", &ns])?;
        println!("Windows in '{}':\n{}", ns, out);
    } else {
        // Filter only sessions that are marked with @jarvisctl=1
        let all_sessions_output = capture_tmux(&["list-sessions", "-F", "#{session_name}"])?;
        let mut valid_sessions = vec![];
        for line in all_sessions_output.lines() {
            let session_name = line.trim();
            let marker = capture_tmux(&["show-option", "-qv", "-t", session_name, "@jarvisctl"])?;
            if marker.trim() == "1" {
                valid_sessions.push(session_name.to_string());
            }
        }

        if valid_sessions.is_empty() {
            println!("NAMESPACES:\n(none)");
            println!("AGENTS:\n(none)");
            return Ok(());
        }

        println!("NAMESPACES:");
        for session in &valid_sessions {
            let info = capture_tmux(&[
                "display-message",
                "-p",
                "-t",
                session,
                "#{session_name}: #{session_windows} windows (created #{session_created})",
            ])?;
            println!("{}", info.trim());
        }

        println!("\nAGENTS:");
        for session in &valid_sessions {
            let windows = capture_tmux(&["list-windows", "-t", session])?;
            print!("{}", windows);
        }
    }

    Ok(())
}

#[instrument(err)]
fn exec_agent(namespace: &str, agent: &str) -> Result<(), JarvisError> {
    run_tmux(&["select-window", "-t", &format!("{}:{}", namespace, agent)])?;
    run_tmux(&["attach", "-t", namespace])?;
    Ok(())
}

#[instrument(err)]
fn tell(namespace: &str, agent: &str, file: &str) -> Result<(), JarvisError> {
    let contents = std::fs::read_to_string(file)?;
    let session_target = format!("{}:{}", namespace, agent);

    // Send each line followed by Ctrl+J (line feed)
    for line in contents.lines() {
        run_tmux(&["send-keys", "-t", &session_target, line, "C-j"])?;
    }

    // After sending all lines, press Enter to submit
    run_tmux(&["send-keys", "-t", &session_target, "Enter"])?;

    println!("✅ Sent '{}' to '{}':'{}'", file, namespace, agent);
    Ok(())
}

// Helpers
fn run_tmux(args: &[&str]) -> Result<(), JarvisError> {
    let status = std::process::Command::new("tmux").args(args).status()?;
    let code = status.code().unwrap_or(-1);
    if code != 0 {
        return Err(JarvisError::NonZero(code));
    }
    Ok(())
}

fn capture_tmux(args: &[&str]) -> Result<String, JarvisError> {
    let out = std::process::Command::new("tmux").args(args).output()?;
    let code = out.status.code().unwrap_or(-1);
    if code != 0 {
        return Err(JarvisError::NonZero(code));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
