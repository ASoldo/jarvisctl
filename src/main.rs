//! jarvisctl: Operator-first control plane for local and hybrid coding agents

use anyhow::{Context, bail, ensure};
use clap::{Parser, Subcommand, ValueEnum, ValueHint};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env,
    ffi::OsStr,
    fs,
    io::{self, Read},
    net::TcpStream,
    path::PathBuf,
    process::{Child, Command as ProcessCommand, ExitCode, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use sysinfo::{Pid, System};
use thiserror::Error;
use tracing::{error, info, instrument};

use tracing_subscriber::{EnvFilter, FmtSubscriber};

mod agent;
mod autonomy;
mod board;
mod capability;
mod codex;
mod codex_app;
mod control_plane;
mod dispatch;
mod mission;
mod native;
mod operator_request;
mod orchestration;
mod proposal;
mod runtime;
#[cfg(test)]
mod test_support;
mod ticket;
mod tui;

use agent::spawn_agent;
use autonomy::{
    AutonomyDaemonOptions, AutonomyServiceInstallOptions, autonomy_service_status,
    install_autonomy_user_service, reconcile_from_records, render_autonomy_service_install,
    render_autonomy_service_status, run_autonomy_daemon, uninstall_autonomy_user_service,
};
use capability::{
    CapabilityRegisterOptions, RecurringMissionSmokeConfigureOptions,
    configure_recurring_mission_smoke, list_capabilities, reconcile_autonomy,
    recurring_mission_smoke_status, register_capability, render_autonomy_reconcile_output,
    render_capabilities_output, render_capability_output, render_capability_validation_output,
    render_mission_smoke_output, render_recurring_mission_smoke_status,
    run_recurring_mission_smoke, run_two_node_mission_smoke, show_capability,
    validate_capabilities, validate_capability,
};
use codex::{CodexLaunchOptions, CodexRuntimeDriver, launch_codex_ticket};
use codex_app::{
    CodexAppInputMode, attach_codex_app_tcp, codex_app_session_metadata_tcp,
    interrupt_codex_app_tcp, read_codex_app_thread, serve_codex_app_session,
    tell_codex_app_with_mode_tcp,
};
use control_plane::{
    ControlPlaneOutput, ControlPlaneResourceKindArg, EvidenceBundleOptions, KubernetesRenderOutput,
    NodeBootstrapOptions, NodeFanoutOptions, NodeLinksOptions, NodePairSessionOptions,
    NodeRegisterOptions, NodeScheduleOptions, NodeStartSessionOptions, NodeSudoOptions,
    NodeVisitOptions, PairDemoOptions, PairDemoSequenceOptions, PairLedgerFinalizeOptions,
    RelayMessageSendOptions, RemoteOperatorRequestResolveOptions, WorkerDriftSmokeOptions,
    WorkerDriftSmokeScheduleOptions, WorkerOffloadOptions, ack_cluster_relay_message,
    ack_relay_message, apply_kubernetes_resources, apply_kustomization, apply_manifests,
    attach_cluster_runtime_session, authorize_runtime_message, bootstrap_node, bundle_evidence,
    check_node_links, cleanup_node, cleanup_pair_demos, cluster_index, collect_node_task_note,
    configure_worker_drift_smoke_schedule, delete_cluster_runtime_session, doctor_nodes,
    export_pair_ledger, finalize_pair_ledger, flush_cluster_relay_messages, flush_relay_messages,
    heartbeat_node, inspect_node, install_node_heartbeat_user_service,
    interrupt_cluster_runtime_session, list_cluster_operator_requests, list_cluster_relay_messages,
    list_pair_ledgers, list_relay_messages, list_worker_run_records,
    load_or_create_orchestration_policy, load_worker_run_record, mark_worker_run,
    migrate_session_to_node, node_heartbeat_service_status, open_visit_capsule,
    orchestration_policy_path, pause_deployment_rollout, preflight_nodes,
    prune_cluster_relay_messages, prune_completed_runtime_sessions, prune_relay_messages,
    prune_worker_runs, read_auth_audit_events, read_worker_run_artifact, reconcile_nodes,
    register_node, render_describe_output, render_evidence_bundle_output, render_get_output,
    render_kubernetes_resources, render_node_heartbeat_service_install,
    render_node_heartbeat_service_status, render_node_probe_output, render_node_sudo_output,
    render_pair_demo_cleanup_output, render_pair_demo_sequence_output, render_pair_export_output,
    render_pair_finalize_output, render_pair_ledgers_output, render_pair_stale_review_output,
    render_relay_message_output, render_relay_messages_output, render_relay_prune_output,
    render_rollout_history_output, render_rollout_status_output, render_runtime_prune_output,
    render_worker_drift_smoke_output, render_worker_drift_smoke_schedule_status,
    render_worker_model_validation_output, render_worker_run_artifact_output,
    render_worker_run_prune_output, render_worker_runs_output, render_worker_validation_output,
    resolve_cluster_operator_request, resolve_service_target, resolve_service_target_for_message,
    respond_cluster_runtime_server_request, restart_deployment_rollout, resume_deployment_rollout,
    retry_cluster_relay_message, retry_relay_message, review_stale_pair_ledgers,
    rotate_capsule_key, run_node_fanout, run_node_sudo, run_node_visit, run_pair_demo_sequence,
    run_recurring_worker_drift_smoke, run_worker_drift_smoke, run_worker_offload, schedule_node,
    send_relay_message, set_node_cordoned, show_cluster_operator_request, start_node_pair_session,
    start_node_session, start_pair_demo, supersede_cluster_relay_message, supersede_relay_message,
    sync_codex_auth_to_node, tell_cluster_runtime_session, tell_runtime_session_on_node,
    undo_deployment_rollout, validate_worker_models, wait_for_rollout_status_output,
    worker_drift_smoke_schedule_status,
};
use dispatch::{DispatchOptions, run_dispatch_loop};
use mission::{
    MissionCreateOptions, MissionEventOptions, append_mission_event, complete_mission,
    create_mission, list_missions, render_mission_detail_output, render_mission_templates_output,
    render_missions_output, show_mission,
};
use native::{
    NativeSessionMetadata, RuntimeContextMetadata, serve_native_session, spawn_native_session,
};
use operator_request::{
    OperatorRequestCreateOptions, OperatorRequestResolveOptions, create_operator_request,
    list_operator_requests, notify_operator_requests, render_operator_request_notify_output,
    render_operator_request_output, render_operator_requests_output, resolve_operator_request,
    show_operator_request,
};
use orchestration::{
    default_autonomy_policy, plan_missions, render_autonomy_policy_output,
    render_mission_plans_output, render_worker_lane_scorecards_output, worker_lane_scorecards,
};
use proposal::{
    ProposalCreateOptions, ProposalDecisionOptions, create_proposal, decide_proposal,
    list_proposals, render_proposal_output, render_proposals_output, show_proposal,
};
use runtime::{
    attach_runtime_session, collect_runtime_sessions, delete_runtime_session,
    interrupt_runtime_session, respond_runtime_server_request, tell_runtime_session,
};
use ticket::slugify;
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

/// CLI tool to launch, steer, and inspect coding-agent work
#[derive(Parser, Debug)]
#[command(
    name = "jarvisctl",
    version,
    about = "Operator-first control plane for local and hybrid coding agents"
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

    /// Run a command in a new namespace
    Run {
        /// Deprecated compatibility flag; native is the only backend
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        /// Namespace name
        #[arg(long, alias = "ns")]
        namespace: String,

        /// Number of agents
        #[arg(long, default_value_t = 1)]
        agents: usize,

        /// Working directory for each agent
        #[arg(long, alias = "wd", value_hint = ValueHint::DirPath)]
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
        #[arg(long, alias = "tn", value_hint = ValueHint::FilePath)]
        task_note: PathBuf,

        /// Override the runtime namespace
        #[arg(long, alias = "ns")]
        namespace: Option<String>,

        /// Bind the launched Codex runtime to a control-plane namespace
        #[arg(long = "control-namespace", alias = "cns")]
        control_namespace: Option<String>,

        /// Internal control-plane deployment metadata for scheduled launches
        #[arg(long, hide = true)]
        deployment: Option<String>,

        /// Internal control-plane runtime label metadata for scheduled launches
        #[arg(long = "runtime-label", alias = "rl", hide = true)]
        runtime_labels: Vec<String>,

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
        #[arg(long = "resume-session-id", alias = "sid")]
        resume_session_id: Option<String>,

        /// Override the working directory instead of repo_path from the note
        #[arg(long, alias = "wd", value_hint = ValueHint::DirPath)]
        working_directory: Option<PathBuf>,

        /// Use an explicit prompt file instead of rendering from the task note
        #[arg(long, alias = "pf", value_hint = ValueHint::FilePath)]
        prompt_file: Option<PathBuf>,

        /// Additional operator message to send with the launch or resume prompt
        #[arg(long)]
        message: Option<String>,

        /// Image(s) to attach to the initial or resumed Codex prompt
        #[arg(long, value_hint = ValueHint::FilePath)]
        image: Vec<PathBuf>,

        /// Wait this long before injecting the prompt
        #[arg(long, alias = "delay-ms", default_value_t = 1500)]
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
        #[arg(long, alias = "vault", value_hint = ValueHint::DirPath, default_value = "/home/rootster/codex")]
        vault_path: PathBuf,

        /// Board file to scan; may be repeated. Defaults to the dispatch board and project boards in the vault.
        #[arg(long, alias = "b", value_hint = ValueHint::FilePath)]
        board: Vec<PathBuf>,

        /// Scan once and exit instead of looping as a daemon
        #[arg(long, default_value_t = false)]
        once: bool,

        /// Evaluate transitions without launching Codex or writing board/ticket changes
        #[arg(long, default_value_t = false)]
        dry_run: bool,

        /// Polling interval in seconds when not using --once
        #[arg(long, alias = "interval", default_value_t = 15)]
        interval_seconds: u64,

        /// Override the dispatch state file
        #[arg(long, alias = "state", value_hint = ValueHint::FilePath)]
        state_file: Option<PathBuf>,

        /// Agent to inject the prompt into
        #[arg(long, default_value = "agent0")]
        agent: String,

        /// Number of Codex agents to create
        #[arg(long, default_value_t = 1)]
        agents: usize,

        /// Wait this long before injecting the prompt
        #[arg(long, alias = "delay-ms", default_value_t = 1500)]
        startup_delay_ms: u64,

        /// Codex command override, defaults to `codex`
        #[arg(last = true, value_hint = ValueHint::CommandString)]
        command: Vec<String>,
    },

    /// Send a bounded Codex prompt capsule to a remote node and return its result
    Visit {
        /// Registered remote node to visit, or `auto` to let the scheduler choose
        #[arg(long, alias = "n", default_value = "auto")]
        node: String,

        /// Run the visit from another registered node instead of this control node
        #[arg(long = "from-node", alias = "from")]
        from_node: Option<String>,

        /// Scheduler role constraint when --node auto is used
        #[arg(long)]
        role: Option<String>,

        /// Scheduler label constraint when --node auto is used
        #[arg(long = "label", alias = "lbl")]
        labels: Vec<String>,

        /// Retry a scheduled visit on another eligible node after failure
        #[arg(long, default_value_t = 0)]
        retries: usize,

        /// Prompt text to send to the remote Codex
        #[arg(long, conflicts_with_all = ["prompt_file", "from_current"])]
        text: Option<String>,

        /// Prompt file to send to the remote Codex
        #[arg(long = "file", alias = "f", value_hint = ValueHint::FilePath, conflicts_with_all = ["text", "from_current"])]
        prompt_file: Option<PathBuf>,

        /// Build a capsule from this shell/workspace and the latest local Codex transcript tail
        #[arg(long = "from-current", default_value_t = false, conflicts_with_all = ["text", "prompt_file"])]
        from_current: bool,

        /// Remote working directory; defaults to the remote user's home directory
        #[arg(long, alias = "wd", value_hint = ValueHint::DirPath)]
        working_directory: Option<String>,

        /// Override the generated visit namespace/lease name
        #[arg(long, alias = "ns")]
        namespace: Option<String>,

        /// Kill the remote visit if it runs longer than this
        #[arg(long = "timeout-seconds", alias = "timeout", default_value_t = 900)]
        timeout_seconds: u64,

        /// Remote Codex sandbox mode for the visit
        #[arg(long, default_value = "read-only")]
        sandbox: String,

        /// Remote Codex model override
        #[arg(long)]
        model: Option<String>,

        /// Remote Codex reasoning effort override
        #[arg(long = "reasoning-effort", alias = "re")]
        reasoning_effort: Option<String>,

        /// Do not persist a Codex session file on the remote node
        #[arg(long, default_value_t = false)]
        ephemeral: bool,

        /// Read an encrypted Jarvis visit capsule from --file
        #[arg(long = "protected-capsule", default_value_t = false, hide = true)]
        protected_capsule: bool,

        /// Print the full remote stdout/stderr envelope instead of just the final answer
        #[arg(long, default_value_t = false)]
        full: bool,
    },

    /// Decrypt a protected Jarvis visit capsule from stdin
    #[command(name = "capsule-open", hide = true)]
    CapsuleOpen,

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

        #[arg(short = 'n', long = "resource-namespace", alias = "ns", alias = "rns")]
        resource_namespace: Option<String>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Describe a declarative control-plane resource
    Describe {
        kind: ControlPlaneResourceKindArg,
        name: String,

        #[arg(short = 'n', long = "resource-namespace", alias = "ns", alias = "rns")]
        resource_namespace: Option<String>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Yaml)]
        output: ControlPlaneOutput,
    },

    /// Register and manage Jarvis cluster nodes
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },

    /// Inspect and run service-backed bounded worker lanes
    Worker {
        #[command(subcommand)]
        command: WorkerCommand,
    },

    /// Track business objectives, decisions, evidence, and runtime links
    Mission {
        #[command(subcommand)]
        command: MissionCommand,
    },

    /// Track proposed actions that require an operator decision before mutation
    Proposal {
        #[command(subcommand)]
        command: ProposalCommand,
    },

    /// Inspect and manage paired runtime coordination ledgers
    Pair {
        #[command(subcommand)]
        command: PairCommand,
    },

    /// Build portable evidence bundles for pairs, namespaces, and missions
    Evidence {
        #[command(subcommand)]
        command: EvidenceCommand,
    },

    /// Inspect and validate autonomous capability lanes
    Capability {
        #[command(subcommand)]
        command: CapabilityCommand,
    },

    /// Reconcile autonomous work queues, blockers, and notifications
    Autonomy {
        #[command(subcommand)]
        command: AutonomyCommand,
    },

    /// Show production readiness across nodes, policy, queues, pairs, and capabilities
    Health {
        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Run a production smoke suite across Codex, nodes, workers, pair demos, approvals, and evidence
    ProductionSmoke {
        #[arg(long = "namespace-prefix", alias = "ns")]
        namespace_prefix: Option<String>,

        #[arg(long, default_value_t = false)]
        history: bool,

        #[arg(long, default_value_t = 20)]
        limit: usize,

        #[arg(long = "no-record", default_value_t = false)]
        no_record: bool,

        #[arg(long, default_value_t = false)]
        skip_worker_models: bool,

        #[arg(long, default_value_t = false)]
        skip_evidence: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Manage durable operator/admin requests and notifications
    #[command(alias = "notify", alias = "operator-requests")]
    OperatorRequest {
        #[command(subcommand)]
        command: OperatorRequestCommand,
    },

    /// Queue, inspect, retry, and acknowledge cross-runtime relay messages
    #[command(alias = "messages", alias = "relay")]
    Message {
        #[command(subcommand)]
        command: MessageCommand,
    },

    /// Inspect or trigger Deployment rollouts
    Rollout {
        #[command(subcommand)]
        command: RolloutCommand,
    },

    /// Experimental: render or apply the narrow Kubernetes adapter surface
    Kube {
        #[command(subcommand)]
        command: KubeCommand,
    },

    /// Open a ratatui session dashboard
    Dashboard {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        #[arg(long, alias = "refresh", default_value_t = 1000)]
        refresh_ms: u64,
    },

    /// Attach to a running namespace
    Attach {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        #[arg(
            long,
            alias = "ns",
            required_unless_present = "service",
            conflicts_with = "service"
        )]
        namespace: Option<String>,

        #[arg(
            long,
            required_unless_present = "namespace",
            conflicts_with = "namespace"
        )]
        service: Option<String>,

        #[arg(
            short = 'n',
            long = "resource-namespace",
            alias = "rns",
            requires = "service"
        )]
        resource_namespace: Option<String>,
    },

    /// Kill a namespace
    Delete {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        #[arg(long, alias = "ns")]
        namespace: String,

        #[arg(long)]
        mission: Option<String>,
    },

    /// List namespaces and agents
    List {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        #[arg(long, alias = "ns")]
        namespace: Option<String>,

        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Read Codex app-server thread history for a namespace
    History {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        #[arg(
            long,
            alias = "ns",
            required_unless_present = "service",
            conflicts_with = "service"
        )]
        namespace: Option<String>,

        #[arg(
            long,
            required_unless_present = "namespace",
            conflicts_with = "namespace"
        )]
        service: Option<String>,

        #[arg(
            short = 'n',
            long = "resource-namespace",
            alias = "rns",
            requires = "service"
        )]
        resource_namespace: Option<String>,

        #[arg(long, default_value_t = true)]
        include_turns: bool,

        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Attach to a specific agent in a namespace
    Exec {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        #[arg(
            long,
            alias = "ns",
            required_unless_present = "service",
            conflicts_with = "service"
        )]
        namespace: Option<String>,

        #[arg(
            long,
            required_unless_present = "namespace",
            conflicts_with = "namespace"
        )]
        service: Option<String>,

        #[arg(
            short = 'n',
            long = "resource-namespace",
            alias = "rns",
            requires = "service"
        )]
        resource_namespace: Option<String>,

        #[arg(long, default_value = "agent0")]
        agent: String,
    },

    /// Send file or text to a running agent's TUI
    Tell {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        /// Direct target in node/namespace or node/namespace/agent form
        #[arg(long, conflicts_with_all = ["service", "namespace", "node"])]
        target: Option<String>,

        /// Route directly to a specific Jarvis node
        #[arg(long)]
        node: Option<String>,

        #[arg(
            long,
            alias = "ns",
            required_unless_present_any = ["service", "target"],
            conflicts_with_all = ["service", "target"]
        )]
        namespace: Option<String>,
        #[arg(
            long,
            required_unless_present_any = ["namespace", "target"],
            conflicts_with_all = ["namespace", "target"]
        )]
        service: Option<String>,
        #[arg(
            short = 'n',
            long = "resource-namespace",
            alias = "rns",
            requires = "service"
        )]
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

    /// Respond to a pending Codex app-server request
    RespondRequest {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        #[arg(long, visible_alias = "ns")]
        namespace: String,

        #[arg(long = "request-id", visible_alias = "id")]
        request_id: String,

        #[arg(long = "response-json", conflicts_with = "error")]
        response_json: Option<String>,

        #[arg(long, conflicts_with = "response_json")]
        error: Option<String>,

        #[arg(long)]
        mission: Option<String>,
    },

    /// Send Ctrl+C to a running agent
    Interrupt {
        #[arg(long, value_enum, default_value_t = SessionBackend::Native, hide = true)]
        backend: SessionBackend,

        #[arg(
            long,
            alias = "ns",
            required_unless_present = "service",
            conflicts_with = "service"
        )]
        namespace: Option<String>,

        #[arg(
            long,
            required_unless_present = "namespace",
            conflicts_with = "namespace"
        )]
        service: Option<String>,

        #[arg(
            short = 'n',
            long = "resource-namespace",
            alias = "rns",
            requires = "service"
        )]
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
}

#[derive(Subcommand, Debug)]
enum MissionCommand {
    /// Create an operational mission ledger entry
    Create {
        #[arg(long)]
        title: String,

        #[arg(long)]
        template: Option<String>,

        #[arg(long)]
        objective: Option<String>,

        #[arg(long)]
        priority: Option<String>,

        #[arg(long)]
        owner: Option<String>,

        #[arg(long = "label", alias = "lbl")]
        labels: Vec<String>,

        #[arg(long = "ticket", alias = "t", value_hint = ValueHint::FilePath)]
        tickets: Vec<PathBuf>,

        #[arg(long = "namespace", visible_alias = "ns")]
        namespaces: Vec<String>,

        #[arg(long = "node", alias = "n")]
        nodes: Vec<String>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// List mission ledger entries
    List {
        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// List built-in mission templates
    Templates {
        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Render autonomous next-step recommendations for mission control
    Plan {
        id: Option<String>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Show autonomy guardrails for what agents may do without approval
    Policy {
        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Score orchestration lanes so autonomy only expands where evidence supports it
    Scorecards {
        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Create or execute a recorded two-node mission smoke
    Smoke {
        #[arg(long = "first-node", alias = "n1")]
        first_node: String,

        #[arg(long = "second-node", alias = "n2")]
        second_node: String,

        #[arg(long = "first-task-note", alias = "t1", value_hint = ValueHint::FilePath)]
        first_task_note: PathBuf,

        #[arg(long = "second-task-note", alias = "t2", value_hint = ValueHint::FilePath)]
        second_task_note: PathBuf,

        #[arg(long = "namespace-prefix", alias = "ns")]
        namespace_prefix: Option<String>,

        #[arg(long, default_value_t = false)]
        dry_run: bool,

        #[arg(long, default_value_t = false, default_missing_value = "true", num_args = 0..=1, require_equals = true)]
        execute: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,

        #[arg(last = true, value_hint = ValueHint::CommandString)]
        command: Vec<String>,
    },

    /// Configure recurring two-node smoke that the autonomy timer runs when due
    SmokeSchedule {
        #[arg(long = "first-node", alias = "n1")]
        first_node: String,

        #[arg(long = "second-node", alias = "n2")]
        second_node: String,

        #[arg(long = "first-task-note", alias = "t1", value_hint = ValueHint::FilePath)]
        first_task_note: Option<PathBuf>,

        #[arg(long = "second-task-note", alias = "t2", value_hint = ValueHint::FilePath)]
        second_task_note: Option<PathBuf>,

        #[arg(long = "namespace-prefix", alias = "ns")]
        namespace_prefix: Option<String>,

        #[arg(long = "interval-seconds", alias = "interval", default_value_t = 24 * 60 * 60)]
        interval_seconds: u64,

        #[arg(long, default_value_t = false, default_missing_value = "true", num_args = 0..=1, require_equals = true)]
        execute: bool,

        #[arg(long, default_value_t = true, default_missing_value = "true", num_args = 0..=1, require_equals = true)]
        enabled: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Show recurring two-node smoke schedule and last-run state
    SmokeStatus {
        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Run the configured recurring two-node smoke now
    SmokeRun {
        #[arg(long, default_value_t = false)]
        force: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Show one mission with its event timeline
    Show {
        id: String,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Append a stage event and optional runtime/evidence links
    Event {
        id: String,

        #[arg(long)]
        stage: String,

        #[arg(long)]
        status: String,

        #[arg(long)]
        summary: String,

        #[arg(long = "ticket", alias = "t", value_hint = ValueHint::FilePath)]
        ticket: Option<PathBuf>,

        #[arg(long = "namespace", visible_alias = "ns")]
        namespace: Option<String>,

        #[arg(long = "node", alias = "n")]
        node: Option<String>,

        #[arg(long)]
        visit: Option<String>,

        #[arg(long)]
        approval: Option<String>,

        #[arg(long = "evidence", alias = "ev")]
        evidence: Vec<String>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Mark a mission complete, failed, or cancelled with an outcome
    Complete {
        id: String,

        #[arg(long, default_value = "completed")]
        status: String,

        #[arg(long)]
        outcome: String,

        #[arg(long = "evidence", alias = "ev")]
        evidence: Vec<String>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },
}

#[derive(Subcommand, Debug)]
enum ProposalCommand {
    /// Create an operator decision proposal
    Create {
        #[arg(long)]
        title: String,

        #[arg(long)]
        mission: Option<String>,

        #[arg(long)]
        action: String,

        #[arg(long)]
        rationale: String,

        #[arg(long)]
        risk: Option<String>,

        #[arg(long = "proposed-by", alias = "by")]
        proposed_by: Option<String>,

        #[arg(long = "evidence", alias = "ev")]
        evidence: Vec<String>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// List proposals
    List {
        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Show one proposal
    Show {
        id: String,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Approve, reject, or mark a proposal superseded
    Decide {
        id: String,

        #[arg(long)]
        status: String,

        #[arg(long)]
        decision: String,

        #[arg(long = "decided-by", alias = "by")]
        decided_by: Option<String>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },
}

#[derive(Subcommand, Debug)]
enum PairCommand {
    /// List paired runtime coordination ledgers
    Ledger {
        #[arg(long = "include-archived")]
        include_archived: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Export pair evidence as Markdown or structured JSON/YAML
    Export {
        id: String,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Create demo pair task notes and optionally launch both node sessions
    Demo {
        #[arg(long = "first-node", alias = "n1")]
        first_node: Option<String>,

        #[arg(long = "second-node", alias = "n2")]
        second_node: Option<String>,

        #[arg(long = "namespace-prefix", alias = "ns")]
        namespace_prefix: Option<String>,

        #[arg(long, default_value_t = false, default_missing_value = "true", num_args = 0..=1, require_equals = true)]
        execute: bool,

        #[arg(long = "startup-delay-ms", default_value_t = 1500)]
        startup_delay_ms: u64,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,

        #[arg(last = true, value_hint = ValueHint::CommandString)]
        command: Vec<String>,
    },

    /// Run the presentation-grade two-node demo sequence
    RunDemo {
        #[arg(long = "first-node", alias = "n1")]
        first_node: Option<String>,

        #[arg(long = "second-node", alias = "n2")]
        second_node: Option<String>,

        #[arg(long = "namespace-prefix", alias = "ns")]
        namespace_prefix: Option<String>,

        #[arg(long, default_value_t = false, default_missing_value = "true", num_args = 0..=1, require_equals = true)]
        execute: bool,

        #[arg(long = "startup-delay-ms", default_value_t = 1500)]
        startup_delay_ms: u64,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,

        #[arg(last = true, value_hint = ValueHint::CommandString)]
        command: Vec<String>,
    },

    /// Remove old generated pair demo task-note directories
    CleanupDemos {
        #[arg(long = "max-age-days", alias = "max-age", default_value_t = 7)]
        max_age_days: u64,

        #[arg(long, default_value_t = true)]
        dry_run: bool,

        #[arg(long, default_value_t = false)]
        apply: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Review stale pair ledgers and optionally archive them
    ReviewStale {
        #[arg(long, default_value_t = true)]
        dry_run: bool,

        #[arg(long, default_value_t = false)]
        archive: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Collect evidence, close namespaces, mark reviewed, and archive a pair ledger
    Finalize {
        id: String,

        #[arg(long = "skip-collect")]
        skip_collect: bool,

        #[arg(long = "skip-close")]
        skip_close: bool,

        #[arg(long = "skip-archive")]
        skip_archive: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },
}

#[derive(Subcommand, Debug)]
enum EvidenceCommand {
    /// Build a portable Markdown evidence bundle
    Bundle {
        #[arg(long = "pair-id", alias = "pair")]
        pair_id: Option<String>,

        #[arg(long = "namespace", alias = "ns")]
        namespace: Option<String>,

        #[arg(long = "output-dir", alias = "dir", value_hint = ValueHint::DirPath)]
        output_dir: Option<PathBuf>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },
}

#[derive(Debug, Serialize)]
struct ProductionHealthReport {
    status: String,
    generated_at_epoch_ms: u128,
    nodes_ready: usize,
    nodes_total: usize,
    link_failures: usize,
    runtime_sessions: usize,
    active_pairs: usize,
    archived_pairs: usize,
    stale_pairs: usize,
    pending_operator_requests: usize,
    pending_proposals: usize,
    capability_count: usize,
    capability_failures: usize,
    failed_worker_runs: usize,
    node_admission: Vec<HealthNodeAdmission>,
    worker_admission: Vec<HealthWorkerAdmission>,
    policy_gates: Vec<HealthPolicyGate>,
    autonomy_queue: Vec<HealthQueueItem>,
    issues: Vec<String>,
}

#[derive(Debug, Serialize)]
struct HealthNodeAdmission {
    node: String,
    status: String,
    schedulable: bool,
    issues: Vec<String>,
    recommendation: String,
}

#[derive(Debug, Serialize)]
struct HealthWorkerAdmission {
    worker: String,
    namespace: String,
    status: String,
    recent_failures: usize,
    recommendation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProductionSmokeReport {
    id: String,
    status: String,
    generated_at_epoch_ms: u128,
    checks: Vec<ProductionSmokeCheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProductionSmokeCheck {
    name: String,
    status: String,
    detail: String,
}

#[derive(Debug, Serialize)]
struct HealthPolicyGate {
    id: String,
    decision: String,
    action_class: String,
}

#[derive(Debug, Serialize)]
struct HealthQueueItem {
    kind: String,
    status: String,
    summary: String,
    command: Option<String>,
}

#[derive(Subcommand, Debug)]
enum CapabilityCommand {
    /// List built-in and registered autonomy capabilities
    List {
        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Show one autonomy capability
    Show {
        id: String,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Register or replace an operator-defined capability lane
    Register {
        #[arg(long)]
        id: String,

        #[arg(long)]
        title: String,

        #[arg(long)]
        lane: String,

        #[arg(long)]
        description: String,

        #[arg(long)]
        status: Option<String>,

        #[arg(long)]
        confidence: Option<u8>,

        #[arg(long, default_value_t = true)]
        schedulable: bool,

        #[arg(long = "evidence", alias = "ev")]
        evidence: Vec<String>,

        #[arg(long = "gap")]
        gaps: Vec<String>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Run validator commands for one or all capability lanes
    Validate {
        id: Option<String>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },
}

#[derive(Subcommand, Debug)]
enum AutonomyCommand {
    /// Reconcile safe autonomous work and decision-grade blockers
    Reconcile {
        #[arg(long, default_value_t = false)]
        notify: bool,

        #[arg(long, default_value_t = false)]
        dry_run: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Run the autonomy reconciler continuously for supervised foreground use
    Daemon {
        #[arg(long = "interval-seconds", alias = "interval", default_value_t = 300)]
        interval_seconds: u64,

        #[arg(long, default_value_t = true)]
        notify: bool,

        #[arg(long, default_value_t = false)]
        once: bool,
    },

    /// Install or update the user-systemd autonomy timer
    InstallUserService {
        #[arg(long = "interval-seconds", alias = "interval", default_value_t = 300)]
        interval_seconds: u64,

        #[arg(long, default_value_t = true, default_missing_value = "true", num_args = 0..=1, require_equals = true)]
        notify: bool,

        #[arg(long, default_value_t = true, default_missing_value = "true", num_args = 0..=1, require_equals = true)]
        enable: bool,

        #[arg(long, default_value_t = true, default_missing_value = "true", num_args = 0..=1, require_equals = true)]
        start: bool,

        #[arg(long = "request-linger", default_value_t = true, default_missing_value = "true", num_args = 0..=1, require_equals = true)]
        request_linger: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Show the user-systemd autonomy timer state
    ServiceStatus {
        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Disable and remove the user-systemd autonomy timer
    UninstallUserService {
        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },
}

#[derive(Subcommand, Debug)]
enum WorkerCommand {
    /// Validate that at least one service-backed worker lane has a ready runtime endpoint
    Validate {
        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Validate that worker provider models are still listed by their providers
    ValidateModels {
        #[arg(long)]
        all: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Send a bounded prompt to a runtime service lane and record a dispatch artifact
    Offload {
        #[arg(long, alias = "svc")]
        service: String,

        #[arg(short = 'n', long = "resource-namespace", alias = "ns", alias = "rns")]
        resource_namespace: Option<String>,

        #[arg(long = "via-runtime-namespace", alias = "via-ns")]
        via_runtime_namespace: Option<String>,

        #[arg(long)]
        intent: Option<String>,

        #[arg(long)]
        prompt: String,

        #[arg(long = "output-path", alias = "artifact", value_hint = ValueHint::FilePath)]
        output_path: Option<PathBuf>,

        #[arg(long = "job-name", alias = "job")]
        job_name: Option<String>,

        #[arg(long)]
        execute: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// List durable worker offload run records
    Runs {
        #[arg(long)]
        all: bool,

        #[arg(long, default_value_t = 25)]
        limit: usize,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Show one durable worker offload run record
    ShowRun {
        id: String,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Yaml)]
        output: ControlPlaneOutput,
    },

    /// Read the artifact produced by a worker run
    Artifact {
        id: String,

        #[arg(long)]
        all: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Mark a worker run as acknowledged, remediated, or ignored
    MarkRun {
        id: String,

        #[arg(long)]
        status: String,

        #[arg(long)]
        note: Option<String>,

        #[arg(long)]
        all: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Json)]
        output: ControlPlaneOutput,
    },

    /// Validate models and execute a provider-backed smoke through a worker service
    DriftSmoke {
        #[arg(long, alias = "svc")]
        service: String,

        #[arg(short = 'n', long = "resource-namespace", alias = "ns", alias = "rns")]
        resource_namespace: Option<String>,

        #[arg(long)]
        prompt: Option<String>,

        #[arg(long = "output-path", alias = "artifact", value_hint = ValueHint::FilePath)]
        output_path: Option<PathBuf>,

        #[arg(long = "job-name", alias = "job")]
        job_name: Option<String>,

        #[arg(long)]
        all: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Json)]
        output: ControlPlaneOutput,
    },

    /// Configure recurring worker drift smoke that autonomy can run when due
    DriftSchedule {
        #[arg(long, alias = "svc")]
        service: String,

        #[arg(short = 'n', long = "resource-namespace", alias = "ns", alias = "rns")]
        resource_namespace: Option<String>,

        #[arg(long = "interval-seconds", alias = "interval", default_value_t = 6 * 60 * 60)]
        interval_seconds: u64,

        #[arg(long, default_value_t = true, default_missing_value = "true", num_args = 0..=1, require_equals = true)]
        enabled: bool,

        #[arg(long)]
        all: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Show recurring worker drift smoke schedule and last-run state
    DriftStatus {
        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Run the configured recurring worker drift smoke now
    DriftRun {
        #[arg(long, default_value_t = false)]
        force: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Json)]
        output: ControlPlaneOutput,
    },

    /// Prune or redact worker run records and artifacts
    PruneRuns {
        #[arg(long = "max-age-days", default_value_t = 30)]
        max_age_days: u64,

        #[arg(long, default_value_t = true)]
        dry_run: bool,

        #[arg(long, default_value_t = false)]
        apply: bool,

        #[arg(long, default_value_t = false)]
        redact: bool,

        #[arg(long = "no-redact", default_value_t = false)]
        no_redact: bool,

        #[arg(long)]
        all: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },
}

#[derive(Subcommand, Debug)]
enum OperatorRequestCommand {
    /// Create a durable operator/admin request
    Create {
        #[arg(long)]
        title: String,

        #[arg(long, default_value = "operator")]
        kind: String,

        #[arg(long, default_value = "medium")]
        severity: String,

        #[arg(long)]
        reason: String,

        #[arg(long)]
        risk: Option<String>,

        #[arg(long = "requested-by", alias = "by")]
        requested_by: Option<String>,

        #[arg(long = "namespace", visible_alias = "ns")]
        namespace: Option<String>,

        #[arg(long = "request-id", visible_alias = "id")]
        request_id: Option<String>,

        #[arg(long)]
        method: Option<String>,

        #[arg(long)]
        command: Option<String>,

        #[arg(long = "params-json")]
        params_json: Option<String>,

        #[arg(long = "ttl-seconds", default_value_t = 12 * 60 * 60)]
        ttl_seconds: u64,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Create a privileged-action request with command and reasoning
    Sudo {
        #[arg(long)]
        title: String,

        #[arg(long)]
        reason: String,

        #[arg(long)]
        command: String,

        #[arg(long)]
        risk: Option<String>,

        #[arg(long = "requested-by", alias = "by", default_value = "agent")]
        requested_by: String,

        #[arg(long = "namespace", visible_alias = "ns")]
        namespace: Option<String>,

        #[arg(long = "ttl-seconds", default_value_t = 12 * 60 * 60)]
        ttl_seconds: u64,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// List operator/admin requests
    List {
        #[arg(long)]
        status: Option<String>,

        #[arg(long, default_value_t = false)]
        all: bool,

        #[arg(long, default_value_t = false)]
        cluster: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Show one operator/admin request
    Show {
        id: String,

        #[arg(long)]
        node: Option<String>,

        #[arg(long, default_value_t = false)]
        cluster: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Send persistent desktop notifications for pending requests
    Notify {
        #[arg(long)]
        status: Option<String>,

        #[arg(long, default_value_t = true)]
        persistent: bool,

        #[arg(long, default_value_t = false)]
        dry_run: bool,

        #[arg(long, default_value_t = false)]
        cluster: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Resolve a request, optionally responding to a linked app-server request
    Resolve {
        id: String,

        #[arg(long, default_value = "approved")]
        status: String,

        #[arg(long = "response-json", conflicts_with = "error")]
        response_json: Option<String>,

        #[arg(long, conflicts_with = "response_json")]
        error: Option<String>,

        #[arg(long = "decided-by", alias = "by")]
        decided_by: Option<String>,

        #[arg(long)]
        decision: Option<String>,

        #[arg(long)]
        node: Option<String>,

        #[arg(long)]
        mission: Option<String>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },
}

#[derive(Subcommand, Debug)]
enum MessageCommand {
    /// Send a durable relay message to a runtime namespace
    Send {
        #[arg(long = "to-namespace", alias = "to-ns", alias = "ns")]
        to_namespace: String,

        #[arg(long = "from-namespace", alias = "from-ns")]
        from_namespace: Option<String>,

        #[arg(long = "to-node", alias = "node")]
        to_node: Option<String>,

        #[arg(long, default_value = "agent0")]
        agent: String,

        #[arg(long, default_value = "auto")]
        mode: String,

        #[arg(long, default_value = "operator")]
        kind: String,

        #[arg(long, conflicts_with = "file")]
        text: Option<String>,

        #[arg(long = "file", alias = "f", value_hint = ValueHint::FilePath, conflicts_with = "text")]
        file: Option<PathBuf>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// List durable relay messages
    List {
        #[arg(long = "namespace", alias = "ns")]
        namespace: Option<String>,

        #[arg(long)]
        status: Option<String>,

        #[arg(long, default_value_t = false)]
        all: bool,

        #[arg(long, default_value_t = false)]
        cluster: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Retry pending relay messages
    Flush {
        #[arg(long = "namespace", alias = "ns")]
        namespace: Option<String>,

        #[arg(long)]
        status: Option<String>,

        #[arg(long, default_value_t = false)]
        cluster: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Mark a relay message as acknowledged
    Ack {
        id: String,

        #[arg(long)]
        node: Option<String>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Retry one relay message now
    Retry {
        id: String,

        #[arg(long)]
        node: Option<String>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Mark a relay message superseded without retrying it again
    Supersede {
        id: String,

        #[arg(long)]
        reason: String,

        #[arg(long)]
        node: Option<String>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Prune old terminal relay messages while preserving pending work
    Prune {
        #[arg(long = "max-age-days", default_value_t = 14)]
        max_age_days: u64,

        #[arg(long, default_value_t = true)]
        dry_run: bool,

        #[arg(long, default_value_t = false)]
        apply: bool,

        #[arg(long, default_value_t = false)]
        cluster: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },
}

#[derive(Subcommand, Debug)]
enum NodeCommand {
    /// Register a local, SSH, or Tailscale-reachable worker node
    Register {
        name: String,

        #[arg(long)]
        address: Option<String>,

        #[arg(long = "ssh-host", alias = "host")]
        ssh_host: Option<String>,

        #[arg(long = "ssh-user", alias = "user")]
        ssh_user: Option<String>,

        #[arg(long, default_value_t = false)]
        local: bool,

        #[arg(long = "role", alias = "r")]
        roles: Vec<String>,

        #[arg(long = "label", alias = "lbl")]
        labels: Vec<String>,

        #[arg(long = "workspace-root", alias = "wd", value_hint = ValueHint::DirPath)]
        workspace_root: Option<String>,

        #[arg(long = "max-sessions", alias = "max")]
        max_sessions: Option<usize>,
    },

    /// Probe a registered node over its configured transport
    Ping {
        name: String,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Mark a node unschedulable for future work
    Cordon { name: String },

    /// Mark a node schedulable again
    Uncordon { name: String },

    /// Copy this machine's Codex auth/config to a node over SSH
    SyncCodexAuth { name: String },

    /// Pick the best available remote worker node
    Schedule {
        #[arg(long)]
        role: Option<String>,

        #[arg(long = "label", alias = "lbl")]
        labels: Vec<String>,

        #[arg(long = "exclude", alias = "x")]
        exclude: Vec<String>,

        #[arg(long = "require-codex-auth", alias = "auth", default_value_t = true)]
        require_codex_auth: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Check all registered nodes for orchestration readiness
    Doctor {
        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Write a durable heartbeat on a local or registered node
    Heartbeat {
        name: Option<String>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Install or update the user-systemd timer that refreshes this node heartbeat
    InstallHeartbeatUserService {
        #[arg(long = "interval-seconds", alias = "interval", default_value_t = 120)]
        interval_seconds: u64,

        #[arg(long, default_value_t = true, default_missing_value = "true", num_args = 0..=1, require_equals = true)]
        enable: bool,

        #[arg(long, default_value_t = true, default_missing_value = "true", num_args = 0..=1, require_equals = true)]
        start: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Show this node heartbeat user-systemd timer state
    HeartbeatServiceStatus {
        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Collect a staged remote task note back to the control node
    CollectTaskNote {
        name: String,

        #[arg(long = "namespace", alias = "ns")]
        namespace: String,

        #[arg(long = "task-note", alias = "tn", value_hint = ValueHint::FilePath)]
        task_note: PathBuf,

        #[arg(long = "destination", alias = "dest", value_hint = ValueHint::FilePath)]
        destination: Option<PathBuf>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Run cluster readiness checks before launching orchestration workloads
    Preflight {
        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Check directed SSH reachability between registered nodes
    Links {
        #[arg(long = "from")]
        from: Vec<String>,

        #[arg(long = "to")]
        to: Vec<String>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Run one approved sudo command on a node using a password from stdin
    Sudo {
        name: String,

        #[arg(long)]
        command: String,

        #[arg(long = "password-stdin", default_value_t = false)]
        password_stdin: bool,

        #[arg(long = "timeout-seconds", alias = "timeout", default_value_t = 120)]
        timeout_seconds: u64,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Show or initialize persistent orchestration policy
    Policy {
        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Yaml)]
        output: ControlPlaneOutput,
    },

    /// Run doctor plus stale lease/artifact cleanup across available nodes
    Reconcile {
        #[arg(long = "max-age-days", alias = "max-age")]
        max_age_days: Option<u64>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Rotate the encrypted visit capsule key and sync it to remote nodes
    RotateCapsuleKey {
        #[arg(long = "no-sync", default_value_t = false)]
        no_sync: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Show running cluster sessions and finished/running visits from one index
    Index {
        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Show auth lease audit events
    Audit {
        #[arg(long, default_value_t = 20)]
        limit: usize,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Send the same protected visit prompt to multiple nodes
    Fanout {
        #[arg(long = "node", alias = "n")]
        nodes: Vec<String>,

        #[arg(long)]
        role: Option<String>,

        #[arg(long = "label", alias = "lbl")]
        labels: Vec<String>,

        #[arg(long)]
        text: Option<String>,

        #[arg(long = "file", alias = "f", value_hint = ValueHint::FilePath)]
        prompt_file: Option<PathBuf>,

        #[arg(long = "timeout-seconds", alias = "timeout", default_value_t = 900)]
        timeout_seconds: u64,

        #[arg(long, default_value = "read-only")]
        sandbox: String,

        #[arg(long)]
        model: Option<String>,

        #[arg(long = "reasoning-effort", alias = "re")]
        reasoning_effort: Option<String>,

        #[arg(long, default_value_t = false)]
        ephemeral: bool,

        #[arg(long = "max-concurrency", alias = "mc", default_value_t = 4)]
        max_concurrency: usize,

        #[arg(long, default_value_t = false)]
        full: bool,
    },

    /// Run one scheduled AI task with retry/failover semantics
    Task {
        #[arg(long)]
        text: Option<String>,

        #[arg(long = "file", alias = "f", value_hint = ValueHint::FilePath)]
        prompt_file: Option<PathBuf>,

        #[arg(long)]
        role: Option<String>,

        #[arg(long = "label", alias = "lbl")]
        labels: Vec<String>,

        #[arg(long, default_value_t = 1)]
        retries: usize,

        #[arg(long = "timeout-seconds", alias = "timeout", default_value_t = 900)]
        timeout_seconds: u64,

        #[arg(long, default_value = "read-only")]
        sandbox: String,

        #[arg(long)]
        model: Option<String>,

        #[arg(long = "reasoning-effort", alias = "re")]
        reasoning_effort: Option<String>,

        #[arg(long, default_value_t = false)]
        ephemeral: bool,

        #[arg(long, default_value_t = false)]
        full: bool,
    },

    /// Start a durable Codex app-server session on a scheduled remote node
    StartSession {
        #[arg(long, alias = "n", default_value = "auto")]
        node: String,

        #[arg(long)]
        role: Option<String>,

        #[arg(long = "label", alias = "lbl")]
        labels: Vec<String>,

        #[arg(long)]
        retries: Option<usize>,

        #[arg(long = "task-note", alias = "tn", value_hint = ValueHint::FilePath)]
        task_note: PathBuf,

        #[arg(long, alias = "ns")]
        namespace: Option<String>,

        #[arg(long = "resume-session-id", alias = "sid")]
        resume_session_id: Option<String>,

        #[arg(long, default_value_t = false, conflicts_with = "resume_session_id")]
        resume_latest: bool,

        #[arg(long, alias = "wd", value_hint = ValueHint::DirPath)]
        working_directory: Option<PathBuf>,

        #[arg(long)]
        message: Option<String>,

        #[arg(long, alias = "delay-ms", default_value_t = 1500)]
        startup_delay_ms: u64,

        #[arg(long)]
        mission: Option<String>,

        #[arg(long, default_value_t = false)]
        full: bool,

        #[arg(last = true, value_hint = ValueHint::CommandString)]
        command: Vec<String>,
    },

    /// Start a paired two-node Codex workload and inject partner context into both agents
    PairSession {
        #[arg(long = "first-node", alias = "n1")]
        first_node: String,

        #[arg(long = "second-node", alias = "n2")]
        second_node: String,

        #[arg(long = "first-task-note", alias = "t1", value_hint = ValueHint::FilePath)]
        first_task_note: PathBuf,

        #[arg(long = "second-task-note", alias = "t2", value_hint = ValueHint::FilePath)]
        second_task_note: PathBuf,

        #[arg(long = "first-namespace", alias = "ns1")]
        first_namespace: Option<String>,

        #[arg(long = "second-namespace", alias = "ns2")]
        second_namespace: Option<String>,

        #[arg(long = "namespace-prefix", alias = "ns")]
        namespace_prefix: Option<String>,

        #[arg(long)]
        message: Option<String>,

        #[arg(long, default_value_t = 1)]
        retries: usize,

        #[arg(long, alias = "delay-ms", default_value_t = 1500)]
        startup_delay_ms: u64,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,

        #[arg(last = true, value_hint = ValueHint::CommandString)]
        command: Vec<String>,
    },

    /// Send a resume-style migration capsule for an existing session to a node
    Migrate {
        #[arg(long)]
        session: String,

        #[arg(long = "to-node", alias = "to", default_value = "auto")]
        to_node: String,

        #[arg(long = "timeout-seconds", alias = "timeout", default_value_t = 900)]
        timeout_seconds: u64,

        #[arg(long, default_value_t = false)]
        full: bool,
    },

    /// Prepare a new SSH node with jarvisctl/codex wrappers and register it
    Bootstrap {
        name: String,

        #[arg(long)]
        address: Option<String>,

        #[arg(long = "ssh-host", alias = "host")]
        ssh_host: String,

        #[arg(long = "ssh-user", alias = "user")]
        ssh_user: Option<String>,

        #[arg(long = "role", alias = "r")]
        roles: Vec<String>,

        #[arg(long = "label", alias = "lbl")]
        labels: Vec<String>,

        #[arg(long = "workspace-root", alias = "wd", value_hint = ValueHint::DirPath)]
        workspace_root: Option<String>,

        #[arg(long = "max-sessions", alias = "max")]
        max_sessions: Option<usize>,

        #[arg(long = "codex-path", alias = "codex")]
        codex_path: Option<String>,
    },

    /// Inspect a node's vault, memory, tools, work dirs, and stale lease counts
    Inspect {
        name: String,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Restore stale auth leases and prune old visit artifacts on a node
    Cleanup {
        name: String,

        #[arg(long = "max-age-days", alias = "max-age", default_value_t = 7)]
        max_age_days: u64,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Archive/delete completed idle app-server sessions after an operator-visible retention window
    PruneSessions {
        #[arg(long = "max-age-minutes", alias = "max-age", default_value_t = 30)]
        max_age_minutes: u64,

        #[arg(long, default_value_t = false)]
        apply: bool,

        #[arg(long = "include-remote", alias = "remote", default_value_t = true)]
        include_remote: bool,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },
}

#[derive(Subcommand, Debug)]
enum RolloutCommand {
    /// Show rollout status for a Deployment
    Status {
        deployment: String,

        #[arg(short = 'n', long = "resource-namespace", alias = "ns", alias = "rns")]
        resource_namespace: Option<String>,

        #[arg(long, default_value_t = false)]
        watch: bool,

        #[arg(long = "timeout-seconds", alias = "timeout", default_value_t = 300)]
        timeout_seconds: u64,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Show rollout history for a Deployment
    History {
        deployment: String,

        #[arg(short = 'n', long = "resource-namespace", alias = "ns", alias = "rns")]
        resource_namespace: Option<String>,

        #[arg(long, alias = "out", value_enum, default_value_t = ControlPlaneOutput::Table)]
        output: ControlPlaneOutput,
    },

    /// Trigger a rollout restart for a Deployment
    Restart {
        deployment: String,

        #[arg(short = 'n', long = "resource-namespace", alias = "ns", alias = "rns")]
        resource_namespace: Option<String>,
    },

    /// Pause a Deployment rollout
    Pause {
        deployment: String,

        #[arg(short = 'n', long = "resource-namespace", alias = "ns", alias = "rns")]
        resource_namespace: Option<String>,
    },

    /// Resume a paused Deployment rollout
    Resume {
        deployment: String,

        #[arg(short = 'n', long = "resource-namespace", alias = "ns", alias = "rns")]
        resource_namespace: Option<String>,
    },

    /// Roll a Deployment back to a prior revision
    Undo {
        deployment: String,

        #[arg(short = 'n', long = "resource-namespace", alias = "ns", alias = "rns")]
        resource_namespace: Option<String>,

        #[arg(long = "to-revision", alias = "to")]
        to_revision: Option<u64>,
    },
}

#[derive(Subcommand, Debug)]
enum KubeCommand {
    /// Experimental: render the supported adapter subset as native Kubernetes manifests
    Render {
        #[arg(short = 'f', long = "file", alias = "manifest", value_hint = ValueHint::FilePath)]
        file: Vec<PathBuf>,

        #[arg(short = 'k', long = "kustomize", alias = "kustomization", value_hint = ValueHint::DirPath)]
        kustomize: Option<PathBuf>,

        #[arg(long, alias = "out", value_enum, default_value_t = KubernetesRenderOutput::Yaml)]
        output: KubernetesRenderOutput,
    },

    /// Experimental: apply the supported adapter subset onto the active Kubernetes cluster
    Apply {
        #[arg(short = 'f', long = "file", alias = "manifest", value_hint = ValueHint::FilePath)]
        file: Vec<PathBuf>,

        #[arg(short = 'k', long = "kustomize", alias = "kustomization", value_hint = ValueHint::DirPath)]
        kustomize: Option<PathBuf>,

        #[arg(long, alias = "ctx")]
        context: Option<String>,

        #[arg(long = "dry-run-server", default_value_t = false)]
        dry_run_server: bool,
    },

    /// Experimental: control a pod-hosted Codex runtime exposed through Kubernetes
    Runtime {
        #[command(subcommand)]
        command: KubeRuntimeCommand,
    },
}

#[derive(Subcommand, Debug)]
enum KubeRuntimeCommand {
    /// Experimental: fetch live metadata from a pod-hosted Codex runtime
    Metadata {
        #[arg(
            long,
            alias = "deploy",
            required_unless_present = "service",
            conflicts_with = "service"
        )]
        deployment: Option<String>,

        #[arg(
            long,
            required_unless_present = "deployment",
            conflicts_with = "deployment"
        )]
        service: Option<String>,

        #[arg(short = 'n', long = "resource-namespace", alias = "ns", alias = "rns")]
        resource_namespace: String,

        #[arg(long, alias = "ctx")]
        context: Option<String>,

        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Experimental: attach to the live output stream of a pod-hosted Codex runtime
    Attach {
        #[arg(
            long,
            alias = "deploy",
            required_unless_present = "service",
            conflicts_with = "service"
        )]
        deployment: Option<String>,

        #[arg(
            long,
            required_unless_present = "deployment",
            conflicts_with = "deployment"
        )]
        service: Option<String>,

        #[arg(short = 'n', long = "resource-namespace", alias = "ns", alias = "rns")]
        resource_namespace: String,

        #[arg(long, alias = "ctx")]
        context: Option<String>,
    },

    /// Experimental: send text into a pod-hosted Codex runtime
    Tell {
        #[arg(
            long,
            alias = "deploy",
            required_unless_present = "service",
            conflicts_with = "service"
        )]
        deployment: Option<String>,

        #[arg(
            long,
            required_unless_present = "deployment",
            conflicts_with = "deployment"
        )]
        service: Option<String>,

        #[arg(short = 'n', long = "resource-namespace", alias = "ns", alias = "rns")]
        resource_namespace: String,

        #[arg(long, alias = "ctx")]
        context: Option<String>,

        #[arg(long, alias = "f", value_hint = ValueHint::FilePath, conflicts_with = "text")]
        file: Option<String>,

        #[arg(long, conflicts_with = "file")]
        text: Option<String>,

        #[arg(long, value_enum, default_value_t = CodexAppInputMode::Auto)]
        mode: CodexAppInputMode,
    },

    /// Experimental: interrupt the active turn inside a pod-hosted Codex runtime
    Interrupt {
        #[arg(
            long,
            alias = "deploy",
            required_unless_present = "service",
            conflicts_with = "service"
        )]
        deployment: Option<String>,

        #[arg(
            long,
            required_unless_present = "deployment",
            conflicts_with = "deployment"
        )]
        service: Option<String>,

        #[arg(short = 'n', long = "resource-namespace", alias = "ns", alias = "rns")]
        resource_namespace: String,

        #[arg(long, alias = "ctx")]
        context: Option<String>,
    },

    /// Experimental: delete a pod-hosted Codex runtime Deployment and its launch ConfigMap
    Delete {
        #[arg(long)]
        deployment: String,

        #[arg(short = 'n', long = "resource-namespace", alias = "ns", alias = "rns")]
        resource_namespace: String,

        #[arg(long, alias = "ctx")]
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
    match cli.command.unwrap_or(Command::List {
        backend: SessionBackend::Native,
        namespace: None,
        json: false,
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
            deployment,
            runtime_labels,
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
                deployment,
                labels: parse_key_value_pairs(&runtime_labels)?,
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
        Command::Visit {
            node,
            from_node,
            role,
            labels,
            retries,
            text,
            prompt_file,
            from_current,
            working_directory,
            namespace,
            timeout_seconds,
            sandbox,
            model,
            reasoning_effort,
            ephemeral,
            protected_capsule,
            full,
        } => visit_node(
            node,
            from_node,
            role,
            labels,
            retries,
            text,
            prompt_file,
            from_current,
            working_directory,
            namespace,
            timeout_seconds,
            sandbox,
            model,
            reasoning_effort,
            ephemeral,
            protected_capsule,
            full,
        ),
        Command::CapsuleOpen => capsule_open(),
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
        Command::Node { command } => node_command(command),
        Command::Worker { command } => worker_command(command),
        Command::Mission { command } => mission_command(command),
        Command::Proposal { command } => proposal_command(command),
        Command::Pair { command } => pair_command(command),
        Command::Evidence { command } => evidence_command(command),
        Command::Capability { command } => capability_command(command),
        Command::Autonomy { command } => autonomy_command(command),
        Command::Health { output } => health_command(output),
        Command::ProductionSmoke {
            namespace_prefix,
            history,
            limit,
            no_record,
            skip_worker_models,
            skip_evidence,
            output,
        } => production_smoke_command(
            namespace_prefix,
            history,
            limit,
            !no_record,
            skip_worker_models,
            skip_evidence,
            output,
        ),
        Command::OperatorRequest { command } => operator_request_command(command),
        Command::Message { command } => message_command(command),
        Command::Rollout { command } => rollout_command(command),
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
        Command::Delete {
            backend,
            namespace,
            mission,
        } => delete_session(backend, &namespace, mission.as_deref()),
        Command::List {
            backend,
            namespace,
            json,
        } => list_sessions(backend, namespace, json),
        Command::History {
            backend,
            namespace,
            service,
            resource_namespace,
            include_turns,
            json,
        } => {
            let namespace = resolve_runtime_namespace(
                namespace.as_deref(),
                service.as_deref(),
                resource_namespace.as_deref(),
            )?;
            history(backend, &namespace, include_turns, json)
        }
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
            target,
            node,
            namespace,
            service,
            resource_namespace,
            agent,
            file,
            text,
            no_enter,
            mode,
        } => {
            let target = target
                .as_deref()
                .map(parse_runtime_tell_target)
                .transpose()?;
            let node = target
                .as_ref()
                .and_then(|target| target.node.as_deref())
                .or(node.as_deref());
            let agent = target
                .as_ref()
                .and_then(|target| target.agent.as_deref())
                .unwrap_or(&agent);
            let namespace = resolve_tell_runtime_namespace(
                target
                    .as_ref()
                    .and_then(|target| target.namespace.as_deref())
                    .or(namespace.as_deref()),
                service.as_deref(),
                resource_namespace.as_deref(),
            )?;
            tell(
                backend,
                node,
                &namespace,
                agent,
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
        Command::RespondRequest {
            backend,
            namespace,
            request_id,
            response_json,
            error,
            mission,
        } => respond_server_request_command(
            backend,
            &namespace,
            &request_id,
            response_json.as_deref(),
            error.as_deref(),
            mission.as_deref(),
        ),
        Command::NativeSessionServe { manifest } => {
            serve_native_session(manifest).map_err(JarvisError::from)
        }
        Command::CodexAppSessionServe { manifest } => {
            serve_codex_app_session(manifest).map_err(JarvisError::from)
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

#[allow(clippy::too_many_arguments)]
fn visit_node(
    node: String,
    from_node: Option<String>,
    role: Option<String>,
    labels: Vec<String>,
    retries: usize,
    text: Option<String>,
    prompt_file: Option<PathBuf>,
    from_current: bool,
    working_directory: Option<String>,
    namespace: Option<String>,
    timeout_seconds: u64,
    sandbox: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    ephemeral: bool,
    protected_capsule: bool,
    full: bool,
) -> Result<(), JarvisError> {
    let mut prompt = match (text, prompt_file, from_current) {
        (Some(text), None, false) => text,
        (None, Some(path), false) => fs::read_to_string(&path).map_err(|error| {
            JarvisError::Other(anyhow::anyhow!(
                "failed to read visit prompt file '{}': {}",
                path.display(),
                error
            ))
        })?,
        (None, None, true) => build_current_visit_capsule().map_err(JarvisError::from)?,
        (None, None, false) => {
            return Err(JarvisError::Other(anyhow::anyhow!(
                "provide --text, --file, or --from-current for the visit prompt"
            )));
        }
        _ => {
            return Err(JarvisError::Other(anyhow::anyhow!(
                "use only one of --text, --file, or --from-current"
            )));
        }
    };
    if protected_capsule {
        prompt = open_visit_capsule(&prompt).map_err(JarvisError::from)?;
    }
    let policy = load_or_create_orchestration_policy().map_err(JarvisError::from)?;
    let mut effective_labels = policy.default_labels.clone();
    effective_labels.extend(parse_key_value_pairs(&labels)?);

    let result = run_node_visit(NodeVisitOptions {
        retries: if retries != 0 {
            retries
        } else if node == "auto" {
            policy.retries
        } else {
            0
        },
        node,
        from_node,
        role: role.or(Some(policy.default_role)),
        labels: effective_labels,
        prompt,
        working_directory,
        namespace,
        timeout_seconds: if timeout_seconds == 900 {
            policy.timeout_seconds
        } else {
            timeout_seconds
        },
        sandbox_mode: Some(sandbox),
        model,
        reasoning_effort,
        ephemeral,
    })
    .map_err(JarvisError::from)?;

    if full {
        println!(
            "{}",
            serde_json::to_string_pretty(&result).map_err(anyhow::Error::from)?
        );
    } else {
        println!("{}", result.final_message);
        eprintln!(
            "visit={} node={} cleanup={}",
            result.namespace, result.node, result.cleanup_status
        );
    }
    Ok(())
}

fn capsule_open() -> Result<(), JarvisError> {
    let mut raw = String::new();
    io::stdin().read_to_string(&mut raw)?;
    print!("{}", open_visit_capsule(&raw).map_err(JarvisError::from)?);
    Ok(())
}

fn now_millis_for_namespace() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn build_current_visit_capsule() -> anyhow::Result<String> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let hostname = fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();
    let git_status = command_output(
        ProcessCommand::new("git")
            .arg("status")
            .arg("--short")
            .arg("--branch")
            .current_dir(&cwd),
    )
    .unwrap_or_else(|error| format!("git status unavailable: {error}"));
    let live_sessions = command_output(ProcessCommand::new("jarvisctl").arg("list").arg("--json"))
        .unwrap_or_else(|error| format!("jarvisctl list unavailable: {error}"));
    let transcript_tail = latest_codex_transcript_tail(80)
        .unwrap_or_else(|error| format!("latest transcript unavailable: {error}"));

    let transcript_tail = truncate_middle(&transcript_tail, 4_000);

    Ok(format!(
        "Remote visit capsule from the current operator context.\n\nOrigin:\n- hostname: {hostname}\n- cwd: {}\n\nCurrent git status:\n```text\n{}\n```\n\nCurrent Jarvis runtimes:\n```json\n{}\n```\n\nLatest local Codex transcript tail, if available:\n```jsonl\n{}\n```\n\nUse the destination node's own filesystem, vault, memory, and tools. Compare or inspect only what is needed for the user's request, then return concise findings and any recommended next actions.",
        cwd.display(),
        git_status.trim(),
        live_sessions.trim(),
        transcript_tail.trim(),
    ))
}

fn command_output(command: &mut ProcessCommand) -> anyhow::Result<String> {
    let output = command.output().context("failed to run command")?;
    ensure!(
        output.status.success(),
        "command exited with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn latest_codex_transcript_tail(lines: usize) -> anyhow::Result<String> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    let sessions_dir = PathBuf::from(home).join(".codex").join("sessions");
    ensure!(
        sessions_dir.exists(),
        "{} does not exist",
        sessions_dir.display()
    );
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    collect_newest_file(&sessions_dir, &mut newest)?;
    let Some((_, path)) = newest else {
        bail!(
            "no Codex transcript files found under {}",
            sessions_dir.display()
        );
    };
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read transcript '{}'", path.display()))?;
    let all_lines = raw
        .lines()
        .filter(|line| {
            !line.contains("\"type\":\"token_count\"")
                && !line.contains("\"encrypted_content\"")
                && !line.contains("\"cached_input_tokens\"")
        })
        .map(|line| truncate_middle(line, 900))
        .collect::<Vec<_>>();
    let start = all_lines.len().saturating_sub(lines);
    Ok(all_lines[start..].join("\n"))
}

fn truncate_middle(value: &str, max_chars: usize) -> String {
    let total = value.chars().count();
    if total <= max_chars {
        return value.to_string();
    }
    let keep_each_side = max_chars.saturating_sub(80) / 2;
    let prefix = value.chars().take(keep_each_side).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(keep_each_side)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!(
        "{prefix}\n\n[truncated {} chars from middle of latest transcript]\n\n{suffix}",
        total.saturating_sub(keep_each_side * 2)
    )
}

fn collect_newest_file(
    dir: &std::path::Path,
    newest: &mut Option<(std::time::SystemTime, PathBuf)>,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read '{}'", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_newest_file(&path, newest)?;
        } else if metadata.is_file() {
            let modified = metadata
                .modified()
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            let replace = newest
                .as_ref()
                .map(|(current, _)| modified > *current)
                .unwrap_or(true);
            if replace {
                *newest = Some((modified, path));
            }
        }
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

fn worker_command(command: WorkerCommand) -> Result<(), JarvisError> {
    match command {
        WorkerCommand::Validate { output } => {
            println!(
                "{}",
                render_worker_validation_output(output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        WorkerCommand::ValidateModels { all, output } => {
            let report = validate_worker_models(all).map_err(JarvisError::from)?;
            println!(
                "{}",
                render_worker_model_validation_output(&report, output)
                    .map_err(JarvisError::from)?
            );
            if report.status == "failed" {
                return Err(JarvisError::from(anyhow::anyhow!(
                    "{} worker model(s) unavailable",
                    report.unavailable
                )));
            }
            Ok(())
        }
        WorkerCommand::Offload {
            service,
            resource_namespace,
            via_runtime_namespace,
            intent,
            prompt,
            output_path,
            job_name,
            execute,
            output,
        } => {
            let report = run_worker_offload(WorkerOffloadOptions {
                service_name: service,
                control_namespace: resource_namespace,
                via_runtime_namespace,
                prompt,
                intent,
                output_path,
                job_name,
                execute,
            })
            .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_worker_offload_output(&report, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        WorkerCommand::Runs { all, limit, output } => {
            let records = list_worker_run_records(Some(limit), all).map_err(JarvisError::from)?;
            println!(
                "{}",
                render_worker_runs_output(&records, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        WorkerCommand::ShowRun { id, output } => {
            let record = load_worker_run_record(&id).map_err(JarvisError::from)?;
            println!(
                "{}",
                render_worker_runs_output(&[record], output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        WorkerCommand::Artifact { id, all, output } => {
            let report = read_worker_run_artifact(&id, all).map_err(JarvisError::from)?;
            println!(
                "{}",
                render_worker_run_artifact_output(&report, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        WorkerCommand::MarkRun {
            id,
            status,
            note,
            all,
            output,
        } => {
            let record = mark_worker_run(&id, &status, note, all).map_err(JarvisError::from)?;
            println!(
                "{}",
                render_worker_runs_output(&[record], output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        WorkerCommand::DriftSmoke {
            service,
            resource_namespace,
            prompt,
            output_path,
            job_name,
            all,
            output,
        } => {
            let report = run_worker_drift_smoke(WorkerDriftSmokeOptions {
                service_name: service,
                control_namespace: resource_namespace,
                prompt,
                output_path,
                job_name,
                all,
            })
            .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_worker_drift_smoke_output(&report, output).map_err(JarvisError::from)?
            );
            if report.status == "failed" {
                return Err(JarvisError::from(anyhow::anyhow!(
                    "worker drift smoke failed"
                )));
            }
            Ok(())
        }
        WorkerCommand::DriftSchedule {
            service,
            resource_namespace,
            interval_seconds,
            enabled,
            all,
            output,
        } => {
            let status = configure_worker_drift_smoke_schedule(WorkerDriftSmokeScheduleOptions {
                service_name: service,
                namespace: resource_namespace,
                interval_seconds,
                enabled,
                all,
            })
            .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_worker_drift_smoke_schedule_status(&status, output)
                    .map_err(JarvisError::from)?
            );
            Ok(())
        }
        WorkerCommand::DriftStatus { output } => {
            let status = worker_drift_smoke_schedule_status().map_err(JarvisError::from)?;
            println!(
                "{}",
                render_worker_drift_smoke_schedule_status(&status, output)
                    .map_err(JarvisError::from)?
            );
            Ok(())
        }
        WorkerCommand::DriftRun { force, output } => {
            let report = run_recurring_worker_drift_smoke(force).map_err(JarvisError::from)?;
            if let Some(report) = report {
                println!(
                    "{}",
                    render_worker_drift_smoke_output(&report, output).map_err(JarvisError::from)?
                );
            } else {
                let status = worker_drift_smoke_schedule_status().map_err(JarvisError::from)?;
                println!(
                    "{}",
                    render_worker_drift_smoke_schedule_status(&status, output)
                        .map_err(JarvisError::from)?
                );
            }
            Ok(())
        }
        WorkerCommand::PruneRuns {
            max_age_days,
            dry_run,
            apply,
            redact,
            no_redact,
            all,
            output,
        } => {
            let report =
                prune_worker_runs(max_age_days, dry_run && !apply, redact && !no_redact, all)
                    .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_worker_run_prune_output(&report, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
    }
}

fn render_worker_offload_output(
    report: &control_plane::WorkerOffloadReport,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(report).context("failed to encode worker offload")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(report).context("failed to encode worker offload")
        }
        ControlPlaneOutput::Table => Ok(format!(
            "RUN\tJOB\tNAMESPACE\tSERVICE\tPHASE\tWORKER\tOUTPUT\n{}\t{}\t{}\t{}\t{}\t{}\t{}",
            report.run_id,
            report.job_name,
            report.namespace,
            report.service_name,
            report.phase,
            report.worker.as_deref().unwrap_or("-"),
            report.output_path.as_deref().unwrap_or("-")
        )),
    }
}

fn node_command(command: NodeCommand) -> Result<(), JarvisError> {
    match command {
        NodeCommand::Register {
            name,
            address,
            ssh_host,
            ssh_user,
            local,
            roles,
            labels,
            workspace_root,
            max_sessions,
        } => {
            let messages = register_node(NodeRegisterOptions {
                name,
                address,
                ssh_host,
                ssh_user,
                roles,
                labels: parse_key_value_pairs(&labels)?,
                workspace_root,
                max_sessions,
                local,
            })
            .map_err(JarvisError::from)?;
            for message in messages {
                println!("{message}");
            }
            Ok(())
        }
        NodeCommand::Ping { name, output } => {
            println!(
                "{}",
                render_node_probe_output(&name, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        NodeCommand::Cordon { name } => {
            println!(
                "{}",
                set_node_cordoned(&name, true).map_err(JarvisError::from)?
            );
            Ok(())
        }
        NodeCommand::Uncordon { name } => {
            println!(
                "{}",
                set_node_cordoned(&name, false).map_err(JarvisError::from)?
            );
            Ok(())
        }
        NodeCommand::SyncCodexAuth { name } => {
            println!(
                "{}",
                sync_codex_auth_to_node(&name).map_err(JarvisError::from)?
            );
            Ok(())
        }
        NodeCommand::Schedule {
            role,
            labels,
            exclude,
            require_codex_auth,
            output,
        } => {
            let result = schedule_node(NodeScheduleOptions {
                role,
                labels: parse_key_value_pairs(&labels)?,
                exclude,
                require_codex_auth,
            })
            .map_err(JarvisError::from)?;
            match output {
                ControlPlaneOutput::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(anyhow::Error::from)?
                ),
                ControlPlaneOutput::Yaml => {
                    println!(
                        "{}",
                        serde_yaml::to_string(&result).map_err(anyhow::Error::from)?
                    )
                }
                ControlPlaneOutput::Table => {
                    println!("NODE\tTARGET\tSCORE\tREASONS");
                    println!(
                        "{}\t{}\t{}\t{}",
                        result.node,
                        result.target,
                        result.score,
                        result.reasons.join(",")
                    );
                }
            }
            Ok(())
        }
        NodeCommand::Doctor { output } => {
            let result = doctor_nodes().map_err(JarvisError::from)?;
            match output {
                ControlPlaneOutput::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(anyhow::Error::from)?
                ),
                ControlPlaneOutput::Yaml => {
                    println!(
                        "{}",
                        serde_yaml::to_string(&result).map_err(anyhow::Error::from)?
                    )
                }
                ControlPlaneOutput::Table => {
                    println!("NODE\tAVAILABLE\tSCHEDULABLE\tISSUES");
                    for check in result {
                        println!(
                            "{}\t{}\t{}\t{}",
                            check.node,
                            check.available,
                            check.schedulable,
                            if check.issues.is_empty() {
                                "-".to_string()
                            } else {
                                check.issues.join(",")
                            }
                        );
                    }
                }
            }
            Ok(())
        }
        NodeCommand::Heartbeat { name, output } => {
            let report = heartbeat_node(name.as_deref()).map_err(JarvisError::from)?;
            match output {
                ControlPlaneOutput::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(anyhow::Error::from)?
                ),
                ControlPlaneOutput::Yaml => {
                    println!(
                        "{}",
                        serde_yaml::to_string(&report).map_err(anyhow::Error::from)?
                    )
                }
                ControlPlaneOutput::Table => {
                    println!("NODE\tTARGET\tHEARTBEAT_EPOCH_MS\tPATH");
                    println!(
                        "{}\t{}\t{}\t{}",
                        report.node, report.target, report.heartbeat_epoch_ms, report.path
                    );
                }
            }
            Ok(())
        }
        NodeCommand::InstallHeartbeatUserService {
            interval_seconds,
            enable,
            start,
            output,
        } => {
            let report = install_node_heartbeat_user_service(interval_seconds, enable, start)
                .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_node_heartbeat_service_install(&report, output)
                    .map_err(JarvisError::from)?
            );
            Ok(())
        }
        NodeCommand::HeartbeatServiceStatus { output } => {
            let status = node_heartbeat_service_status().map_err(JarvisError::from)?;
            println!(
                "{}",
                render_node_heartbeat_service_status(&status, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        NodeCommand::CollectTaskNote {
            name,
            namespace,
            task_note,
            destination,
            output,
        } => {
            let report =
                collect_node_task_note(&name, &namespace, &task_note, destination.as_deref())
                    .map_err(JarvisError::from)?;
            match output {
                ControlPlaneOutput::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(anyhow::Error::from)?
                ),
                ControlPlaneOutput::Yaml => {
                    println!(
                        "{}",
                        serde_yaml::to_string(&report).map_err(anyhow::Error::from)?
                    )
                }
                ControlPlaneOutput::Table => {
                    println!("NODE\tNAMESPACE\tCOLLECTED\tDESTINATION\tDETAIL");
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        report.node,
                        report.namespace,
                        report.collected,
                        report.destination,
                        report.detail
                    );
                }
            }
            Ok(())
        }
        NodeCommand::Preflight { output } => {
            let result = preflight_nodes().map_err(JarvisError::from)?;
            match output {
                ControlPlaneOutput::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(anyhow::Error::from)?
                ),
                ControlPlaneOutput::Yaml => {
                    println!(
                        "{}",
                        serde_yaml::to_string(&result).map_err(anyhow::Error::from)?
                    )
                }
                ControlPlaneOutput::Table => {
                    println!("OK\tISSUES");
                    println!(
                        "{}\t{}",
                        result.ok,
                        if result.issues.is_empty() {
                            "-".to_string()
                        } else {
                            result.issues.join(",")
                        }
                    );
                    println!();
                    println!("NODE\tAVAILABLE\tSCHEDULABLE\tJARVISCTL\tCODEX\tISSUES");
                    for check in result.doctors {
                        println!(
                            "{}\t{}\t{}\t{}\t{}\t{}",
                            check.node,
                            check.available,
                            check.schedulable,
                            check
                                .facts
                                .get("jarvisctl")
                                .map(String::as_str)
                                .unwrap_or("-"),
                            check
                                .facts
                                .get("codex_cli")
                                .map(String::as_str)
                                .unwrap_or("-"),
                            if check.issues.is_empty() {
                                "-".to_string()
                            } else {
                                check.issues.join(",")
                            }
                        );
                    }
                    println!();
                    println!("FROM\tTO\tOK\tCLASS");
                    for link in result.links {
                        println!(
                            "{}\t{}\t{}\t{}",
                            link.from,
                            link.to,
                            link.ok,
                            link.failure_class.unwrap_or_else(|| "-".to_string())
                        );
                    }
                }
            }
            Ok(())
        }
        NodeCommand::Links { from, to, output } => {
            let result =
                check_node_links(NodeLinksOptions { from, to }).map_err(JarvisError::from)?;
            match output {
                ControlPlaneOutput::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(anyhow::Error::from)?
                ),
                ControlPlaneOutput::Yaml => {
                    println!(
                        "{}",
                        serde_yaml::to_string(&result).map_err(anyhow::Error::from)?
                    )
                }
                ControlPlaneOutput::Table => {
                    println!("FROM\tTO\tOK\tEXIT\tCLASS\tAUTH_URL\tDETAIL");
                    for check in result {
                        println!(
                            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                            check.from,
                            check.to,
                            check.ok,
                            check.exit_status,
                            check.failure_class.unwrap_or_else(|| "-".to_string()),
                            check.auth_url.unwrap_or_else(|| "-".to_string()),
                            check.detail.replace('\n', "\\n")
                        );
                    }
                }
            }
            Ok(())
        }
        NodeCommand::Sudo {
            name,
            command,
            password_stdin,
            timeout_seconds,
            output,
        } => {
            if !password_stdin {
                return Err(JarvisError::Other(anyhow::anyhow!(
                    "--password-stdin is required so jarvisctl never prompts or logs credentials"
                )));
            }
            let mut password = String::new();
            io::stdin()
                .read_line(&mut password)
                .map_err(JarvisError::from)?;
            let report = run_node_sudo(NodeSudoOptions {
                node: name,
                command,
                password: password.trim_end_matches(['\r', '\n']).to_string(),
                timeout_seconds,
            })
            .map_err(JarvisError::from)?;
            let rendered = render_node_sudo_output(&report, output).map_err(JarvisError::from)?;
            println!("{rendered}");
            if report.exit_status == 0 {
                Ok(())
            } else {
                Err(JarvisError::Other(anyhow::anyhow!(
                    "sudo command on Node '{}' failed with exit status {}",
                    report.node,
                    report.exit_status
                )))
            }
        }
        NodeCommand::Policy { output } => {
            let policy = load_or_create_orchestration_policy().map_err(JarvisError::from)?;
            match output {
                ControlPlaneOutput::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&policy).map_err(anyhow::Error::from)?
                ),
                ControlPlaneOutput::Yaml => {
                    println!(
                        "# {}\n{}",
                        orchestration_policy_path()
                            .map_err(JarvisError::from)?
                            .display(),
                        serde_yaml::to_string(&policy).map_err(anyhow::Error::from)?
                    )
                }
                ControlPlaneOutput::Table => {
                    println!("KEY\tVALUE");
                    println!("default_role\t{}", policy.default_role);
                    println!("retries\t{}", policy.retries);
                    println!("timeout_seconds\t{}", policy.timeout_seconds);
                    println!("fanout_max_concurrency\t{}", policy.fanout_max_concurrency);
                    println!("cleanup_retention_days\t{}", policy.cleanup_retention_days);
                    println!(
                        "remote_index_timeout_seconds\t{}",
                        policy.remote_index_timeout_seconds
                    );
                    if policy.default_labels.is_empty() {
                        println!("default_labels\t-");
                    } else {
                        println!(
                            "default_labels\t{}",
                            policy
                                .default_labels
                                .iter()
                                .map(|(key, value)| format!("{key}={value}"))
                                .collect::<Vec<_>>()
                                .join(",")
                        );
                    }
                }
            }
            Ok(())
        }
        NodeCommand::Reconcile {
            max_age_days,
            output,
        } => {
            let policy = load_or_create_orchestration_policy().map_err(JarvisError::from)?;
            let result = reconcile_nodes(max_age_days.unwrap_or(policy.cleanup_retention_days))
                .map_err(JarvisError::from)?;
            match output {
                ControlPlaneOutput::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(anyhow::Error::from)?
                ),
                ControlPlaneOutput::Yaml => {
                    println!(
                        "{}",
                        serde_yaml::to_string(&result).map_err(anyhow::Error::from)?
                    )
                }
                ControlPlaneOutput::Table => {
                    println!("DOCTOR\tAVAILABLE\tSCHEDULABLE\tISSUES");
                    for check in &result.doctors {
                        println!(
                            "{}\t{}\t{}\t{}",
                            check.node,
                            check.available,
                            check.schedulable,
                            if check.issues.is_empty() {
                                "-".to_string()
                            } else {
                                check.issues.join(",")
                            }
                        );
                    }
                    println!("CLEANUP\tRESTORED\tSKIPPED\tREMOVED");
                    for cleanup in &result.cleanups {
                        println!(
                            "{}\t{}\t{}\t{}",
                            cleanup.node,
                            cleanup.restored_leases.join(","),
                            cleanup.skipped_active_leases.join(","),
                            cleanup.removed_visit_artifacts
                        );
                    }
                    for failure in &result.failures {
                        eprintln!(
                            "reconcile_failed node={} error={}",
                            failure.node, failure.error
                        );
                    }
                }
            }
            Ok(())
        }
        NodeCommand::RotateCapsuleKey { no_sync, output } => {
            let result = rotate_capsule_key(!no_sync).map_err(JarvisError::from)?;
            match output {
                ControlPlaneOutput::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(anyhow::Error::from)?
                ),
                ControlPlaneOutput::Yaml => {
                    println!(
                        "{}",
                        serde_yaml::to_string(&result).map_err(anyhow::Error::from)?
                    )
                }
                ControlPlaneOutput::Table => {
                    println!("KEY_PATH\tSYNCED_NODES\tFAILURES");
                    println!(
                        "{}\t{}\t{}",
                        result.key_path,
                        if result.synced_nodes.is_empty() {
                            "-".to_string()
                        } else {
                            result.synced_nodes.join(",")
                        },
                        if result.failures.is_empty() {
                            "-".to_string()
                        } else {
                            result
                                .failures
                                .iter()
                                .map(|failure| format!("{}:{}", failure.node, failure.error))
                                .collect::<Vec<_>>()
                                .join(",")
                        }
                    );
                }
            }
            Ok(())
        }
        NodeCommand::Index { output } => {
            let result = cluster_index().map_err(JarvisError::from)?;
            match output {
                ControlPlaneOutput::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(anyhow::Error::from)?
                ),
                ControlPlaneOutput::Yaml => {
                    println!(
                        "{}",
                        serde_yaml::to_string(&result).map_err(anyhow::Error::from)?
                    )
                }
                ControlPlaneOutput::Table => {
                    println!("SESSIONS:");
                    if result.sessions.is_empty() {
                        println!("(none)");
                    } else {
                        println!("NAMESPACE\tBACKEND\tNODE\tCREATED");
                        for session in result.sessions {
                            let node = session
                                .context
                                .as_ref()
                                .and_then(|context| context.labels.get("jarvisctl.io/node"))
                                .cloned()
                                .unwrap_or_else(|| "local".to_string());
                            println!(
                                "{}\t{}\t{}\t{}",
                                session.namespace,
                                session.backend,
                                node,
                                session.created_at_epoch_ms
                            );
                        }
                    }
                    println!("VISITS:");
                    if result.visits.is_empty() {
                        println!("(none)");
                    } else {
                        println!(
                            "NAMESPACE\tSTATUS\tNODE\tFROM\tINDEX_SOURCE\tSTARTED\tEXIT\tCLASS\tRETRYABLE"
                        );
                        for visit in result.visits {
                            println!(
                                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                                visit.namespace,
                                visit.status,
                                visit.node,
                                visit.from_node.unwrap_or_else(|| "-".to_string()),
                                visit.index_source.unwrap_or_else(|| "local".to_string()),
                                visit.started_at_epoch_ms,
                                visit
                                    .exit_status
                                    .map(|value| value.to_string())
                                    .unwrap_or_else(|| "-".to_string()),
                                visit.failure_class.unwrap_or_else(|| "-".to_string()),
                                visit
                                    .retryable
                                    .map(|value| value.to_string())
                                    .unwrap_or_else(|| "-".to_string())
                            );
                        }
                    }
                }
            }
            Ok(())
        }
        NodeCommand::Audit { limit, output } => {
            let result = read_auth_audit_events(Some(limit)).map_err(JarvisError::from)?;
            match output {
                ControlPlaneOutput::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(anyhow::Error::from)?
                ),
                ControlPlaneOutput::Yaml => {
                    println!(
                        "{}",
                        serde_yaml::to_string(&result).map_err(anyhow::Error::from)?
                    )
                }
                ControlPlaneOutput::Table => {
                    println!("TS\tEVENT\tNODE\tNAMESPACE\tSTATUS\tDETAIL");
                    for event in result {
                        println!(
                            "{}\t{}\t{}\t{}\t{}\t{}",
                            event.ts_epoch_ms,
                            event.event,
                            event.node,
                            event.namespace,
                            event.status,
                            event.detail
                        );
                    }
                }
            }
            Ok(())
        }
        NodeCommand::Fanout {
            nodes,
            role,
            labels,
            text,
            prompt_file,
            timeout_seconds,
            sandbox,
            model,
            reasoning_effort,
            ephemeral,
            max_concurrency,
            full,
        } => {
            let prompt = match (text, prompt_file) {
                (Some(text), None) => text,
                (None, Some(path)) => fs::read_to_string(&path).map_err(|error| {
                    JarvisError::Other(anyhow::anyhow!(
                        "failed to read fanout prompt file '{}': {}",
                        path.display(),
                        error
                    ))
                })?,
                (None, None) => {
                    return Err(JarvisError::Other(anyhow::anyhow!(
                        "provide --text or --file for fanout"
                    )));
                }
                _ => {
                    return Err(JarvisError::Other(anyhow::anyhow!(
                        "use only one of --text or --file for fanout"
                    )));
                }
            };
            let result = run_node_fanout(NodeFanoutOptions {
                nodes,
                role,
                labels: {
                    let policy =
                        load_or_create_orchestration_policy().map_err(JarvisError::from)?;
                    let mut effective = policy.default_labels;
                    effective.extend(parse_key_value_pairs(&labels)?);
                    effective
                },
                prompt,
                timeout_seconds: {
                    let policy =
                        load_or_create_orchestration_policy().map_err(JarvisError::from)?;
                    if timeout_seconds == 900 {
                        policy.timeout_seconds
                    } else {
                        timeout_seconds
                    }
                },
                sandbox_mode: Some(sandbox),
                model,
                reasoning_effort,
                ephemeral,
                max_concurrency: {
                    let policy =
                        load_or_create_orchestration_policy().map_err(JarvisError::from)?;
                    if max_concurrency == 4 {
                        policy.fanout_max_concurrency
                    } else {
                        max_concurrency
                    }
                },
            })
            .map_err(JarvisError::from)?;
            if full {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(anyhow::Error::from)?
                );
            } else {
                println!("NODE\tSTATUS\tNAMESPACE\tMESSAGE");
                for visit in &result.results {
                    println!(
                        "{}\tok\t{}\t{}",
                        visit.node,
                        visit.namespace,
                        visit.final_message.replace('\n', "\\n")
                    );
                }
                for failure in &result.failures {
                    println!(
                        "{}\tfailed\t-\t{}",
                        failure.node,
                        failure.error.replace('\n', "\\n")
                    );
                }
            }
            Ok(())
        }
        NodeCommand::Task {
            text,
            prompt_file,
            role,
            labels,
            retries,
            timeout_seconds,
            sandbox,
            model,
            reasoning_effort,
            ephemeral,
            full,
        } => {
            let prompt = match (text, prompt_file) {
                (Some(text), None) => text,
                (None, Some(path)) => fs::read_to_string(&path).map_err(|error| {
                    JarvisError::Other(anyhow::anyhow!(
                        "failed to read task prompt file '{}': {}",
                        path.display(),
                        error
                    ))
                })?,
                (None, None) => {
                    return Err(JarvisError::Other(anyhow::anyhow!(
                        "provide --text or --file for task"
                    )));
                }
                _ => {
                    return Err(JarvisError::Other(anyhow::anyhow!(
                        "use only one of --text or --file for task"
                    )));
                }
            };
            let result = run_node_visit(NodeVisitOptions {
                node: "auto".to_string(),
                from_node: None,
                role: {
                    let policy =
                        load_or_create_orchestration_policy().map_err(JarvisError::from)?;
                    role.or(Some(policy.default_role))
                },
                labels: {
                    let policy =
                        load_or_create_orchestration_policy().map_err(JarvisError::from)?;
                    let mut effective = policy.default_labels;
                    effective.extend(parse_key_value_pairs(&labels)?);
                    effective
                },
                retries: {
                    let policy =
                        load_or_create_orchestration_policy().map_err(JarvisError::from)?;
                    if retries == 1 {
                        policy.retries
                    } else {
                        retries
                    }
                },
                prompt,
                working_directory: None,
                namespace: Some(format!("task-{}", now_millis_for_namespace())),
                timeout_seconds: {
                    let policy =
                        load_or_create_orchestration_policy().map_err(JarvisError::from)?;
                    if timeout_seconds == 900 {
                        policy.timeout_seconds
                    } else {
                        timeout_seconds
                    }
                },
                sandbox_mode: Some(sandbox),
                model,
                reasoning_effort,
                ephemeral,
            })
            .map_err(JarvisError::from)?;
            if full {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(anyhow::Error::from)?
                );
            } else {
                println!("{}", result.final_message);
                eprintln!(
                    "task_visit={} node={} cleanup={}",
                    result.namespace, result.node, result.cleanup_status
                );
            }
            Ok(())
        }
        NodeCommand::StartSession {
            node,
            role,
            labels,
            retries,
            task_note,
            namespace,
            resume_session_id,
            resume_latest,
            working_directory,
            message,
            startup_delay_ms,
            mission,
            full,
            command,
        } => {
            let policy = load_or_create_orchestration_policy().map_err(JarvisError::from)?;
            let mut effective_labels = policy.default_labels.clone();
            effective_labels.extend(parse_key_value_pairs(&labels)?);
            let task_note_for_event = task_note.clone();
            let result = start_node_session(NodeStartSessionOptions {
                node,
                role: role.or(Some(policy.default_role)),
                labels: effective_labels,
                retries: retries.unwrap_or(policy.retries),
                task_note,
                namespace,
                fresh_session: !resume_latest,
                resume_session_id,
                working_directory,
                message,
                startup_delay_ms,
                command,
            })
            .map_err(JarvisError::from)?;
            append_cli_mission_event(
                mission.as_deref(),
                "task",
                if result.exit_status == 0 {
                    "running"
                } else {
                    "failed"
                },
                format!(
                    "Remote Codex session '{}' on Node '{}' {}.",
                    result.namespace,
                    result.node,
                    if result.exit_status == 0 {
                        "started"
                    } else {
                        "failed to start"
                    }
                ),
                Some(task_note_for_event),
                Some(result.namespace.clone()),
                Some(result.node.clone()),
                None,
                None,
                vec![format!("remote-session:{}", result.namespace)],
            )?;
            if full {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(anyhow::Error::from)?
                );
            } else if result.exit_status == 0 {
                println!(
                    "remote_session={} node={} status=started",
                    result.namespace, result.node
                );
                if !result.stdout.trim().is_empty() {
                    println!("{}", result.stdout.trim());
                }
            } else {
                println!(
                    "remote_session={} node={} status=failed class={} retryable={}",
                    result.namespace,
                    result.node,
                    result.failure_class.as_deref().unwrap_or("unknown"),
                    result.retryable
                );
                if !result.stderr.trim().is_empty() {
                    eprintln!("{}", result.stderr.trim());
                }
            }
            if result.exit_status == 0 {
                Ok(())
            } else {
                Err(JarvisError::Other(anyhow::anyhow!(
                    "remote session '{}' on Node '{}' failed with exit status {}",
                    result.namespace,
                    result.node,
                    result.exit_status
                )))
            }
        }
        NodeCommand::PairSession {
            first_node,
            second_node,
            first_task_note,
            second_task_note,
            first_namespace,
            second_namespace,
            namespace_prefix,
            message,
            retries,
            startup_delay_ms,
            output,
            command,
        } => {
            let result = start_node_pair_session(NodePairSessionOptions {
                first_node,
                second_node,
                first_task_note,
                second_task_note,
                first_namespace,
                second_namespace,
                namespace_prefix,
                message,
                startup_delay_ms,
                retries,
                command,
            })
            .map_err(JarvisError::from)?;
            match output {
                ControlPlaneOutput::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(anyhow::Error::from)?
                ),
                ControlPlaneOutput::Yaml => {
                    println!(
                        "{}",
                        serde_yaml::to_string(&result).map_err(anyhow::Error::from)?
                    )
                }
                ControlPlaneOutput::Table => {
                    println!("COORDINATION\tNOTE\tROLE\tNODE\tNAMESPACE\tTASK_NOTE\tSTATUS");
                    for member in result.members {
                        let status = if member.exit_status == 0 {
                            "started".to_string()
                        } else {
                            format!(
                                "failed:{}",
                                member.failure_class.as_deref().unwrap_or("unknown")
                            )
                        };
                        println!(
                            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                            result.coordination_id,
                            result.coordination_note,
                            member.role,
                            member.node,
                            member.namespace,
                            member.task_note,
                            status
                        );
                    }
                }
            }
            Ok(())
        }
        NodeCommand::Migrate {
            session,
            to_node,
            timeout_seconds,
            full,
        } => {
            let target = if to_node == "auto" {
                schedule_node(NodeScheduleOptions {
                    role: Some("worker".to_string()),
                    require_codex_auth: true,
                    ..NodeScheduleOptions::default()
                })
                .map_err(JarvisError::from)?
                .node
            } else {
                to_node
            };
            let result = migrate_session_to_node(&session, &target, timeout_seconds)
                .map_err(JarvisError::from)?;
            if full {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(anyhow::Error::from)?
                );
            } else {
                println!("{}", result.final_message);
                eprintln!(
                    "migration_visit={} node={} cleanup={}",
                    result.namespace, result.node, result.cleanup_status
                );
            }
            Ok(())
        }
        NodeCommand::Bootstrap {
            name,
            address,
            ssh_host,
            ssh_user,
            roles,
            labels,
            workspace_root,
            max_sessions,
            codex_path,
        } => {
            let messages = bootstrap_node(NodeBootstrapOptions {
                name,
                address,
                ssh_host,
                ssh_user,
                roles,
                labels: parse_key_value_pairs(&labels)?,
                workspace_root,
                max_sessions,
                codex_path,
            })
            .map_err(JarvisError::from)?;
            for message in messages {
                println!("{message}");
            }
            Ok(())
        }
        NodeCommand::Inspect { name, output } => {
            let result = inspect_node(&name).map_err(JarvisError::from)?;
            match output {
                ControlPlaneOutput::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(anyhow::Error::from)?
                ),
                ControlPlaneOutput::Yaml => {
                    println!(
                        "{}",
                        serde_yaml::to_string(&result).map_err(anyhow::Error::from)?
                    )
                }
                ControlPlaneOutput::Table => {
                    println!("NODE\tTARGET\tAVAILABLE\tKEY\tVALUE");
                    for (key, value) in result.facts {
                        println!(
                            "{}\t{}\t{}\t{}\t{}",
                            result.node, result.target, result.available, key, value
                        );
                    }
                }
            }
            Ok(())
        }
        NodeCommand::Cleanup {
            name,
            max_age_days,
            output,
        } => {
            let result = cleanup_node(&name, max_age_days).map_err(JarvisError::from)?;
            match output {
                ControlPlaneOutput::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(anyhow::Error::from)?
                ),
                ControlPlaneOutput::Yaml => {
                    println!(
                        "{}",
                        serde_yaml::to_string(&result).map_err(anyhow::Error::from)?
                    )
                }
                ControlPlaneOutput::Table => {
                    println!(
                        "NODE\tTARGET\tRESTORED_LEASES\tSKIPPED_ACTIVE_LEASES\tREMOVED_VISIT_ARTIFACTS"
                    );
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        result.node,
                        result.target,
                        result.restored_leases.join(","),
                        result.skipped_active_leases.join(","),
                        result.removed_visit_artifacts
                    );
                }
            }
            Ok(())
        }
        NodeCommand::PruneSessions {
            max_age_minutes,
            apply,
            include_remote,
            output,
        } => {
            let result = prune_completed_runtime_sessions(max_age_minutes, apply, include_remote)
                .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_runtime_prune_output(&result, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
    }
}

fn parse_key_value_pairs(
    values: &[String],
) -> Result<std::collections::BTreeMap<String, String>, JarvisError> {
    let mut parsed = std::collections::BTreeMap::new();
    for value in values {
        let Some((key, val)) = value.split_once('=') else {
            return Err(JarvisError::Other(anyhow::anyhow!(
                "expected KEY=VALUE, got '{}'",
                value
            )));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(JarvisError::Other(anyhow::anyhow!(
                "label key must not be empty"
            )));
        }
        parsed.insert(key.to_string(), val.trim().to_string());
    }
    Ok(parsed)
}

fn mission_command(command: MissionCommand) -> Result<(), JarvisError> {
    match command {
        MissionCommand::Create {
            title,
            template,
            objective,
            priority,
            owner,
            labels,
            tickets,
            namespaces,
            nodes,
            output,
        } => {
            let mission = create_mission(MissionCreateOptions {
                title,
                template,
                objective,
                priority,
                owner,
                labels: parse_key_value_pairs(&labels)?,
                tickets,
                namespaces,
                nodes,
            })
            .map_err(JarvisError::from)?;
            let detail = show_mission(&mission.id).map_err(JarvisError::from)?;
            println!(
                "{}",
                render_mission_detail_output(&detail, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        MissionCommand::List { output } => {
            let missions = list_missions().map_err(JarvisError::from)?;
            println!(
                "{}",
                render_missions_output(&missions, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        MissionCommand::Templates { output } => {
            println!(
                "{}",
                render_mission_templates_output(output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        MissionCommand::Plan { id, output } => {
            let missions = list_missions().map_err(JarvisError::from)?;
            let proposals = list_proposals().map_err(JarvisError::from)?;
            let plans = plan_missions(&missions, &proposals, id.as_deref());
            println!(
                "{}",
                render_mission_plans_output(&plans, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        MissionCommand::Policy { output } => {
            let rules = default_autonomy_policy();
            println!(
                "{}",
                render_autonomy_policy_output(&rules, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        MissionCommand::Scorecards { output } => {
            let missions = list_missions().map_err(JarvisError::from)?;
            let proposals = list_proposals().map_err(JarvisError::from)?;
            let scorecards = worker_lane_scorecards(&missions, &proposals);
            println!(
                "{}",
                render_worker_lane_scorecards_output(&scorecards, output)
                    .map_err(JarvisError::from)?
            );
            Ok(())
        }
        MissionCommand::Smoke {
            first_node,
            second_node,
            first_task_note,
            second_task_note,
            namespace_prefix,
            dry_run,
            execute,
            output,
            command,
        } => {
            let report = run_two_node_mission_smoke(
                first_node,
                second_node,
                first_task_note,
                second_task_note,
                namespace_prefix,
                dry_run,
                execute,
                command,
            )
            .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_mission_smoke_output(&report, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        MissionCommand::SmokeSchedule {
            first_node,
            second_node,
            first_task_note,
            second_task_note,
            namespace_prefix,
            interval_seconds,
            execute,
            enabled,
            output,
        } => {
            let status = configure_recurring_mission_smoke(RecurringMissionSmokeConfigureOptions {
                first_node,
                second_node,
                first_task_note,
                second_task_note,
                namespace_prefix,
                interval_seconds,
                execute,
                enabled,
            })
            .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_recurring_mission_smoke_status(&status, output)
                    .map_err(JarvisError::from)?
            );
            Ok(())
        }
        MissionCommand::SmokeStatus { output } => {
            let status = recurring_mission_smoke_status().map_err(JarvisError::from)?;
            println!(
                "{}",
                render_recurring_mission_smoke_status(&status, output)
                    .map_err(JarvisError::from)?
            );
            Ok(())
        }
        MissionCommand::SmokeRun { force, output } => {
            let report = run_recurring_mission_smoke(force).map_err(JarvisError::from)?;
            if let Some(report) = report {
                println!(
                    "{}",
                    render_mission_smoke_output(&report, output).map_err(JarvisError::from)?
                );
            } else {
                let status = recurring_mission_smoke_status().map_err(JarvisError::from)?;
                println!(
                    "{}",
                    render_recurring_mission_smoke_status(&status, output)
                        .map_err(JarvisError::from)?
                );
            }
            Ok(())
        }
        MissionCommand::Show { id, output } => {
            let detail = show_mission(&id).map_err(JarvisError::from)?;
            println!(
                "{}",
                render_mission_detail_output(&detail, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        MissionCommand::Event {
            id,
            stage,
            status,
            summary,
            ticket,
            namespace,
            node,
            visit,
            approval,
            evidence,
            output,
        } => {
            let detail = append_mission_event(MissionEventOptions {
                mission_id: id,
                stage,
                status,
                summary,
                ticket,
                namespace,
                node,
                visit,
                approval,
                evidence,
            })
            .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_mission_detail_output(&detail, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        MissionCommand::Complete {
            id,
            status,
            outcome,
            evidence,
            output,
        } => {
            let detail =
                complete_mission(&id, &status, &outcome, evidence).map_err(JarvisError::from)?;
            println!(
                "{}",
                render_mission_detail_output(&detail, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
    }
}

fn capability_command(command: CapabilityCommand) -> Result<(), JarvisError> {
    match command {
        CapabilityCommand::List { output } => {
            let records = list_capabilities().map_err(JarvisError::from)?;
            println!(
                "{}",
                render_capabilities_output(&records, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        CapabilityCommand::Show { id, output } => {
            let record = show_capability(&id).map_err(JarvisError::from)?;
            println!(
                "{}",
                render_capability_output(&record, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        CapabilityCommand::Register {
            id,
            title,
            lane,
            description,
            status,
            confidence,
            schedulable,
            evidence,
            gaps,
            output,
        } => {
            let record = register_capability(CapabilityRegisterOptions {
                id,
                title,
                lane,
                status,
                confidence,
                schedulable,
                description,
                evidence,
                gaps,
            })
            .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_capability_output(&record, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        CapabilityCommand::Validate { id, output } => {
            let reports = if let Some(id) = id {
                vec![validate_capability(&id).map_err(JarvisError::from)?]
            } else {
                validate_capabilities().map_err(JarvisError::from)?
            };
            println!(
                "{}",
                render_capability_validation_output(&reports, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
    }
}

fn autonomy_command(command: AutonomyCommand) -> Result<(), JarvisError> {
    match command {
        AutonomyCommand::Reconcile {
            notify,
            dry_run,
            output,
        } => {
            let missions = list_missions().map_err(JarvisError::from)?;
            let proposals = list_proposals().map_err(JarvisError::from)?;
            let report = reconcile_autonomy(&missions, &proposals, notify, dry_run)
                .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_autonomy_reconcile_output(&report, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        AutonomyCommand::Daemon {
            interval_seconds,
            notify,
            once,
        } => run_autonomy_daemon(
            AutonomyDaemonOptions {
                interval_seconds,
                notify,
                once,
            },
            |notify, dry_run| {
                let missions = list_missions()?;
                let proposals = list_proposals()?;
                reconcile_from_records(&missions, &proposals, notify, dry_run)
            },
        )
        .map_err(JarvisError::from),
        AutonomyCommand::InstallUserService {
            interval_seconds,
            notify,
            enable,
            start,
            request_linger,
            output,
        } => {
            let report = install_autonomy_user_service(AutonomyServiceInstallOptions {
                interval_seconds,
                notify,
                enable,
                start,
                request_linger,
            })
            .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_autonomy_service_install(&report, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        AutonomyCommand::ServiceStatus { output } => {
            let status = autonomy_service_status().map_err(JarvisError::from)?;
            println!(
                "{}",
                render_autonomy_service_status(&status, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        AutonomyCommand::UninstallUserService { output } => {
            let status = uninstall_autonomy_user_service().map_err(JarvisError::from)?;
            println!(
                "{}",
                render_autonomy_service_status(&status, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
    }
}

fn proposal_command(command: ProposalCommand) -> Result<(), JarvisError> {
    match command {
        ProposalCommand::Create {
            title,
            mission,
            action,
            rationale,
            risk,
            proposed_by,
            evidence,
            output,
        } => {
            let proposal = create_proposal(ProposalCreateOptions {
                title,
                mission_id: mission,
                action,
                rationale,
                risk,
                proposed_by,
                evidence,
            })
            .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_proposal_output(&proposal, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        ProposalCommand::List { output } => {
            let proposals = list_proposals().map_err(JarvisError::from)?;
            println!(
                "{}",
                render_proposals_output(&proposals, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        ProposalCommand::Show { id, output } => {
            let proposal = show_proposal(&id).map_err(JarvisError::from)?;
            println!(
                "{}",
                render_proposal_output(&proposal, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        ProposalCommand::Decide {
            id,
            status,
            decision,
            decided_by,
            output,
        } => {
            let proposal = decide_proposal(ProposalDecisionOptions {
                id,
                status,
                decision,
                decided_by,
            })
            .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_proposal_output(&proposal, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
    }
}

fn pair_command(command: PairCommand) -> Result<(), JarvisError> {
    match command {
        PairCommand::Ledger {
            include_archived,
            output,
        } => {
            let ledgers = list_pair_ledgers(include_archived).map_err(JarvisError::from)?;
            println!(
                "{}",
                render_pair_ledgers_output(&ledgers, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        PairCommand::Export { id, output } => {
            let export = export_pair_ledger(&id).map_err(JarvisError::from)?;
            println!(
                "{}",
                render_pair_export_output(&export, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        PairCommand::Demo {
            first_node,
            second_node,
            namespace_prefix,
            execute,
            startup_delay_ms,
            output,
            command,
        } => {
            let report = start_pair_demo(PairDemoOptions {
                first_node,
                second_node,
                namespace_prefix,
                execute,
                startup_delay_ms,
                command,
            })
            .map_err(JarvisError::from)?;
            match output {
                ControlPlaneOutput::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(anyhow::Error::from)?
                ),
                ControlPlaneOutput::Yaml => {
                    println!(
                        "{}",
                        serde_yaml::to_string(&report).map_err(anyhow::Error::from)?
                    )
                }
                ControlPlaneOutput::Table => {
                    println!("PAIR\tEXECUTED\tFIRST_TASK\tSECOND_TASK\tCOMMAND");
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        report.id,
                        report.executed,
                        report.first_task_note,
                        report.second_task_note,
                        report.command
                    );
                }
            }
            Ok(())
        }
        PairCommand::RunDemo {
            first_node,
            second_node,
            namespace_prefix,
            execute,
            startup_delay_ms,
            output,
            command,
        } => {
            let report = run_pair_demo_sequence(PairDemoSequenceOptions {
                first_node,
                second_node,
                namespace_prefix,
                execute,
                startup_delay_ms,
                command,
            })
            .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_pair_demo_sequence_output(&report, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        PairCommand::CleanupDemos {
            max_age_days,
            dry_run,
            apply,
            output,
        } => {
            let report =
                cleanup_pair_demos(max_age_days, dry_run && !apply).map_err(JarvisError::from)?;
            println!(
                "{}",
                render_pair_demo_cleanup_output(&report, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        PairCommand::ReviewStale {
            dry_run,
            archive,
            output,
        } => {
            let report = review_stale_pair_ledgers(dry_run && !archive, archive)
                .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_pair_stale_review_output(&report, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        PairCommand::Finalize {
            id,
            skip_collect,
            skip_close,
            skip_archive,
            output,
        } => {
            let report = finalize_pair_ledger(PairLedgerFinalizeOptions {
                id,
                collect: !skip_collect,
                close: !skip_close,
                archive: !skip_archive,
            })
            .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_pair_finalize_output(&report, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
    }
}

fn evidence_command(command: EvidenceCommand) -> Result<(), JarvisError> {
    match command {
        EvidenceCommand::Bundle {
            pair_id,
            namespace,
            output_dir,
            output,
        } => {
            let report = bundle_evidence(EvidenceBundleOptions {
                pair_id,
                namespace,
                output_dir,
            })
            .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_evidence_bundle_output(&report, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
    }
}

fn health_command(output: ControlPlaneOutput) -> Result<(), JarvisError> {
    let preflight = preflight_nodes().map_err(JarvisError::from)?;
    let sessions = collect_runtime_sessions().map_err(JarvisError::from)?;
    let pairs = list_pair_ledgers(true).map_err(JarvisError::from)?;
    let active_pairs = pairs.iter().filter(|pair| !pair.archived).count();
    let archived_pairs = pairs.iter().filter(|pair| pair.archived).count();
    let stale_pairs = pairs
        .iter()
        .filter(|pair| !pair.archived && pair.stale)
        .count();
    let operator_requests = list_operator_requests().map_err(JarvisError::from)?;
    let proposals = list_proposals().map_err(JarvisError::from)?;
    let capabilities = list_capabilities().map_err(JarvisError::from)?;
    let capability_validation = validate_capabilities().map_err(JarvisError::from)?;
    let worker_runs = list_worker_run_records(Some(50), true).map_err(JarvisError::from)?;
    let failed_worker_runs = worker_runs
        .iter()
        .filter(|run| worker_run_failed(run))
        .count();
    let node_admission = preflight
        .doctors
        .iter()
        .map(|doctor| {
            let status = if doctor.schedulable {
                "admit"
            } else if doctor.available {
                "degraded"
            } else {
                "deny"
            };
            HealthNodeAdmission {
                node: doctor.node.clone(),
                status: status.to_string(),
                schedulable: doctor.schedulable,
                issues: doctor.issues.clone(),
                recommendation: if doctor.schedulable {
                    "eligible for new work".to_string()
                } else if doctor.available {
                    "keep visible but avoid automatic placement until issues clear".to_string()
                } else {
                    "do not place work; node is unreachable".to_string()
                },
            }
        })
        .collect::<Vec<_>>();
    let mut worker_admission_by_key = BTreeMap::<String, HealthWorkerAdmission>::new();
    if let Ok(model_validation) = validate_worker_models(false) {
        for result in model_validation.results {
            let key = format!("{}/{}", result.namespace, result.worker);
            worker_admission_by_key.insert(
                key,
                HealthWorkerAdmission {
                    worker: result.worker,
                    namespace: result.namespace,
                    status: if result.status == "available" {
                        "admit".to_string()
                    } else if result.status == "skipped" {
                        "unknown".to_string()
                    } else {
                        "deny".to_string()
                    },
                    recent_failures: 0,
                    recommendation: match result.status.as_str() {
                        "available" => "eligible for bounded worker placement".to_string(),
                        "skipped" => "keep manual until provider validation runs".to_string(),
                        _ => format!("avoid worker until model check clears: {}", result.detail),
                    },
                },
            );
        }
    }
    for run in &worker_runs {
        let worker = run
            .worker
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or(&run.service_name);
        let namespace = run
            .worker_namespace
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or(&run.namespace);
        let key = format!("{namespace}/{worker}");
        let entry = worker_admission_by_key
            .entry(key)
            .or_insert_with(|| HealthWorkerAdmission {
                worker: worker.to_string(),
                namespace: namespace.to_string(),
                status: "admit".to_string(),
                recent_failures: 0,
                recommendation: "eligible based on recent run history".to_string(),
            });
        if worker_run_failed(run) {
            entry.recent_failures = entry.recent_failures.saturating_add(1);
            entry.status = "degraded".to_string();
            entry.recommendation =
                "prefer another lane until recent failure is reviewed or remediated".to_string();
        }
    }
    let worker_admission = worker_admission_by_key.into_values().collect::<Vec<_>>();
    let autonomy = reconcile_autonomy(
        &list_missions().map_err(JarvisError::from)?,
        &proposals,
        false,
        true,
    )
    .map_err(JarvisError::from)?;
    let mut issues = preflight.issues.clone();
    if stale_pairs > 0 {
        issues.push(format!("stale_pairs={stale_pairs}"));
    }
    let pending_operator_requests = operator_requests
        .iter()
        .filter(|request| request.status == "pending")
        .count();
    if pending_operator_requests > 0 {
        issues.push(format!(
            "pending_operator_requests={pending_operator_requests}"
        ));
    }
    let pending_proposals = proposals
        .iter()
        .filter(|proposal| proposal.status == "pending")
        .count();
    if pending_proposals > 0 {
        issues.push(format!("pending_proposals={pending_proposals}"));
    }
    let capability_failures = capability_validation
        .iter()
        .filter(|report| report.status != "passed")
        .count();
    if capability_failures > 0 {
        issues.push(format!("capability_failures={capability_failures}"));
    }
    if failed_worker_runs > 0 {
        issues.push(format!("recent_failed_worker_runs={failed_worker_runs}"));
    }
    let nodes_total = preflight.doctors.len();
    let nodes_ready = preflight
        .doctors
        .iter()
        .filter(|doctor| doctor.schedulable)
        .count();
    let link_failures = preflight.links.iter().filter(|link| !link.ok).count();
    let report = ProductionHealthReport {
        status: if issues.is_empty() {
            "ready"
        } else {
            "attention"
        }
        .to_string(),
        generated_at_epoch_ms: now_epoch_ms_local(),
        nodes_ready,
        nodes_total,
        link_failures,
        runtime_sessions: sessions.len(),
        active_pairs,
        archived_pairs,
        stale_pairs,
        pending_operator_requests,
        pending_proposals,
        capability_count: capabilities.len(),
        capability_failures,
        failed_worker_runs,
        node_admission,
        worker_admission,
        policy_gates: default_autonomy_policy()
            .into_iter()
            .map(|rule| HealthPolicyGate {
                id: rule.id,
                decision: rule.decision,
                action_class: rule.action_class,
            })
            .collect(),
        autonomy_queue: autonomy
            .blocked_actions
            .into_iter()
            .chain(autonomy.safe_actions)
            .map(|action| HealthQueueItem {
                kind: action.kind,
                status: action.status,
                summary: action.summary,
                command: action.command,
            })
            .collect(),
        issues,
    };
    match output {
        ControlPlaneOutput::Json => println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(anyhow::Error::from)?
        ),
        ControlPlaneOutput::Yaml => {
            println!(
                "{}",
                serde_yaml::to_string(&report).map_err(anyhow::Error::from)?
            )
        }
        ControlPlaneOutput::Table => {
            println!(
                "STATUS\tNODES\tLINK_FAILURES\tSESSIONS\tPAIRS\tARCHIVED\tSTALE\tAPPROVALS\tPROPOSALS\tCAPABILITIES\tWORKER_FAILURES"
            );
            println!(
                "{}\t{}/{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}/{}\t{}",
                report.status,
                report.nodes_ready,
                report.nodes_total,
                report.link_failures,
                report.runtime_sessions,
                report.active_pairs,
                report.archived_pairs,
                report.stale_pairs,
                report.pending_operator_requests,
                report.pending_proposals,
                report
                    .capability_count
                    .saturating_sub(report.capability_failures),
                report.capability_count,
                report.failed_worker_runs
            );
            if !report.issues.is_empty() {
                println!("ISSUES\t{}", report.issues.join(","));
            }
        }
    }
    Ok(())
}

fn production_smoke_command(
    namespace_prefix: Option<String>,
    history: bool,
    limit: usize,
    record: bool,
    skip_worker_models: bool,
    skip_evidence: bool,
    output: ControlPlaneOutput,
) -> Result<(), JarvisError> {
    if history {
        let reports = list_production_smoke_history(limit).map_err(JarvisError::from)?;
        println!(
            "{}",
            render_production_smoke_history_output(&reports, output)?
        );
        return Ok(());
    }

    let mut checks = Vec::new();

    match preflight_nodes() {
        Ok(preflight) => {
            let nodes_ready = preflight
                .doctors
                .iter()
                .filter(|doctor| doctor.schedulable)
                .count();
            checks.push(ProductionSmokeCheck {
                name: "node_preflight".to_string(),
                status: if preflight.ok { "pass" } else { "fail" }.to_string(),
                detail: format!(
                    "nodes={nodes_ready}/{} links_failed={} issues={}",
                    preflight.doctors.len(),
                    preflight.links.iter().filter(|link| !link.ok).count(),
                    preflight.issues.join(";")
                ),
            });

            let missing_capability_nodes = preflight
                .doctors
                .iter()
                .filter(|doctor| {
                    doctor
                        .facts
                        .get("codex_app_server_ws_auth")
                        .map(String::as_str)
                        != Some("supported")
                        || doctor.facts.get("codex_remote_control").map(String::as_str)
                            != Some("supported")
                        || doctor.facts.get("codex_exec_server").map(String::as_str)
                            != Some("supported")
                        || doctor
                            .facts
                            .get("codex_feature_multi_agent")
                            .map(String::as_str)
                            != Some("true")
                })
                .map(|doctor| doctor.node.clone())
                .collect::<Vec<_>>();
            checks.push(ProductionSmokeCheck {
                name: "codex_0133_capabilities".to_string(),
                status: if missing_capability_nodes.is_empty() {
                    "pass"
                } else {
                    "fail"
                }
                .to_string(),
                detail: if missing_capability_nodes.is_empty() {
                    "all nodes report app-server ws auth, remote-control, exec-server, and multi-agent".to_string()
                } else {
                    format!("missing required Codex capabilities on {}", missing_capability_nodes.join(","))
                },
            });
        }
        Err(error) => checks.push(ProductionSmokeCheck {
            name: "node_preflight".to_string(),
            status: "fail".to_string(),
            detail: error.to_string(),
        }),
    }

    match validate_capabilities() {
        Ok(reports) => {
            let failures = reports
                .iter()
                .filter(|report| report.status != "passed")
                .count();
            checks.push(ProductionSmokeCheck {
                name: "capability_validation".to_string(),
                status: if failures == 0 { "pass" } else { "fail" }.to_string(),
                detail: format!("{} checked, {failures} failing", reports.len()),
            });
        }
        Err(error) => checks.push(ProductionSmokeCheck {
            name: "capability_validation".to_string(),
            status: "fail".to_string(),
            detail: error.to_string(),
        }),
    }

    if skip_worker_models {
        checks.push(ProductionSmokeCheck {
            name: "worker_model_validation".to_string(),
            status: "skip".to_string(),
            detail: "skipped by operator".to_string(),
        });
    } else {
        match validate_worker_models(true) {
            Ok(report) => checks.push(ProductionSmokeCheck {
                name: "worker_model_validation".to_string(),
                status: if report.status == "passed" {
                    "pass"
                } else {
                    "fail"
                }
                .to_string(),
                detail: format!(
                    "workers={} checked={} available={} unavailable={} skipped={}",
                    report.workers,
                    report.checked,
                    report.available,
                    report.unavailable,
                    report.skipped
                ),
            }),
            Err(error) => checks.push(ProductionSmokeCheck {
                name: "worker_model_validation".to_string(),
                status: "fail".to_string(),
                detail: error.to_string(),
            }),
        }
    }

    let smoke_id =
        namespace_prefix.unwrap_or_else(|| format!("production-smoke-{}", now_epoch_ms_local()));
    match run_pair_demo_sequence(PairDemoSequenceOptions {
        first_node: None,
        second_node: None,
        namespace_prefix: Some(smoke_id),
        execute: false,
        startup_delay_ms: 250,
        command: Vec::new(),
    }) {
        Ok(report) => checks.push(ProductionSmokeCheck {
            name: "pair_demo_dry_run".to_string(),
            status: if report.preflight_ok { "pass" } else { "fail" }.to_string(),
            detail: format!(
                "id={} nodes={}/{} dry={}",
                report.id, report.nodes_ready, report.nodes_total, report.dry_run.id
            ),
        }),
        Err(error) => checks.push(ProductionSmokeCheck {
            name: "pair_demo_dry_run".to_string(),
            status: "fail".to_string(),
            detail: error.to_string(),
        }),
    }

    match notify_operator_requests(
        &list_operator_requests().map_err(JarvisError::from)?,
        true,
        true,
    ) {
        Ok(report) => checks.push(ProductionSmokeCheck {
            name: "operator_request_dry_run".to_string(),
            status: "pass".to_string(),
            detail: format!(
                "attempted={} delivered={} persistent={} dry_run={}",
                report.attempted, report.delivered, report.persistent, report.dry_run
            ),
        }),
        Err(error) => checks.push(ProductionSmokeCheck {
            name: "operator_request_dry_run".to_string(),
            status: "fail".to_string(),
            detail: error.to_string(),
        }),
    }

    if skip_evidence {
        checks.push(ProductionSmokeCheck {
            name: "evidence_bundle".to_string(),
            status: "skip".to_string(),
            detail: "skipped by operator".to_string(),
        });
    } else {
        let evidence_pair = list_pair_ledgers(true)
            .map_err(JarvisError::from)?
            .into_iter()
            .find(|pair| !pair.members.is_empty());
        match evidence_pair {
            Some(pair) => match bundle_evidence(EvidenceBundleOptions {
                pair_id: Some(pair.id.clone()),
                namespace: None,
                output_dir: None,
            }) {
                Ok(report) => checks.push(ProductionSmokeCheck {
                    name: "evidence_bundle".to_string(),
                    status: "pass".to_string(),
                    detail: format!("pair={} path={}", pair.id, report.path),
                }),
                Err(error) => checks.push(ProductionSmokeCheck {
                    name: "evidence_bundle".to_string(),
                    status: "fail".to_string(),
                    detail: error.to_string(),
                }),
            },
            None => checks.push(ProductionSmokeCheck {
                name: "evidence_bundle".to_string(),
                status: "skip".to_string(),
                detail: "no pair ledger exists yet".to_string(),
            }),
        }
    }

    let status = if checks.iter().any(|check| check.status == "fail") {
        "failed"
    } else if checks.iter().any(|check| check.status == "warn") {
        "attention"
    } else {
        "passed"
    }
    .to_string();
    let generated_at_epoch_ms = now_epoch_ms_local();
    let report = ProductionSmokeReport {
        id: format!("production-smoke-{}", generated_at_epoch_ms),
        status,
        generated_at_epoch_ms,
        checks,
    };
    if record {
        save_production_smoke_report(&report).map_err(JarvisError::from)?;
    }
    println!("{}", render_production_smoke_output(&report, output)?);
    Ok(())
}

fn production_smoke_history_dir() -> anyhow::Result<PathBuf> {
    if let Some(path) = env::var_os("JARVIS_CODEX_DIR") {
        return Ok(PathBuf::from(path).join("production-smoke"));
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join(".jarvis")
        .join("codex")
        .join("production-smoke"))
}

fn save_production_smoke_report(report: &ProductionSmokeReport) -> anyhow::Result<()> {
    let dir = production_smoke_history_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", slugify(&report.id)));
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(report)?)
        .with_context(|| format!("failed to write '{}'", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| {
        format!(
            "failed to replace production smoke report '{}'",
            path.display()
        )
    })
}

fn list_production_smoke_history(limit: usize) -> anyhow::Result<Vec<ProductionSmokeReport>> {
    let dir = production_smoke_history_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut reports = Vec::new();
    for entry in
        fs::read_dir(&dir).with_context(|| format!("failed to read '{}'", dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(report) = serde_json::from_str::<ProductionSmokeReport>(&raw) else {
            continue;
        };
        reports.push(report);
    }
    reports.sort_by(|left, right| right.generated_at_epoch_ms.cmp(&left.generated_at_epoch_ms));
    reports.truncate(limit);
    Ok(reports)
}

fn render_production_smoke_history_output(
    reports: &[ProductionSmokeReport],
    output: ControlPlaneOutput,
) -> Result<String, JarvisError> {
    match output {
        ControlPlaneOutput::Json => serde_json::to_string_pretty(reports)
            .map_err(anyhow::Error::from)
            .map_err(JarvisError::from),
        ControlPlaneOutput::Yaml => serde_yaml::to_string(reports)
            .map_err(anyhow::Error::from)
            .map_err(JarvisError::from),
        ControlPlaneOutput::Table => {
            let mut lines = vec!["ID\tSTATUS\tCHECKS\tFAILED".to_string()];
            lines.extend(reports.iter().map(|report| {
                let failed = report
                    .checks
                    .iter()
                    .filter(|check| check.status == "fail")
                    .count();
                format!(
                    "{}\t{}\t{}\t{}",
                    report.id,
                    report.status,
                    report.checks.len(),
                    failed
                )
            }));
            Ok(lines.join("\n"))
        }
    }
}

fn render_production_smoke_output(
    report: &ProductionSmokeReport,
    output: ControlPlaneOutput,
) -> Result<String, JarvisError> {
    match output {
        ControlPlaneOutput::Json => serde_json::to_string_pretty(report)
            .map_err(anyhow::Error::from)
            .map_err(JarvisError::from),
        ControlPlaneOutput::Yaml => serde_yaml::to_string(report)
            .map_err(anyhow::Error::from)
            .map_err(JarvisError::from),
        ControlPlaneOutput::Table => {
            let mut lines = vec![
                format!("STATUS\t{}", report.status),
                "CHECK\tSTATUS\tDETAIL".to_string(),
            ];
            lines.extend(
                report
                    .checks
                    .iter()
                    .map(|check| format!("{}\t{}\t{}", check.name, check.status, check.detail)),
            );
            Ok(lines.join("\n"))
        }
    }
}

fn worker_run_failed(run: &control_plane::WorkerRunRecord) -> bool {
    let remediation_status = run
        .remediation_status
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        remediation_status.as_str(),
        "reviewed" | "remediated" | "acknowledged"
    ) {
        return false;
    }
    run.phase.eq_ignore_ascii_case("failed")
        || run
            .error
            .as_deref()
            .map(|value| !value.is_empty())
            .unwrap_or(false)
}

fn operator_request_command(command: OperatorRequestCommand) -> Result<(), JarvisError> {
    match command {
        OperatorRequestCommand::Create {
            title,
            kind,
            severity,
            reason,
            risk,
            requested_by,
            namespace,
            request_id,
            method,
            command,
            params_json,
            ttl_seconds,
            output,
        } => {
            let params = parse_optional_json(params_json.as_deref(), "--params-json")?;
            let record = create_operator_request(OperatorRequestCreateOptions {
                title,
                kind,
                severity,
                reason,
                risk,
                requested_by,
                namespace,
                request_id,
                method,
                command,
                params,
                ttl_seconds: Some(ttl_seconds),
            })
            .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_operator_request_output(&record, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        OperatorRequestCommand::Sudo {
            title,
            reason,
            command,
            risk,
            requested_by,
            namespace,
            ttl_seconds,
            output,
        } => {
            let record = create_operator_request(OperatorRequestCreateOptions {
                title,
                kind: "sudo".to_string(),
                severity: "high".to_string(),
                reason,
                risk: risk.or_else(|| {
                    Some(
                        "This requires administrator privileges and may mutate the host."
                            .to_string(),
                    )
                }),
                requested_by: Some(requested_by),
                namespace,
                request_id: None,
                method: Some("sudo".to_string()),
                command: Some(command),
                params: None,
                ttl_seconds: Some(ttl_seconds),
            })
            .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_operator_request_output(&record, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        OperatorRequestCommand::List {
            status,
            all,
            cluster,
            output,
        } => {
            let mut records = list_operator_requests().map_err(JarvisError::from)?;
            if cluster {
                records.extend(list_cluster_operator_requests().map_err(JarvisError::from)?);
            }
            if !all {
                let wanted = status.unwrap_or_else(|| "pending".to_string());
                records.retain(|record| record.status == wanted);
            } else if let Some(wanted) = status {
                records.retain(|record| record.status == wanted);
            }
            println!(
                "{}",
                render_operator_requests_output(&records, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        OperatorRequestCommand::Show {
            id,
            node,
            cluster,
            output,
        } => {
            let record = if node.is_some() || cluster {
                show_cluster_operator_request(&id)
                    .map_err(JarvisError::from)?
                    .ok_or_else(|| {
                        JarvisError::Other(anyhow::anyhow!(
                            "operator request '{}' does not exist on remote nodes",
                            id
                        ))
                    })?
            } else {
                match show_operator_request(&id) {
                    Ok(record) => record,
                    Err(error) => {
                        if let Some(record) =
                            show_cluster_operator_request(&id).map_err(JarvisError::from)?
                        {
                            record
                        } else {
                            return Err(JarvisError::from(error));
                        }
                    }
                }
            };
            println!(
                "{}",
                render_operator_request_output(&record, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        OperatorRequestCommand::Notify {
            status,
            persistent,
            dry_run,
            cluster,
            output,
        } => {
            let wanted = status.unwrap_or_else(|| "pending".to_string());
            let mut records = list_operator_requests().map_err(JarvisError::from)?;
            if cluster {
                records.extend(list_cluster_operator_requests().map_err(JarvisError::from)?);
            }
            records.retain(|record| record.status == wanted);
            let report = notify_operator_requests(&records, persistent, dry_run)
                .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_operator_request_notify_output(&report, output)
                    .map_err(JarvisError::from)?
            );
            Ok(())
        }
        OperatorRequestCommand::Resolve {
            id,
            status,
            response_json,
            error,
            decided_by,
            decision,
            node,
            mission,
            output,
        } => {
            let parsed_response = parse_optional_json(response_json.as_deref(), "--response-json")?;
            let existing = match show_operator_request(&id) {
                Ok(record) if node.is_none() => record,
                local_result => {
                    if node.is_some() || local_result.is_err() {
                        let remote =
                            resolve_cluster_operator_request(RemoteOperatorRequestResolveOptions {
                                node: node.clone(),
                                id: id.clone(),
                                status: status.clone(),
                                response_json: response_json.clone(),
                                error: error.clone(),
                                decided_by: decided_by.clone(),
                                decision: decision.clone(),
                            })
                            .map_err(JarvisError::from)?;
                        if let Some(record) = remote {
                            println!(
                                "{}",
                                render_operator_request_output(&record, output)
                                    .map_err(JarvisError::from)?
                            );
                            return Ok(());
                        }
                    }
                    local_result.map_err(JarvisError::from)?
                }
            };
            let response = default_operator_request_response(&existing, &status, parsed_response);
            if let (Some(namespace), Some(request_id)) = (
                existing.namespace.as_deref(),
                existing.request_id.as_deref(),
            ) {
                if status == "approved" || status == "resolved" || status == "denied" {
                    let response_for_request = if status == "denied" {
                        None
                    } else {
                        response.clone().or(Some(serde_json::Value::Null))
                    };
                    let error_for_request = if status == "denied" {
                        error
                            .as_deref()
                            .or(decision.as_deref())
                            .or(Some("Denied by operator"))
                    } else {
                        error.as_deref()
                    };
                    if let Err(local_error) = respond_runtime_server_request(
                        namespace,
                        request_id,
                        response_for_request.clone(),
                        error_for_request.map(ToOwned::to_owned),
                    ) {
                        if !respond_cluster_runtime_server_request(
                            namespace,
                            request_id,
                            response_for_request.as_ref(),
                            error_for_request,
                        )
                        .map_err(JarvisError::from)?
                            && !is_missing_runtime_response_error(&local_error)
                        {
                            return Err(JarvisError::from(local_error));
                        }
                    }
                    append_cli_mission_event(
                        mission.as_deref(),
                        "authorize",
                        if status == "denied" {
                            "denied"
                        } else {
                            "approved"
                        },
                        format!(
                            "Resolved operator request '{}' for namespace '{}'.",
                            id, namespace
                        ),
                        None,
                        Some(namespace.to_string()),
                        None,
                        None,
                        Some(id.clone()),
                        Vec::new(),
                    )?;
                }
            }
            let record = resolve_operator_request(
                &id,
                OperatorRequestResolveOptions {
                    status,
                    response,
                    error,
                    decided_by,
                    decision,
                },
            )
            .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_operator_request_output(&record, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
    }
}

fn message_command(command: MessageCommand) -> Result<(), JarvisError> {
    match command {
        MessageCommand::Send {
            to_namespace,
            from_namespace,
            to_node,
            agent,
            mode,
            kind,
            text,
            file,
            output,
        } => {
            let body = match (text, file) {
                (Some(text), None) => text,
                (None, Some(file)) => fs::read_to_string(&file).map_err(JarvisError::from)?,
                (Some(_), Some(_)) => {
                    return Err(JarvisError::Other(anyhow::anyhow!(
                        "--text and --file cannot be used together"
                    )));
                }
                (None, None) => {
                    return Err(JarvisError::Other(anyhow::anyhow!(
                        "provide either --text or --file"
                    )));
                }
            };
            let record = send_relay_message(RelayMessageSendOptions {
                from_namespace,
                to_namespace,
                to_node,
                agent,
                mode,
                kind,
                body,
            })
            .map_err(JarvisError::from)?;
            println!(
                "{}",
                render_relay_message_output(&record, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        MessageCommand::List {
            namespace,
            status,
            all,
            cluster,
            output,
        } => {
            let wanted = if all {
                status.as_deref()
            } else {
                Some(status.as_deref().unwrap_or("pending"))
            };
            let mut records =
                list_relay_messages(namespace.as_deref(), wanted).map_err(JarvisError::from)?;
            if cluster {
                records.extend(
                    list_cluster_relay_messages(namespace.as_deref(), wanted)
                        .map_err(JarvisError::from)?,
                );
                records.sort_by(|left, right| {
                    right.updated_at_epoch_ms.cmp(&left.updated_at_epoch_ms)
                });
            }
            println!(
                "{}",
                render_relay_messages_output(&records, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        MessageCommand::Flush {
            namespace,
            status,
            cluster,
            output,
        } => {
            let mut records = flush_relay_messages(namespace.as_deref(), status.as_deref())
                .map_err(JarvisError::from)?;
            if cluster {
                records.extend(
                    flush_cluster_relay_messages(namespace.as_deref(), status.as_deref())
                        .map_err(JarvisError::from)?,
                );
                records.sort_by(|left, right| {
                    right.updated_at_epoch_ms.cmp(&left.updated_at_epoch_ms)
                });
            }
            println!(
                "{}",
                render_relay_messages_output(&records, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        MessageCommand::Ack { id, node, output } => {
            let record = if node.is_some() {
                ack_cluster_relay_message(node.as_deref(), &id)
                    .map_err(JarvisError::from)?
                    .ok_or_else(|| {
                        JarvisError::Other(anyhow::anyhow!(
                            "relay message '{}' does not exist on remote nodes",
                            id
                        ))
                    })?
            } else {
                ack_relay_message(&id).map_err(JarvisError::from)?
            };
            println!(
                "{}",
                render_relay_message_output(&record, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        MessageCommand::Retry { id, node, output } => {
            let record = if node.is_some() {
                retry_cluster_relay_message(node.as_deref(), &id)
                    .map_err(JarvisError::from)?
                    .ok_or_else(|| {
                        JarvisError::Other(anyhow::anyhow!(
                            "relay message '{}' does not exist on remote nodes",
                            id
                        ))
                    })?
            } else {
                retry_relay_message(&id).map_err(JarvisError::from)?
            };
            println!(
                "{}",
                render_relay_message_output(&record, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        MessageCommand::Supersede {
            id,
            reason,
            node,
            output,
        } => {
            let record = if node.is_some() {
                supersede_cluster_relay_message(node.as_deref(), &id, &reason)
                    .map_err(JarvisError::from)?
                    .ok_or_else(|| {
                        JarvisError::Other(anyhow::anyhow!(
                            "relay message '{}' does not exist on remote nodes",
                            id
                        ))
                    })?
            } else {
                supersede_relay_message(&id, &reason).map_err(JarvisError::from)?
            };
            println!(
                "{}",
                render_relay_message_output(&record, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
        MessageCommand::Prune {
            max_age_days,
            dry_run,
            apply,
            cluster,
            output,
        } => {
            let effective_dry_run = dry_run && !apply;
            let mut reports = vec![
                prune_relay_messages(max_age_days, effective_dry_run).map_err(JarvisError::from)?,
            ];
            if cluster {
                reports.extend(
                    prune_cluster_relay_messages(max_age_days, effective_dry_run)
                        .map_err(JarvisError::from)?,
                );
            }
            println!(
                "{}",
                render_relay_prune_output(&reports, output).map_err(JarvisError::from)?
            );
            Ok(())
        }
    }
}

fn parse_optional_json(
    raw: Option<&str>,
    flag: &str,
) -> Result<Option<serde_json::Value>, JarvisError> {
    raw.map(|value| {
        serde_json::from_str::<serde_json::Value>(value).map_err(|parse_error| {
            JarvisError::Other(anyhow::anyhow!("{flag} must be valid JSON: {parse_error}"))
        })
    })
    .transpose()
}

fn is_missing_runtime_response_error(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.contains("runtime session") && message.contains("does not exist")
}

fn default_operator_request_response(
    record: &operator_request::OperatorRequestRecord,
    status: &str,
    explicit: Option<serde_json::Value>,
) -> Option<serde_json::Value> {
    if explicit.is_some() || status == "denied" {
        return explicit;
    }
    let decisions = record
        .params
        .as_ref()
        .and_then(|params| params.get("availableDecisions"))
        .and_then(|decisions| decisions.as_array())?;
    decisions
        .iter()
        .find(|decision| decision.as_str() == Some("accept"))
        .cloned()
        .or_else(|| {
            decisions
                .iter()
                .find(|decision| {
                    decision
                        .as_object()
                        .map(|object| object.contains_key("accept"))
                        .unwrap_or(false)
                })
                .cloned()
        })
        .or_else(|| {
            decisions
                .iter()
                .find(|decision| decision.as_str() != Some("cancel"))
                .cloned()
        })
}

#[allow(clippy::too_many_arguments)]
fn append_cli_mission_event(
    mission_id: Option<&str>,
    stage: &str,
    status: &str,
    summary: String,
    ticket: Option<PathBuf>,
    namespace: Option<String>,
    node: Option<String>,
    visit: Option<String>,
    approval: Option<String>,
    evidence: Vec<String>,
) -> Result<(), JarvisError> {
    let Some(mission_id) = mission_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    append_mission_event(MissionEventOptions {
        mission_id: mission_id.to_string(),
        stage: stage.to_string(),
        status: status.to_string(),
        summary,
        ticket,
        namespace,
        node,
        visit,
        approval,
        evidence,
    })
    .map(|_| ())
    .map_err(JarvisError::from)
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

fn print_experimental_surface_notice(surface: &str) {
    eprintln!(
        "warning: the {surface} surface is experimental and currently receives smoke-test maintenance while jarvisctl core operator paths are prioritized"
    );
}

fn kube_command(command: KubeCommand) -> Result<(), JarvisError> {
    print_experimental_surface_notice("kubernetes");
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

#[instrument(err)]
fn list_sessions(
    backend: SessionBackend,
    namespace: Option<String>,
    json: bool,
) -> Result<(), JarvisError> {
    let _ = backend;
    let mut sessions = collect_runtime_sessions().map_err(JarvisError::from)?;

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
fn history(
    backend: SessionBackend,
    namespace: &str,
    include_turns: bool,
    json: bool,
) -> Result<(), JarvisError> {
    let _ = backend;
    let metadata = runtime::session_metadata_for_namespace(namespace).map_err(JarvisError::from)?;
    if metadata.backend != "codex-app" {
        return Err(JarvisError::Other(anyhow::anyhow!(
            "history is only available for codex-app sessions; '{}' uses '{}'",
            namespace,
            metadata.backend
        )));
    }

    let response = read_codex_app_thread(namespace, include_turns).map_err(JarvisError::from)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&response).map_err(anyhow::Error::from)?
        );
        return Ok(());
    }

    print_thread_history(namespace, &response);
    Ok(())
}

fn print_thread_history(namespace: &str, value: &serde_json::Value) {
    let thread = value.get("thread").unwrap_or(value);
    let thread_id = thread
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("-");
    let status = thread
        .get("status")
        .and_then(status_label)
        .unwrap_or_else(|| "-".to_string());
    println!("{namespace} {thread_id} {status}");

    let turns = thread
        .get("turns")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    if turns.is_empty() {
        println!("(no turns loaded)");
        return;
    }

    for turn in turns.iter().rev().take(12).rev() {
        let id = turn
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(short_id)
            .unwrap_or_else(|| "-".to_string());
        let status = turn
            .get("status")
            .and_then(status_label)
            .unwrap_or_else(|| "-".to_string());
        let items_view = turn
            .get("itemsView")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("-");
        let items = turn
            .get("items")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len)
            .unwrap_or_default();
        let preview = turn_preview(turn).unwrap_or_default();
        println!("{id:10} {status:12} {items_view:10} {items:3} {preview}");
    }
}

fn status_label(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(raw) => Some(raw.clone()),
        serde_json::Value::Object(object) => object
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        _ => None,
    }
}

fn turn_preview(turn: &serde_json::Value) -> Option<String> {
    let items = turn.get("items")?.as_array()?;
    items.iter().rev().find_map(|item| {
        item.get("text")
            .and_then(serde_json::Value::as_str)
            .or_else(|| item.get("message").and_then(serde_json::Value::as_str))
            .or_else(|| item.get("command").and_then(serde_json::Value::as_str))
            .map(|text| truncate_plain(text, 96))
    })
}

fn short_id(raw: &str) -> String {
    raw.split('-')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or(raw)
        .to_string()
}

fn truncate_plain(raw: &str, limit: usize) -> String {
    let normalized = raw.replace('\n', " ").trim().to_string();
    if normalized.chars().count() <= limit {
        return normalized;
    }
    let mut rendered = normalized
        .chars()
        .take(limit.saturating_sub(3))
        .collect::<String>();
    rendered.push_str("...");
    rendered
}

#[instrument(err)]
fn exec_agent(backend: SessionBackend, namespace: &str, agent: &str) -> Result<(), JarvisError> {
    let _ = backend;
    attach_runtime_session(namespace, agent).map_err(JarvisError::from)
}

#[derive(Debug)]
struct RuntimeTellTarget {
    node: Option<String>,
    namespace: Option<String>,
    agent: Option<String>,
}

fn parse_runtime_tell_target(value: &str) -> anyhow::Result<RuntimeTellTarget> {
    let parts = value
        .split('/')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    match parts.as_slice() {
        [node, namespace] => Ok(RuntimeTellTarget {
            node: Some((*node).to_string()),
            namespace: Some((*namespace).to_string()),
            agent: None,
        }),
        [node, namespace, agent] => Ok(RuntimeTellTarget {
            node: Some((*node).to_string()),
            namespace: Some((*namespace).to_string()),
            agent: Some((*agent).to_string()),
        }),
        _ => Err(anyhow::anyhow!(
            "--target must use node/namespace or node/namespace/agent"
        )),
    }
}

#[instrument(err)]
fn tell(
    backend: SessionBackend,
    node: Option<&str>,
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
    let mode_arg = codex_input_mode_arg(mode);
    if let Some(node) = node {
        if tell_runtime_session_on_node(node, namespace, agent, &contents, mode_arg)
            .map_err(JarvisError::from)?
        {
            if let Some(file) = file {
                println!(
                    "Sent '{}' to '{}':'{}' on node '{}'",
                    file, namespace, agent, node
                );
            } else {
                println!(
                    "Sent text to '{}':'{}' on node '{}'",
                    namespace, agent, node
                );
            }
            return Ok(());
        }
    }

    if let Err(local_error) = tell_runtime_session(namespace, agent, &contents, press_enter, mode) {
        if !press_enter {
            return Err(JarvisError::from(local_error));
        }
        if !tell_cluster_runtime_session(namespace, agent, &contents, mode_arg)
            .map_err(JarvisError::from)?
        {
            return Err(JarvisError::from(local_error));
        }
    }

    if let Some(file) = file {
        println!("✅ Sent '{}' to '{}':'{}'", file, namespace, agent);
    } else {
        println!("✅ Sent text to '{}':'{}'", namespace, agent);
    }
    Ok(())
}

#[instrument(err)]
fn respond_server_request_command(
    backend: SessionBackend,
    namespace: &str,
    request_id: &str,
    response_json: Option<&str>,
    error: Option<&str>,
    mission: Option<&str>,
) -> Result<(), JarvisError> {
    let _ = backend;
    let response = match response_json {
        Some(raw) => Some(serde_json::from_str::<serde_json::Value>(raw).map_err(
            |parse_error| {
                JarvisError::Other(anyhow::anyhow!(
                    "--response-json must be valid JSON: {}",
                    parse_error
                ))
            },
        )?),
        None => None,
    };
    let error = error
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    if response.is_none() && error.is_none() {
        return Err(JarvisError::Other(anyhow::anyhow!(
            "provide either --response-json or --error"
        )));
    }
    if let Err(local_error) =
        respond_runtime_server_request(namespace, request_id, response.clone(), error.clone())
    {
        if !respond_cluster_runtime_server_request(
            namespace,
            request_id,
            response.as_ref(),
            error.as_deref(),
        )
        .map_err(JarvisError::from)?
        {
            return Err(JarvisError::from(local_error));
        }
    }
    println!(
        "✅ Responded to app-server request '{}' in '{}'",
        request_id, namespace
    );
    append_cli_mission_event(
        mission,
        "authorize",
        if error.is_some() {
            "denied"
        } else {
            "approved"
        },
        format!(
            "Responded to app-server request '{}' in namespace '{}'.",
            request_id, namespace
        ),
        None,
        Some(namespace.to_string()),
        None,
        None,
        Some(request_id.to_string()),
        Vec::new(),
    )?;
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
    if let Err(local_error) = attach_runtime_session(namespace, "agent0") {
        if !attach_cluster_runtime_session(namespace, "agent0").map_err(JarvisError::from)? {
            return Err(JarvisError::from(local_error));
        }
    }
    Ok(())
}

#[instrument(err)]
fn delete_session(
    backend: SessionBackend,
    namespace: &str,
    mission: Option<&str>,
) -> Result<(), JarvisError> {
    let _ = backend;
    if let Err(local_error) = delete_runtime_session(namespace) {
        if !delete_cluster_runtime_session(namespace).map_err(JarvisError::from)? {
            return Err(JarvisError::from(local_error));
        }
    }
    append_cli_mission_event(
        mission,
        "verify",
        "closed",
        format!("Closed runtime namespace '{}'.", namespace),
        None,
        Some(namespace.to_string()),
        None,
        None,
        None,
        Vec::new(),
    )?;
    Ok(())
}

#[instrument(err)]
fn interrupt_agent(
    backend: SessionBackend,
    namespace: &str,
    agent: &str,
) -> Result<(), JarvisError> {
    let _ = backend;
    if let Err(local_error) = interrupt_runtime_session(namespace, agent) {
        if !interrupt_cluster_runtime_session(namespace, agent).map_err(JarvisError::from)? {
            return Err(JarvisError::from(local_error));
        }
    }
    Ok(())
}

fn codex_input_mode_arg(mode: CodexAppInputMode) -> &'static str {
    match mode {
        CodexAppInputMode::Auto => "auto",
        CodexAppInputMode::Steer => "steer",
        CodexAppInputMode::Queue => "queue",
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

fn now_epoch_ms_local() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{parse_runtime_tell_target, select_kube_runtime_control_port};

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

    #[test]
    fn parses_runtime_tell_target_address() {
        let target = parse_runtime_tell_target("archiebald/demo-ns/agent1").unwrap();
        assert_eq!(target.node.as_deref(), Some("archiebald"));
        assert_eq!(target.namespace.as_deref(), Some("demo-ns"));
        assert_eq!(target.agent.as_deref(), Some("agent1"));

        let target = parse_runtime_tell_target("archiebald/demo-ns").unwrap();
        assert_eq!(target.node.as_deref(), Some("archiebald"));
        assert_eq!(target.namespace.as_deref(), Some("demo-ns"));
        assert_eq!(target.agent, None);

        assert!(parse_runtime_tell_target("demo-ns").is_err());
    }
}
