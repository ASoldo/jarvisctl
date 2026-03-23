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
use std::{
    env,
    ffi::OsStr,
    fs,
    net::TcpStream,
    path::PathBuf,
    process::{Child, Command as ProcessCommand, ExitCode, Stdio},
    thread,
    time::Duration,
};
use sysinfo::{Pid, System};
use thiserror::Error;
use tracing::{error, info, instrument};

use tracing_subscriber::{EnvFilter, FmtSubscriber};

mod agent;
mod board;
mod codex;
mod codex_app;
mod control_plane;
mod dispatch;
mod native;
mod ticket;
mod tui;

use agent::spawn_agent;
use codex::{CodexLaunchOptions, CodexRuntimeDriver, enrich_native_sessions, launch_codex_ticket};
use codex_app::{
    CodexAppInputMode, attach_codex_app, attach_codex_app_tcp, codex_app_session_metadata,
    codex_app_session_metadata_tcp, collect_codex_app_sessions, delete_codex_app_session,
    interrupt_codex_app, interrupt_codex_app_tcp, request_worker_offload_for_current_runtime,
    request_worker_offload_via_runtime_namespace, serve_codex_app_session,
    tell_codex_app_with_mode, tell_codex_app_with_mode_tcp,
};
use control_plane::{
    ControlPlaneOutput, ControlPlaneResourceKindArg, KubernetesRenderOutput, WorkerOffloadRequest,
    apply_kubernetes_resources, apply_kustomization, apply_manifests, authorize_runtime_message,
    invoke_worker, offload_worker_task, pause_deployment_rollout, render_application_diff_output,
    render_describe_output, render_get_output, render_kubernetes_resources,
    render_rollout_history_output, render_rollout_status_output, resolve_service_target,
    resolve_service_target_for_message, restart_deployment_rollout, resume_deployment_rollout,
    serve_worker_run, sync_application_resource, undo_deployment_rollout,
    wait_for_rollout_status_output,
};
use dispatch::{DispatchOptions, run_dispatch_loop};
use native::{
    NativeSessionMetadata, RuntimeContextMetadata, attach_native, collect_native_sessions,
    delete_native_session, interrupt_native, serve_native_session, spawn_native_session,
    tell_native,
};
use ticket::slugify;
use tui::{run_dashboard, view_agent};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum SessionBackend {
    Native,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum WorkerOffloadOutput {
    Text,
    Json,
    Yaml,
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

        /// Codex runtime driver, defaults to the headless app-server backend
        #[arg(long, value_enum, default_value_t = CodexRuntimeDriver::AppServer)]
        driver: CodexRuntimeDriver,

        /// Ticket or task note with YAML frontmatter
        #[arg(long, value_hint = ValueHint::FilePath)]
        task_note: PathBuf,

        /// Override the runtime namespace
        #[arg(long)]
        namespace: Option<String>,

        /// Bind the launched Codex runtime to a control-plane namespace
        #[arg(long = "control-namespace")]
        control_namespace: Option<String>,

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

        /// Codex runtime driver used when dispatch launches work
        #[arg(long, value_enum, default_value_t = CodexRuntimeDriver::AppServer)]
        driver: CodexRuntimeDriver,

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
        #[arg(long, default_value_t = 15)]
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

    /// Apply declarative control-plane resources from YAML manifests
    Apply {
        #[arg(short = 'f', long = "file", value_hint = ValueHint::FilePath)]
        file: Vec<PathBuf>,

        #[arg(short = 'k', long = "kustomize", value_hint = ValueHint::DirPath)]
        kustomize: Option<PathBuf>,
    },

    /// Get declarative control-plane resources
    Get {
        kind: ControlPlaneResourceKindArg,

        #[arg(short = 'n', long = "resource-namespace")]
        resource_namespace: Option<String>,

        #[arg(long, value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Describe a declarative control-plane resource
    Describe {
        kind: ControlPlaneResourceKindArg,
        name: String,

        #[arg(short = 'n', long = "resource-namespace")]
        resource_namespace: Option<String>,

        #[arg(long, value_enum, default_value_t = ControlPlaneOutput::Yaml)]
        output: ControlPlaneOutput,
    },

    /// Inspect or trigger Deployment rollouts
    Rollout {
        #[command(subcommand)]
        command: RolloutCommand,
    },

    /// Inspect or trigger Application sync operations
    #[command(alias = "app")]
    Application {
        #[command(subcommand)]
        command: ApplicationCommand,
    },

    /// Invoke a namespaced worker resource
    Worker {
        #[command(subcommand)]
        command: WorkerCommand,
    },

    /// Render or apply supported resources onto a Kubernetes cluster
    Kube {
        #[command(subcommand)]
        command: KubeCommand,
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

        #[arg(long, required_unless_present = "service", conflicts_with = "service")]
        namespace: Option<String>,

        #[arg(
            long,
            required_unless_present = "namespace",
            conflicts_with = "namespace"
        )]
        service: Option<String>,

        #[arg(short = 'n', long = "resource-namespace", requires = "service")]
        resource_namespace: Option<String>,
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

        #[arg(long, required_unless_present = "service", conflicts_with = "service")]
        namespace: Option<String>,

        #[arg(
            long,
            required_unless_present = "namespace",
            conflicts_with = "namespace"
        )]
        service: Option<String>,

        #[arg(short = 'n', long = "resource-namespace", requires = "service")]
        resource_namespace: Option<String>,

        #[arg(long, default_value = "agent0")]
        agent: String,
    },

    /// Send file or text to a running agent's TUI
    Tell {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        #[arg(long, required_unless_present = "service", conflicts_with = "service")]
        namespace: Option<String>,
        #[arg(
            long,
            required_unless_present = "namespace",
            conflicts_with = "namespace"
        )]
        service: Option<String>,
        #[arg(short = 'n', long = "resource-namespace", requires = "service")]
        resource_namespace: Option<String>,
        #[arg(long, default_value = "agent0")]
        agent: String,
        #[arg(long, value_hint = ValueHint::FilePath, conflicts_with = "text")]
        file: Option<String>,
        #[arg(long, conflicts_with = "file")]
        text: Option<String>,
        #[arg(long, default_value_t = false)]
        no_enter: bool,
        #[arg(long, value_enum, default_value_t = CodexAppInputMode::Auto)]
        mode: CodexAppInputMode,
    },

    /// Send Ctrl+C to a running agent
    Interrupt {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        #[arg(long, required_unless_present = "service", conflicts_with = "service")]
        namespace: Option<String>,

        #[arg(
            long,
            required_unless_present = "namespace",
            conflicts_with = "namespace"
        )]
        service: Option<String>,

        #[arg(short = 'n', long = "resource-namespace", requires = "service")]
        resource_namespace: Option<String>,

        #[arg(long, default_value = "agent0")]
        agent: String,
    },

    #[command(hide = true)]
    NativeSessionServe {
        #[arg(long, value_hint = ValueHint::FilePath)]
        manifest: PathBuf,
    },

    #[command(hide = true)]
    CodexAppSessionServe {
        #[arg(long, value_hint = ValueHint::FilePath)]
        manifest: PathBuf,
    },

    #[command(hide = true)]
    WorkerRunServe {
        #[arg(long, value_hint = ValueHint::FilePath)]
        manifest: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
enum RolloutCommand {
    /// Show rollout status for a Deployment
    Status {
        deployment: String,

        #[arg(short = 'n', long = "resource-namespace")]
        resource_namespace: Option<String>,

        #[arg(long, default_value_t = false)]
        watch: bool,

        #[arg(long = "timeout-seconds", default_value_t = 300)]
        timeout_seconds: u64,

        #[arg(long, value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Show rollout history for a Deployment
    History {
        deployment: String,

        #[arg(short = 'n', long = "resource-namespace")]
        resource_namespace: Option<String>,

        #[arg(long, value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Trigger a rollout restart for a Deployment
    Restart {
        deployment: String,

        #[arg(short = 'n', long = "resource-namespace")]
        resource_namespace: Option<String>,
    },

    /// Pause a Deployment rollout
    Pause {
        deployment: String,

        #[arg(short = 'n', long = "resource-namespace")]
        resource_namespace: Option<String>,
    },

    /// Resume a paused Deployment rollout
    Resume {
        deployment: String,

        #[arg(short = 'n', long = "resource-namespace")]
        resource_namespace: Option<String>,
    },

    /// Roll a Deployment back to a prior revision
    Undo {
        deployment: String,

        #[arg(short = 'n', long = "resource-namespace")]
        resource_namespace: Option<String>,

        #[arg(long = "to-revision")]
        to_revision: Option<u64>,
    },
}

#[derive(Subcommand, Debug)]
enum ApplicationCommand {
    /// Force an Application sync, even if automated sync is disabled
    Sync {
        application: String,

        #[arg(short = 'n', long = "resource-namespace")]
        resource_namespace: Option<String>,
    },

    /// Show the diff between desired Application source and live managed resources
    Diff {
        application: String,

        #[arg(short = 'n', long = "resource-namespace")]
        resource_namespace: Option<String>,

        #[arg(long, value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },
}

#[derive(Subcommand, Debug)]
enum WorkerCommand {
    /// Invoke a worker with a prompt or prompt file
    Invoke {
        worker: String,

        #[arg(short = 'n', long = "resource-namespace")]
        resource_namespace: Option<String>,

        #[arg(long, conflicts_with = "file")]
        prompt: Option<String>,

        #[arg(long, value_hint = ValueHint::FilePath, conflicts_with = "prompt")]
        file: Option<PathBuf>,
    },

    /// Submit a worker-backed offload job through a worker Service and wait for the result
    Offload {
        #[arg(long)]
        service: String,

        #[arg(short = 'n', long = "resource-namespace")]
        resource_namespace: Option<String>,

        #[arg(long = "via-runtime-namespace")]
        via_runtime_namespace: Option<String>,

        #[arg(long, conflicts_with = "file")]
        prompt: Option<String>,

        #[arg(long, value_hint = ValueHint::FilePath, conflicts_with = "prompt")]
        file: Option<PathBuf>,

        #[arg(long)]
        intent: Option<String>,

        #[arg(long = "job-name")]
        job_name: Option<String>,

        #[arg(long = "timeout-seconds", default_value_t = 180)]
        timeout_seconds: u64,

        #[arg(long = "output-path", value_hint = ValueHint::FilePath)]
        output_path: Option<PathBuf>,

        #[arg(long, value_enum, default_value_t = WorkerOffloadOutput::Text)]
        output: WorkerOffloadOutput,
    },
}

#[derive(Subcommand, Debug)]
enum KubeCommand {
    /// Render supported jarvisctl resources as native Kubernetes manifests
    Render {
        #[arg(short = 'f', long = "file", value_hint = ValueHint::FilePath)]
        file: Vec<PathBuf>,

        #[arg(short = 'k', long = "kustomize", value_hint = ValueHint::DirPath)]
        kustomize: Option<PathBuf>,

        #[arg(long, value_enum, default_value_t = KubernetesRenderOutput::Yaml)]
        output: KubernetesRenderOutput,
    },

    /// Apply supported jarvisctl resources onto the active Kubernetes cluster
    Apply {
        #[arg(short = 'f', long = "file", value_hint = ValueHint::FilePath)]
        file: Vec<PathBuf>,

        #[arg(short = 'k', long = "kustomize", value_hint = ValueHint::DirPath)]
        kustomize: Option<PathBuf>,

        #[arg(long)]
        context: Option<String>,

        #[arg(long = "dry-run-server", default_value_t = false)]
        dry_run_server: bool,
    },

    /// Control a pod-hosted Codex runtime exposed through Kubernetes
    Runtime {
        #[command(subcommand)]
        command: KubeRuntimeCommand,
    },
}

#[derive(Subcommand, Debug)]
enum KubeRuntimeCommand {
    /// Fetch live metadata from a pod-hosted Codex runtime
    Metadata {
        #[arg(long, required_unless_present = "service", conflicts_with = "service")]
        deployment: Option<String>,

        #[arg(
            long,
            required_unless_present = "deployment",
            conflicts_with = "deployment"
        )]
        service: Option<String>,

        #[arg(short = 'n', long = "resource-namespace")]
        resource_namespace: String,

        #[arg(long)]
        context: Option<String>,

        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Attach to the live output stream of a pod-hosted Codex runtime
    Attach {
        #[arg(long, required_unless_present = "service", conflicts_with = "service")]
        deployment: Option<String>,

        #[arg(
            long,
            required_unless_present = "deployment",
            conflicts_with = "deployment"
        )]
        service: Option<String>,

        #[arg(short = 'n', long = "resource-namespace")]
        resource_namespace: String,

        #[arg(long)]
        context: Option<String>,
    },

    /// Send text into a pod-hosted Codex runtime
    Tell {
        #[arg(long, required_unless_present = "service", conflicts_with = "service")]
        deployment: Option<String>,

        #[arg(
            long,
            required_unless_present = "deployment",
            conflicts_with = "deployment"
        )]
        service: Option<String>,

        #[arg(short = 'n', long = "resource-namespace")]
        resource_namespace: String,

        #[arg(long)]
        context: Option<String>,

        #[arg(long, value_hint = ValueHint::FilePath, conflicts_with = "text")]
        file: Option<String>,

        #[arg(long, conflicts_with = "file")]
        text: Option<String>,

        #[arg(long, value_enum, default_value_t = CodexAppInputMode::Auto)]
        mode: CodexAppInputMode,
    },

    /// Interrupt the active turn inside a pod-hosted Codex runtime
    Interrupt {
        #[arg(long, required_unless_present = "service", conflicts_with = "service")]
        deployment: Option<String>,

        #[arg(
            long,
            required_unless_present = "deployment",
            conflicts_with = "deployment"
        )]
        service: Option<String>,

        #[arg(short = 'n', long = "resource-namespace")]
        resource_namespace: String,

        #[arg(long)]
        context: Option<String>,
    },

    /// Delete a pod-hosted Codex runtime Deployment and its launch ConfigMap
    Delete {
        #[arg(long)]
        deployment: String,

        #[arg(short = 'n', long = "resource-namespace")]
        resource_namespace: String,

        #[arg(long)]
        context: Option<String>,
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
            driver,
            task_note,
            namespace,
            control_namespace,
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
            driver,
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
            environment: Default::default(),
            context_overlay: RuntimeContextMetadata {
                control_namespace,
                ..RuntimeContextMetadata::default()
            },
            extra_runtime_args: Vec::new(),
            startup_delay_ms,
            command,
        }),
        Command::Dispatch {
            backend,
            driver,
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
            driver,
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
        Command::Apply { file, kustomize } => apply_resources(&file, kustomize.as_deref()),
        Command::Get {
            kind,
            resource_namespace,
            output,
        } => get_resources(kind, resource_namespace.as_deref(), output),
        Command::Describe {
            kind,
            name,
            resource_namespace,
            output,
        } => describe_resource(kind, &name, resource_namespace.as_deref(), output),
        Command::Rollout { command } => rollout_command(command),
        Command::Application { command } => application_command(command),
        Command::Worker { command } => worker_command(command),
        Command::Kube { command } => kube_command(command),
        Command::Dashboard {
            backend,
            refresh_ms,
        } => run_dashboard(backend, refresh_ms).map_err(JarvisError::from),

        Command::Attach {
            backend,
            namespace,
            service,
            resource_namespace,
        } => attach_session(
            backend,
            resolve_runtime_namespace(
                namespace.as_deref(),
                service.as_deref(),
                resource_namespace.as_deref(),
            )?
            .as_str(),
        ),
        Command::Delete { backend, namespace } => delete_session(backend, &namespace),
        Command::List {
            backend,
            namespace,
            json,
        } => list_sessions(backend, namespace, json),
        Command::Exec {
            backend,
            namespace,
            service,
            resource_namespace,
            agent,
        } => {
            let namespace = resolve_runtime_namespace(
                namespace.as_deref(),
                service.as_deref(),
                resource_namespace.as_deref(),
            )?;
            exec_agent(backend, &namespace, &agent)
        }
        Command::Tell {
            backend,
            namespace,
            service,
            resource_namespace,
            agent,
            file,
            text,
            no_enter,
            mode,
        } => {
            let namespace = resolve_tell_runtime_namespace(
                namespace.as_deref(),
                service.as_deref(),
                resource_namespace.as_deref(),
            )?;
            tell(
                backend,
                &namespace,
                &agent,
                file.as_deref(),
                text.as_deref(),
                !no_enter,
                mode,
            )
        }
        Command::Interrupt {
            backend,
            namespace,
            service,
            resource_namespace,
            agent,
        } => {
            let namespace = resolve_runtime_namespace(
                namespace.as_deref(),
                service.as_deref(),
                resource_namespace.as_deref(),
            )?;
            interrupt_agent(backend, &namespace, &agent)
        }
        Command::NativeSessionServe { manifest } => {
            serve_native_session(manifest).map_err(JarvisError::from)
        }
        Command::CodexAppSessionServe { manifest } => {
            serve_codex_app_session(manifest).map_err(JarvisError::from)
        }
        Command::WorkerRunServe { manifest } => {
            serve_worker_run(manifest).map_err(JarvisError::from)
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

fn apply_resources(
    files: &[PathBuf],
    kustomize: Option<&std::path::Path>,
) -> Result<(), JarvisError> {
    let messages = match (files.is_empty(), kustomize) {
        (false, None) => apply_manifests(files).map_err(JarvisError::from)?,
        (true, Some(path)) => apply_kustomization(path).map_err(JarvisError::from)?,
        (false, Some(_)) => {
            return Err(JarvisError::Other(anyhow::anyhow!(
                "use either --file or --kustomize, not both"
            )));
        }
        (true, None) => {
            return Err(JarvisError::Other(anyhow::anyhow!(
                "provide at least one --file or one --kustomize path"
            )));
        }
    };
    for message in messages {
        println!("{}", message);
    }
    Ok(())
}

fn get_resources(
    kind: ControlPlaneResourceKindArg,
    resource_namespace: Option<&str>,
    output: ControlPlaneOutput,
) -> Result<(), JarvisError> {
    println!(
        "{}",
        render_get_output(kind, resource_namespace, output).map_err(JarvisError::from)?
    );
    Ok(())
}

fn describe_resource(
    kind: ControlPlaneResourceKindArg,
    name: &str,
    resource_namespace: Option<&str>,
    output: ControlPlaneOutput,
) -> Result<(), JarvisError> {
    println!(
        "{}",
        render_describe_output(kind, name, resource_namespace, output)
            .map_err(JarvisError::from)?
    );
    Ok(())
}

fn rollout_command(command: RolloutCommand) -> Result<(), JarvisError> {
    match command {
        RolloutCommand::Status {
            deployment,
            resource_namespace,
            watch,
            timeout_seconds,
            output,
        } => {
            let rendered = if watch {
                wait_for_rollout_status_output(
                    &deployment,
                    resource_namespace.as_deref(),
                    output,
                    Duration::from_secs(timeout_seconds),
                )
            } else {
                render_rollout_status_output(&deployment, resource_namespace.as_deref(), output)
            }
            .map_err(JarvisError::from)?;
            println!("{}", rendered);
            Ok(())
        }
        RolloutCommand::History {
            deployment,
            resource_namespace,
            output,
        } => {
            println!(
                "{}",
                render_rollout_history_output(&deployment, resource_namespace.as_deref(), output)
                    .map_err(JarvisError::from)?
            );
            Ok(())
        }
        RolloutCommand::Restart {
            deployment,
            resource_namespace,
        } => {
            println!(
                "{}",
                restart_deployment_rollout(&deployment, resource_namespace.as_deref())
                    .map_err(JarvisError::from)?
            );
            Ok(())
        }
        RolloutCommand::Pause {
            deployment,
            resource_namespace,
        } => {
            println!(
                "{}",
                pause_deployment_rollout(&deployment, resource_namespace.as_deref())
                    .map_err(JarvisError::from)?
            );
            Ok(())
        }
        RolloutCommand::Resume {
            deployment,
            resource_namespace,
        } => {
            println!(
                "{}",
                resume_deployment_rollout(&deployment, resource_namespace.as_deref())
                    .map_err(JarvisError::from)?
            );
            Ok(())
        }
        RolloutCommand::Undo {
            deployment,
            resource_namespace,
            to_revision,
        } => {
            println!(
                "{}",
                undo_deployment_rollout(&deployment, resource_namespace.as_deref(), to_revision)
                    .map_err(JarvisError::from)?
            );
            Ok(())
        }
    }
}

fn application_command(command: ApplicationCommand) -> Result<(), JarvisError> {
    match command {
        ApplicationCommand::Sync {
            application,
            resource_namespace,
        } => {
            println!(
                "{}",
                sync_application_resource(&application, resource_namespace.as_deref())
                    .map_err(JarvisError::from)?
            );
            Ok(())
        }
        ApplicationCommand::Diff {
            application,
            resource_namespace,
            output,
        } => {
            println!(
                "{}",
                render_application_diff_output(&application, resource_namespace.as_deref(), output)
                    .map_err(JarvisError::from)?
            );
            Ok(())
        }
    }
}

fn worker_command(command: WorkerCommand) -> Result<(), JarvisError> {
    match command {
        WorkerCommand::Invoke {
            worker,
            resource_namespace,
            prompt,
            file,
        } => {
            let prompt = read_worker_prompt(prompt.as_deref(), file.as_deref())?;
            println!(
                "{}",
                invoke_worker(&worker, resource_namespace.as_deref(), &prompt)
                    .map_err(JarvisError::from)?
            );
            Ok(())
        }
        WorkerCommand::Offload {
            service,
            resource_namespace,
            via_runtime_namespace,
            prompt,
            file,
            intent,
            job_name,
            timeout_seconds,
            output_path,
            output,
        } => {
            let prompt = read_worker_prompt(prompt.as_deref(), file.as_deref())?;
            let request_payload = WorkerOffloadRequest {
                service_name: service,
                resource_namespace,
                prompt,
                intent,
                timeout_seconds: Some(timeout_seconds),
                output_path: output_path.map(|path| path.display().to_string()),
                job_name,
            };
            let result = if let Some(runtime_namespace) = via_runtime_namespace {
                request_worker_offload_via_runtime_namespace(&runtime_namespace, request_payload)
                    .map_err(JarvisError::from)?
            } else {
                request_worker_offload_for_current_runtime(request_payload.clone())
                    .map_err(JarvisError::from)?
                    .map_or_else(
                        || offload_worker_task(request_payload).map_err(JarvisError::from),
                        Ok,
                    )?
            };

            match output {
                WorkerOffloadOutput::Text => {
                    if let Some(response) = result.response.as_deref() {
                        print!("{}", response);
                        if !response.ends_with('\n') {
                            println!();
                        }
                    } else {
                        println!(
                            "worker offload completed: {}/{} via {}",
                            result.namespace, result.job_name, result.service_name
                        );
                    }
                }
                WorkerOffloadOutput::Json => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&result)
                            .map_err(anyhow::Error::from)
                            .map_err(JarvisError::from)?
                    );
                }
                WorkerOffloadOutput::Yaml => {
                    println!(
                        "{}",
                        serde_yaml::to_string(&result)
                            .map_err(anyhow::Error::from)
                            .map_err(JarvisError::from)?
                    );
                }
            }
            Ok(())
        }
    }
}

fn kube_command(command: KubeCommand) -> Result<(), JarvisError> {
    match command {
        KubeCommand::Render {
            file,
            kustomize,
            output,
        } => {
            println!(
                "{}",
                render_kubernetes_resources(&file, kustomize.as_deref(), output)
                    .map_err(JarvisError::from)?
            );
            Ok(())
        }
        KubeCommand::Apply {
            file,
            kustomize,
            context,
            dry_run_server,
        } => {
            println!(
                "{}",
                apply_kubernetes_resources(
                    &file,
                    kustomize.as_deref(),
                    context.as_deref(),
                    dry_run_server,
                )
                .map_err(JarvisError::from)?
            );
            Ok(())
        }
        KubeCommand::Runtime { command } => kube_runtime_command(command),
    }
}

struct KubePortForward {
    child: Child,
    local_port: u16,
}

struct KubeRuntimeTarget {
    resource_kind: &'static str,
    resource_name: String,
    remote_port: u16,
}

impl Drop for KubePortForward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn kube_runtime_command(command: KubeRuntimeCommand) -> Result<(), JarvisError> {
    match command {
        KubeRuntimeCommand::Metadata {
            deployment,
            service,
            resource_namespace,
            context,
            json,
        } => {
            let forward = start_kube_runtime_port_forward(
                deployment.as_deref(),
                service.as_deref(),
                &resource_namespace,
                context.as_deref(),
            )?;
            let metadata = codex_app_session_metadata_tcp("127.0.0.1", forward.local_port)
                .map_err(JarvisError::from)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&metadata).map_err(anyhow::Error::from)?
                );
            } else {
                println!(
                    "{}",
                    serde_yaml::to_string(&metadata).map_err(anyhow::Error::from)?
                );
            }
            Ok(())
        }
        KubeRuntimeCommand::Attach {
            deployment,
            service,
            resource_namespace,
            context,
        } => {
            let target = kube_runtime_target_label(deployment.as_deref(), service.as_deref())?;
            let forward = start_kube_runtime_port_forward(
                deployment.as_deref(),
                service.as_deref(),
                &resource_namespace,
                context.as_deref(),
            )?;
            attach_codex_app_tcp("127.0.0.1", forward.local_port, &target)
                .map_err(JarvisError::from)
        }
        KubeRuntimeCommand::Tell {
            deployment,
            service,
            resource_namespace,
            context,
            file,
            text,
            mode,
        } => {
            let contents = match (text.as_deref(), file.as_deref()) {
                (Some(text), None) => text.to_string(),
                (None, Some(path)) => fs::read_to_string(path)?,
                _ => {
                    return Err(JarvisError::Other(anyhow::anyhow!(
                        "provide either --text or --file for kube runtime tell"
                    )));
                }
            };
            let forward = start_kube_runtime_port_forward(
                deployment.as_deref(),
                service.as_deref(),
                &resource_namespace,
                context.as_deref(),
            )?;
            tell_codex_app_with_mode_tcp("127.0.0.1", forward.local_port, &contents, mode)
                .map_err(JarvisError::from)
        }
        KubeRuntimeCommand::Interrupt {
            deployment,
            service,
            resource_namespace,
            context,
        } => {
            let forward = start_kube_runtime_port_forward(
                deployment.as_deref(),
                service.as_deref(),
                &resource_namespace,
                context.as_deref(),
            )?;
            interrupt_codex_app_tcp("127.0.0.1", forward.local_port).map_err(JarvisError::from)
        }
        KubeRuntimeCommand::Delete {
            deployment,
            resource_namespace,
            context,
        } => kube_runtime_delete(&deployment, &resource_namespace, context.as_deref()),
    }
}

fn kube_runtime_target_label(
    deployment: Option<&str>,
    service: Option<&str>,
) -> Result<String, JarvisError> {
    match (deployment, service) {
        (Some(deployment), None) => Ok(deployment.to_string()),
        (None, Some(service)) => Ok(service.to_string()),
        _ => Err(JarvisError::Other(anyhow::anyhow!(
            "provide either --deployment or --service"
        ))),
    }
}

fn start_kube_runtime_port_forward(
    deployment: Option<&str>,
    service: Option<&str>,
    resource_namespace: &str,
    context: Option<&str>,
) -> Result<KubePortForward, JarvisError> {
    let target = resolve_kube_runtime_target(deployment, service, resource_namespace, context)?;
    if target.resource_name.is_empty() {
        return Err(JarvisError::Other(anyhow::anyhow!(
            "runtime target name must not be empty"
        )));
    }

    let local_port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(JarvisError::from)?
        .local_addr()
        .map_err(JarvisError::from)?
        .port();

    let mut command = ProcessCommand::new("kubectl");
    if let Some(context) = context.map(str::trim).filter(|value| !value.is_empty()) {
        command.arg("--context").arg(context);
    }
    command
        .arg("-n")
        .arg(resource_namespace)
        .arg("port-forward")
        .arg(format!("{}/{}", target.resource_kind, target.resource_name))
        .arg(format!("{local_port}:{}", target.remote_port))
        .arg("--address")
        .arg("127.0.0.1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(JarvisError::from)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if TcpStream::connect(("127.0.0.1", local_port)).is_ok()
            && codex_app_session_metadata_tcp("127.0.0.1", local_port).is_ok()
        {
            return Ok(KubePortForward { child, local_port });
        }
        if let Some(status) = child.try_wait().map_err(JarvisError::from)? {
            let stderr = child
                .stderr
                .take()
                .map(|mut stream| {
                    use std::io::Read as _;
                    let mut buffer = String::new();
                    let _ = stream.read_to_string(&mut buffer);
                    buffer
                })
                .unwrap_or_default();
            return Err(JarvisError::Other(anyhow::anyhow!(
                "kubectl port-forward for {}/{} exited with status {status}: {}",
                target.resource_kind,
                target.resource_name,
                stderr.trim()
            )));
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let stderr = child
                .stderr
                .take()
                .map(|mut stream| {
                    use std::io::Read as _;
                    let mut buffer = String::new();
                    let _ = stream.read_to_string(&mut buffer);
                    buffer
                })
                .unwrap_or_default();
            return Err(JarvisError::Other(anyhow::anyhow!(
                "timed out waiting for Kubernetes runtime {}/{} on port {}: {}",
                target.resource_kind,
                target.resource_name,
                target.remote_port,
                stderr.trim()
            )));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn resolve_kube_runtime_target(
    deployment: Option<&str>,
    service: Option<&str>,
    resource_namespace: &str,
    context: Option<&str>,
) -> Result<KubeRuntimeTarget, JarvisError> {
    let (resource_kind, resource_name) = match (deployment, service) {
        (Some(deployment), None) => ("deployment", deployment.trim()),
        (None, Some(service)) => ("service", service.trim()),
        _ => {
            return Err(JarvisError::Other(anyhow::anyhow!(
                "provide either --deployment or --service"
            )));
        }
    };
    if resource_name.is_empty() {
        return Err(JarvisError::Other(anyhow::anyhow!(
            "runtime target name must not be empty"
        )));
    }

    let manifest = kubectl_get_json(resource_kind, resource_name, resource_namespace, context)?;
    let remote_port =
        kube_runtime_control_port_from_manifest(resource_kind, resource_name, &manifest)?;
    Ok(KubeRuntimeTarget {
        resource_kind,
        resource_name: resource_name.to_string(),
        remote_port,
    })
}

fn kubectl_get_json(
    resource_kind: &str,
    resource_name: &str,
    resource_namespace: &str,
    context: Option<&str>,
) -> Result<serde_json::Value, JarvisError> {
    let mut command = ProcessCommand::new("kubectl");
    if let Some(context) = context.map(str::trim).filter(|value| !value.is_empty()) {
        command.arg("--context").arg(context);
    }
    let output = command
        .arg("-n")
        .arg(resource_namespace)
        .arg("get")
        .arg(resource_kind)
        .arg(resource_name)
        .arg("-o")
        .arg("json")
        .output()
        .map_err(JarvisError::from)?;
    if !output.status.success() {
        return Err(JarvisError::Other(anyhow::anyhow!(
            "kubectl get {resource_kind}/{resource_name} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(anyhow::Error::from)
        .map_err(JarvisError::from)
}

fn kube_runtime_control_port_from_manifest(
    resource_kind: &str,
    resource_name: &str,
    manifest: &serde_json::Value,
) -> Result<u16, JarvisError> {
    let candidates = match resource_kind {
        "service" => manifest
            .get("spec")
            .and_then(|value| value.get("ports"))
            .and_then(serde_json::Value::as_array)
            .map(|ports| {
                ports
                    .iter()
                    .filter_map(|port| {
                        let port_number = port
                            .get("port")
                            .and_then(serde_json::Value::as_u64)
                            .and_then(|value| u16::try_from(value).ok())?;
                        Some((
                            port.get("name")
                                .and_then(serde_json::Value::as_str)
                                .map(ToOwned::to_owned),
                            port_number,
                        ))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        "deployment" => manifest
            .get("spec")
            .and_then(|value| value.get("template"))
            .and_then(|value| value.get("spec"))
            .and_then(|value| value.get("containers"))
            .and_then(serde_json::Value::as_array)
            .map(|containers| {
                containers
                    .iter()
                    .flat_map(|container| {
                        container
                            .get("ports")
                            .and_then(serde_json::Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(|port| {
                                let port_number = port
                                    .get("containerPort")
                                    .and_then(serde_json::Value::as_u64)
                                    .and_then(|value| u16::try_from(value).ok())?;
                                Some((
                                    port.get("name")
                                        .and_then(serde_json::Value::as_str)
                                        .map(ToOwned::to_owned),
                                    port_number,
                                ))
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        other => {
            return Err(JarvisError::Other(anyhow::anyhow!(
                "unsupported Kubernetes runtime resource kind '{other}'"
            )));
        }
    };

    select_kube_runtime_control_port(resource_kind, resource_name, &candidates)
}

fn select_kube_runtime_control_port(
    resource_kind: &str,
    resource_name: &str,
    candidates: &[(Option<String>, u16)],
) -> Result<u16, JarvisError> {
    if candidates.is_empty() {
        return Err(JarvisError::Other(anyhow::anyhow!(
            "Kubernetes runtime {resource_kind}/{resource_name} does not expose any ports"
        )));
    }

    let named_control = candidates
        .iter()
        .filter_map(|(name, port)| match name.as_deref() {
            Some("control") => Some(*port),
            _ => None,
        })
        .collect::<Vec<_>>();
    if let Some(port) = named_control.first().copied() {
        if named_control.iter().all(|candidate| *candidate == port) {
            return Ok(port);
        }
        return Err(JarvisError::Other(anyhow::anyhow!(
            "Kubernetes runtime {resource_kind}/{resource_name} exposes multiple distinct 'control' ports: {:?}",
            named_control
        )));
    }

    let unique_ports = candidates
        .iter()
        .map(|(_, port)| *port)
        .collect::<std::collections::BTreeSet<_>>();
    if unique_ports.len() == 1 {
        return Ok(*unique_ports.iter().next().expect("unique port"));
    }

    Err(JarvisError::Other(anyhow::anyhow!(
        "Kubernetes runtime {resource_kind}/{resource_name} exposes multiple ports without a unique 'control' port: {:?}",
        candidates
    )))
}

fn kube_runtime_delete(
    deployment: &str,
    resource_namespace: &str,
    context: Option<&str>,
) -> Result<(), JarvisError> {
    let deployment_manifest =
        kubectl_get_json("deployment", deployment, resource_namespace, context).ok();
    let runtime_service_names = deployment_manifest
        .as_ref()
        .map(|manifest| kube_runtime_matching_service_names(manifest, context))
        .transpose()?
        .unwrap_or_default();

    let mut command = ProcessCommand::new("kubectl");
    if let Some(context) = context.map(str::trim).filter(|value| !value.is_empty()) {
        command.arg("--context").arg(context);
    }
    let output = command
        .arg("-n")
        .arg(resource_namespace)
        .arg("delete")
        .arg("deployment")
        .arg(deployment)
        .arg("--ignore-not-found=true")
        .output()
        .map_err(JarvisError::from)?;
    if !output.status.success() {
        return Err(JarvisError::Other(anyhow::anyhow!(
            "kubectl delete deployment failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let launch_config_map = format!("{}-codex-launch", slugify(deployment));
    let mut config_map_command = ProcessCommand::new("kubectl");
    if let Some(context) = context.map(str::trim).filter(|value| !value.is_empty()) {
        config_map_command.arg("--context").arg(context);
    }
    let _ = config_map_command
        .arg("-n")
        .arg(resource_namespace)
        .arg("delete")
        .arg("configmap")
        .arg(launch_config_map)
        .arg("--ignore-not-found=true")
        .output();

    for service_name in &runtime_service_names {
        let mut service_command = ProcessCommand::new("kubectl");
        if let Some(context) = context.map(str::trim).filter(|value| !value.is_empty()) {
            service_command.arg("--context").arg(context);
        }
        let _ = service_command
            .arg("-n")
            .arg(resource_namespace)
            .arg("delete")
            .arg("service")
            .arg(service_name)
            .arg("--ignore-not-found=true")
            .output();
    }

    println!(
        "deleted Kubernetes runtime deployment {}/{}{}",
        resource_namespace,
        deployment,
        if runtime_service_names.is_empty() {
            String::new()
        } else {
            format!(" and {} runtime service(s)", runtime_service_names.len())
        }
    );
    Ok(())
}

fn kube_runtime_matching_service_names(
    deployment_manifest: &serde_json::Value,
    context: Option<&str>,
) -> Result<Vec<String>, JarvisError> {
    let resource_namespace = deployment_manifest
        .get("metadata")
        .and_then(|value| value.get("namespace"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            JarvisError::Other(anyhow::anyhow!("deployment manifest missing namespace"))
        })?;
    let deployment_name = deployment_manifest
        .get("metadata")
        .and_then(|value| value.get("name"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| JarvisError::Other(anyhow::anyhow!("deployment manifest missing name")))?;
    let labels = deployment_manifest
        .get("spec")
        .and_then(|value| value.get("template"))
        .and_then(|value| value.get("metadata"))
        .and_then(|value| value.get("labels"))
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            JarvisError::Other(anyhow::anyhow!(
                "deployment {resource_namespace}/{deployment_name} is missing pod template labels"
            ))
        })?;
    let services = kubectl_list_json("service", resource_namespace, context)?;
    let items = services
        .get("items")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            JarvisError::Other(anyhow::anyhow!("kubectl get service returned no items"))
        })?;

    let mut matches = Vec::new();
    for item in items {
        let metadata = item.get("metadata").unwrap_or(&serde_json::Value::Null);
        if metadata
            .get("labels")
            .and_then(|value| value.get("jarvisctl.io/runtime-deployment"))
            .and_then(serde_json::Value::as_str)
            == Some(deployment_name)
        {
            if let Some(name) = metadata.get("name").and_then(serde_json::Value::as_str) {
                matches.push(name.to_string());
            }
            continue;
        }

        let Some(selector) = item
            .get("spec")
            .and_then(|value| value.get("selector"))
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        if selector.is_empty() {
            continue;
        }
        let selector_matches = selector.iter().all(|(key, value)| {
            labels.get(key).and_then(serde_json::Value::as_str) == value.as_str()
        });
        if selector_matches {
            if let Some(name) = metadata.get("name").and_then(serde_json::Value::as_str) {
                matches.push(name.to_string());
            }
        }
    }

    matches.sort();
    matches.dedup();
    Ok(matches)
}

fn kubectl_list_json(
    resource_kind: &str,
    resource_namespace: &str,
    context: Option<&str>,
) -> Result<serde_json::Value, JarvisError> {
    let mut command = ProcessCommand::new("kubectl");
    if let Some(context) = context.map(str::trim).filter(|value| !value.is_empty()) {
        command.arg("--context").arg(context);
    }
    let output = command
        .arg("-n")
        .arg(resource_namespace)
        .arg("get")
        .arg(resource_kind)
        .arg("-o")
        .arg("json")
        .output()
        .map_err(JarvisError::from)?;
    if !output.status.success() {
        return Err(JarvisError::Other(anyhow::anyhow!(
            "kubectl get {resource_kind} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(anyhow::Error::from)
        .map_err(JarvisError::from)
}

fn read_worker_prompt(
    prompt: Option<&str>,
    file: Option<&std::path::Path>,
) -> Result<String, JarvisError> {
    match (prompt, file) {
        (Some(prompt), None) => Ok(prompt.to_string()),
        (None, Some(path)) => Ok(fs::read_to_string(path)?),
        _ => Err(JarvisError::Other(anyhow::anyhow!(
            "provide either --prompt or --file for worker invoke"
        ))),
    }
}

fn resolve_runtime_namespace(
    namespace: Option<&str>,
    service: Option<&str>,
    resource_namespace: Option<&str>,
) -> Result<String, JarvisError> {
    let effective_resource_namespace = infer_resource_namespace(resource_namespace);
    match (namespace, service) {
        (Some(namespace), None) => Ok(namespace.to_string()),
        (None, Some(service)) => Ok(resolve_service_target(
            service,
            effective_resource_namespace.as_deref(),
        )
        .map_err(JarvisError::from)?
        .runtime_namespace),
        _ => Err(JarvisError::Other(anyhow::anyhow!(
            "provide either --namespace or --service"
        ))),
    }
}

fn resolve_tell_runtime_namespace(
    namespace: Option<&str>,
    service: Option<&str>,
    resource_namespace: Option<&str>,
) -> Result<String, JarvisError> {
    let source_runtime_namespace = current_runtime_namespace_from_env();
    let effective_resource_namespace = infer_resource_namespace(resource_namespace);

    match (namespace, service) {
        (Some(namespace), None) => {
            authorize_runtime_message(source_runtime_namespace.as_deref(), namespace)
                .map_err(JarvisError::from)?;
            Ok(namespace.to_string())
        }
        (None, Some(service)) => Ok(resolve_service_target_for_message(
            service,
            effective_resource_namespace.as_deref(),
            source_runtime_namespace.as_deref(),
        )
        .map_err(JarvisError::from)?
        .runtime_namespace),
        _ => Err(JarvisError::Other(anyhow::anyhow!(
            "provide either --namespace or --service"
        ))),
    }
}

fn infer_resource_namespace(resource_namespace: Option<&str>) -> Option<String> {
    resource_namespace
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| env::var("JARVIS_CONTROL_NAMESPACE").ok())
}

fn current_runtime_namespace_from_env() -> Option<String> {
    env::var("JARVIS_RUNTIME_NAMESPACE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn collect_runtime_sessions() -> Result<Vec<NativeSessionMetadata>, JarvisError> {
    let mut sessions = collect_native_sessions().map_err(JarvisError::from)?;
    sessions.extend(collect_codex_app_sessions().map_err(JarvisError::from)?);
    enrich_native_sessions(&mut sessions).map_err(JarvisError::from)?;
    sessions.sort_by(|left, right| left.namespace.cmp(&right.namespace));
    Ok(sessions)
}

fn session_metadata_for_namespace(namespace: &str) -> Result<NativeSessionMetadata, JarvisError> {
    if let Some(session) = codex_app_session_metadata(namespace).map_err(JarvisError::from)? {
        return Ok(session);
    }
    if let Some(session) = native::native_session_metadata(namespace).map_err(JarvisError::from)? {
        return Ok(session);
    }
    Err(JarvisError::Other(anyhow::anyhow!(
        "runtime session '{}' does not exist",
        namespace
    )))
}

#[instrument(err)]
fn list_sessions(
    backend: SessionBackend,
    namespace: Option<String>,
    json: bool,
) -> Result<(), JarvisError> {
    let _ = backend;
    let mut sessions = collect_runtime_sessions()?;

    if let Some(namespace) = namespace.as_deref() {
        sessions.retain(|session| session.namespace == namespace);
        if sessions.is_empty() {
            return Err(JarvisError::Other(anyhow::anyhow!(
                "runtime session '{}' does not exist",
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
            "{}: {} agents (created {}) [{}]",
            session.namespace,
            session.agents.len(),
            session.created_at_epoch_ms,
            session.backend
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
    let session = session_metadata_for_namespace(namespace)?;
    match session.backend.as_str() {
        "codex-app" => attach_codex_app(namespace).map_err(JarvisError::from),
        _ => attach_native(namespace, agent).map_err(JarvisError::from),
    }
}

#[instrument(err)]
fn tell(
    backend: SessionBackend,
    namespace: &str,
    agent: &str,
    file: Option<&str>,
    text: Option<&str>,
    press_enter: bool,
    mode: CodexAppInputMode,
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
    let session = session_metadata_for_namespace(namespace)?;
    match session.backend.as_str() {
        "codex-app" => {
            if !press_enter {
                return Err(JarvisError::Other(anyhow::anyhow!(
                    "--no-enter is not supported for codex app sessions"
                )));
            }
            if agent != "agent0" {
                return Err(JarvisError::Other(anyhow::anyhow!(
                    "codex app sessions expose a single logical agent named agent0"
                )));
            }
            tell_codex_app_with_mode(namespace, &contents, mode).map_err(JarvisError::from)?;
        }
        _ => {
            tell_native(namespace, agent, &contents, press_enter).map_err(JarvisError::from)?;
        }
    }

    if let Some(file) = file {
        println!("✅ Sent '{}' to '{}':'{}'", file, namespace, agent);
    } else {
        println!("✅ Sent text to '{}':'{}'", namespace, agent);
    }
    Ok(())
}

fn launch_and_print_codex(options: CodexLaunchOptions) -> Result<(), JarvisError> {
    let record = launch_codex_ticket(options)?;

    println!("✅ Codex session launched.");
    println!("   Namespace:   {}", record.namespace);
    println!("   Agent:       {}", record.agent);
    println!("   Runtime:     {}", record.runtime_backend);
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
    let session = session_metadata_for_namespace(namespace)?;
    match session.backend.as_str() {
        "codex-app" => attach_codex_app(namespace).map_err(JarvisError::from),
        _ => attach_native(namespace, "agent0").map_err(JarvisError::from),
    }
}

#[instrument(err)]
fn delete_session(backend: SessionBackend, namespace: &str) -> Result<(), JarvisError> {
    let _ = backend;
    let session = session_metadata_for_namespace(namespace)?;
    match session.backend.as_str() {
        "codex-app" => delete_codex_app_session(namespace).map_err(JarvisError::from),
        _ => delete_native_session(namespace).map_err(JarvisError::from),
    }
}

#[instrument(err)]
fn interrupt_agent(
    backend: SessionBackend,
    namespace: &str,
    agent: &str,
) -> Result<(), JarvisError> {
    let _ = backend;
    let session = session_metadata_for_namespace(namespace)?;
    match session.backend.as_str() {
        "codex-app" => {
            if agent != "agent0" {
                return Err(JarvisError::Other(anyhow::anyhow!(
                    "codex app sessions expose a single logical agent named agent0"
                )));
            }
            interrupt_codex_app(namespace).map_err(JarvisError::from)
        }
        _ => interrupt_native(namespace, agent).map_err(JarvisError::from),
    }
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

#[cfg(test)]
mod tests {
    use super::select_kube_runtime_control_port;

    #[test]
    fn selects_named_control_port_first() {
        let candidates = vec![
            (Some("metrics".to_string()), 8080),
            (Some("control".to_string()), 47999),
        ];
        let port = select_kube_runtime_control_port("service", "runtime-svc", &candidates).unwrap();
        assert_eq!(port, 47999);
    }

    #[test]
    fn selects_single_unique_port_without_name() {
        let candidates = vec![(None, 47832), (Some("secondary".to_string()), 47832)];
        let port =
            select_kube_runtime_control_port("deployment", "codex-runtime", &candidates).unwrap();
        assert_eq!(port, 47832);
    }

    #[test]
    fn rejects_ambiguous_ports_without_control_name() {
        let candidates = vec![
            (Some("first".to_string()), 47832),
            (Some("second".to_string()), 47999),
        ];
        let error =
            select_kube_runtime_control_port("service", "runtime-svc", &candidates).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("multiple ports without a unique 'control' port")
        );
    }
}
