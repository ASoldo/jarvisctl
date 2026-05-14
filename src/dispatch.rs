use crate::SessionBackend;
use crate::board::{BoardFile, discover_default_boards, normalize_column, resolve_wiki_link};
use crate::codex::{
    CodexLaunchOptions, CodexRuntimeDriver, default_namespace_for_ticket,
    discover_codex_session_id, discover_latest_launch_session_id, launch_codex_ticket,
};
use crate::native::RuntimeContextMetadata;
use crate::runtime::{
    RuntimeSessionState, cancel_runtime_session, delete_runtime_session_if_exists,
    probe_runtime_session_state,
};
use crate::ticket::TicketNote;
use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::{info, warn};

const READY_FOR_CODEX: &str = "ready for codex";
const CODEX_WORKING: &str = "codex working";

#[derive(Debug, Clone)]
pub struct DispatchOptions {
    pub backend: SessionBackend,
    pub driver: CodexRuntimeDriver,
    pub vault_path: PathBuf,
    pub boards: Vec<PathBuf>,
    pub interval_seconds: u64,
    pub once: bool,
    pub dry_run: bool,
    pub state_file: Option<PathBuf>,
    pub agent: String,
    pub agents: usize,
    pub startup_delay_ms: u64,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchState {
    pub version: u32,
    pub boards: BTreeMap<String, BoardSnapshotState>,
    pub active_runs: BTreeMap<String, ActiveRunState>,
    #[serde(default)]
    pub tickets: BTreeMap<String, TicketRuntimeState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardSnapshotState {
    pub updated_at_epoch_ms: u128,
    #[serde(default)]
    pub file_modified_epoch_ms: Option<u128>,
    #[serde(default)]
    pub file_len_bytes: Option<u64>,
    pub card_columns: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy)]
struct BoardFileSignature {
    modified_epoch_ms: u128,
    len_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveRunState {
    pub ticket_note: String,
    pub ticket_link: String,
    pub board_path: String,
    pub namespace: String,
    pub agent: String,
    pub record_file: String,
    pub launched_at_epoch_ms: u128,
    #[serde(default)]
    pub codex_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TicketRuntimeState {
    #[serde(default)]
    pub last_namespace: Option<String>,
    #[serde(default)]
    pub last_record_file: Option<String>,
    #[serde(default)]
    pub last_codex_session_id: Option<String>,
    #[serde(default)]
    pub last_outcome: Option<String>,
    #[serde(default)]
    pub last_transition_epoch_ms: Option<u128>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct HookStateFile {
    #[serde(default)]
    sessions: BTreeMap<String, HookSessionState>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct HookSessionState {
    #[serde(default)]
    last_event: Option<String>,
    #[serde(default)]
    last_event_at: Option<String>,
}

impl Default for DispatchState {
    fn default() -> Self {
        Self {
            version: 2,
            boards: BTreeMap::new(),
            active_runs: BTreeMap::new(),
            tickets: BTreeMap::new(),
        }
    }
}

pub fn run_dispatch_loop(options: DispatchOptions) -> anyhow::Result<()> {
    loop {
        dispatch_once(&options)?;
        if options.once {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(options.interval_seconds.max(1)));
    }
}

fn dispatch_once(options: &DispatchOptions) -> anyhow::Result<()> {
    let state_path = options
        .state_file
        .clone()
        .unwrap_or(default_state_file(&options.vault_path)?);
    let mut state = load_state(&state_path)?;
    let board_paths = resolve_board_paths(options)?;
    let active_board_paths = state
        .active_runs
        .values()
        .map(|run| PathBuf::from(&run.board_path))
        .collect::<BTreeSet<_>>();
    let mut boards_to_load = Vec::new();
    for board_path in &board_paths {
        let snapshot = state.boards.get(&board_path.display().to_string());
        if should_load_board(
            board_path,
            snapshot,
            active_board_paths.contains(board_path),
        )? {
            boards_to_load.push(board_path.clone());
        }
    }
    info!(
        "Dispatch scan starting for {} board(s); loading {} changed/active board(s) using state '{}'",
        board_paths.len(),
        boards_to_load.len(),
        state_path.display()
    );
    let mut boards = load_boards(&boards_to_load)?;
    let mut dirty_boards = BTreeSet::new();

    let canceled_links = process_manual_cancellations(&mut boards, &mut state, options)?;
    process_completed_runs(&mut boards, &mut state, options, &mut dirty_boards)?;

    for board_path in &board_paths {
        info!("Scanning board '{}'", board_path.display());
        let key = board_path.display().to_string();
        let previous_positions = state
            .boards
            .get(&key)
            .map(|snapshot| snapshot.card_columns.clone())
            .unwrap_or_default();

        let Some(board) = boards.get_mut(board_path) else {
            continue;
        };
        let current_positions = board
            .card_positions()
            .into_iter()
            .collect::<BTreeMap<String, String>>();

        for (ticket_link, current_column) in &current_positions {
            if canceled_links.contains(ticket_link) {
                continue;
            }

            let previous_column = previous_positions.get(ticket_link);
            if normalize_column(current_column) == READY_FOR_CODEX
                && previous_column.map(|value| normalize_column(value))
                    != Some(READY_FOR_CODEX.to_string())
            {
                handle_ready_transition(
                    board,
                    ticket_link,
                    options,
                    &mut state,
                    &mut dirty_boards,
                )?;
            }
        }
    }

    if !options.dry_run {
        for board_path in &dirty_boards {
            let board = boards
                .get(board_path)
                .ok_or_else(|| anyhow!("board '{}' was not loaded", board_path.display()))?;
            board.save()?;
        }
        for board_path in boards.keys() {
            let board = boards
                .get(board_path)
                .ok_or_else(|| anyhow!("board '{}' was not loaded", board_path.display()))?;
            refresh_board_snapshot(&mut state, board_path, board)?;
        }
        save_state(&state_path, &state)?;
    }

    info!("Dispatch scan finished");

    Ok(())
}

fn handle_ready_transition(
    board: &mut BoardFile,
    ticket_link: &str,
    options: &DispatchOptions,
    state: &mut DispatchState,
    dirty_boards: &mut BTreeSet<PathBuf>,
) -> anyhow::Result<()> {
    let ticket_path = resolve_wiki_link(&options.vault_path, ticket_link);
    let ticket_key = ticket_path.display().to_string();

    if state.active_runs.contains_key(&ticket_key) {
        info!("Skipping '{}': already has an active run", ticket_key);
        return Ok(());
    }

    let ticket = TicketNote::load(&ticket_path)?;
    ticket.validate_codex_minimum()?;

    if !matches!(
        ticket.frontmatter.owner.as_deref(),
        Some(owner) if owner.eq_ignore_ascii_case("codex")
    ) {
        warn!("Skipping '{}': owner is not codex", ticket_path.display());
        return Ok(());
    }

    if ticket.frontmatter.autostart != Some(true) {
        warn!(
            "Skipping '{}': autostart is not true",
            ticket_path.display()
        );
        return Ok(());
    }

    let expected_namespace = default_namespace_for_ticket(&ticket);
    let last_codex_session_id = state
        .tickets
        .get(&ticket_key)
        .and_then(|runtime| runtime.last_codex_session_id.clone());

    match probe_runtime_session_state(&expected_namespace)? {
        RuntimeSessionState::ActiveWork => {
            info!(
                "Adopting existing active Codex session '{}' for '{}'",
                expected_namespace,
                ticket_path.display()
            );
            if options.dry_run {
                return Ok(());
            }

            let codex_session_id =
                resolve_ticket_session_id(&ticket, now_epoch_ms()?, last_codex_session_id.clone())?;
            board.move_card(ticket_link, CODEX_WORKING)?;
            update_ticket_status(&ticket_path, "active")?;
            remember_ticket_runtime(
                state,
                &ticket_key,
                Some(expected_namespace.clone()),
                None,
                codex_session_id.clone(),
                "active",
            )?;
            state.active_runs.insert(
                ticket_key.clone(),
                ActiveRunState {
                    ticket_note: ticket_key,
                    ticket_link: ticket_link.to_string(),
                    board_path: board.path.display().to_string(),
                    namespace: expected_namespace,
                    agent: options.agent.clone(),
                    record_file: String::new(),
                    launched_at_epoch_ms: now_epoch_ms()?,
                    codex_session_id,
                },
            );
            return Ok(());
        }
        RuntimeSessionState::Idle => {
            info!(
                "Removing stale namespace '{}' before relaunching '{}'",
                expected_namespace,
                ticket_path.display()
            );
            if !options.dry_run {
                delete_runtime_session_if_exists(&expected_namespace)?;
            }
        }
        RuntimeSessionState::Missing => {}
    }

    if last_codex_session_id.is_some() {
        info!(
            "Resuming prior Codex conversation for '{}'",
            ticket_path.display()
        );
    } else {
        info!("Dispatching '{}'", ticket_path.display());
    }

    if options.dry_run {
        return Ok(());
    }

    let launch = launch_codex_ticket(CodexLaunchOptions {
        backend: options.backend,
        driver: options.driver,
        task_note: ticket_path.clone(),
        namespace: None,
        agents: options.agents,
        agent: options.agent.clone(),
        fresh_session: false,
        resume_session_id: last_codex_session_id.clone(),
        working_directory: None,
        prompt_file: None,
        operator_message: None,
        images: Vec::new(),
        environment: Default::default(),
        context_overlay: RuntimeContextMetadata::default(),
        extra_runtime_args: Vec::new(),
        startup_delay_ms: options.startup_delay_ms,
        command: options.command.clone(),
    })?;

    board.move_card(ticket_link, CODEX_WORKING)?;
    dirty_boards.insert(board.path.clone());
    update_ticket_status(&ticket_path, "active")?;
    remember_ticket_runtime(
        state,
        &ticket_key,
        Some(launch.namespace.clone()),
        Some(launch.record_file.clone()),
        launch.codex_session_id.clone(),
        "active",
    )?;

    state.active_runs.insert(
        ticket_key.clone(),
        ActiveRunState {
            ticket_note: ticket_key,
            ticket_link: ticket_link.to_string(),
            board_path: board.path.display().to_string(),
            namespace: launch.namespace,
            agent: launch.agent,
            record_file: launch.record_file,
            launched_at_epoch_ms: launch.launched_at_epoch_ms,
            codex_session_id: launch.codex_session_id,
        },
    );

    Ok(())
}

fn process_manual_cancellations(
    boards: &mut BTreeMap<PathBuf, BoardFile>,
    state: &mut DispatchState,
    options: &DispatchOptions,
) -> anyhow::Result<BTreeSet<String>> {
    let mut canceled_links = BTreeSet::new();
    let active_runs = state.active_runs.clone();

    for (ticket_note, run) in active_runs {
        let board_path = PathBuf::from(&run.board_path);
        let current_column = boards
            .get(&board_path)
            .and_then(|board| column_for_ticket(board, &run.ticket_link));

        if matches!(
            current_column.as_deref(),
            Some(column) if normalize_column(column) == CODEX_WORKING
        ) {
            continue;
        }

        let destination = current_column.unwrap_or_else(|| "Removed".to_string());
        info!(
            "Canceling active Codex run '{}' because board moved to '{}'",
            run.namespace, destination
        );
        canceled_links.insert(run.ticket_link.clone());

        if options.dry_run {
            continue;
        }

        let ticket_path = PathBuf::from(&ticket_note);
        append_ticket_progress(
            &ticket_path,
            &format!(
                "Run canceled after the board moved to '{}'. Namespace '{}' was closed.",
                destination, run.namespace
            ),
        )?;
        update_ticket_status(&ticket_path, "canceled")?;
        cancel_runtime_session(&run.namespace, &run.agent)?;
        remember_ticket_runtime(
            state,
            &ticket_note,
            Some(run.namespace.clone()),
            record_file_option(&run.record_file),
            run.codex_session_id.clone(),
            "canceled",
        )?;
        state.active_runs.remove(&ticket_note);
    }

    Ok(canceled_links)
}

fn process_completed_runs(
    boards: &mut BTreeMap<PathBuf, BoardFile>,
    state: &mut DispatchState,
    options: &DispatchOptions,
    dirty_boards: &mut BTreeSet<PathBuf>,
) -> anyhow::Result<()> {
    let mut completed = Vec::new();
    let active_runs = state.active_runs.clone();
    let hook_state = load_hook_state(&options.vault_path)?;

    for (ticket_note, run) in active_runs {
        match probe_runtime_session_state(&run.namespace)? {
            RuntimeSessionState::ActiveWork => {
                let ticket_path = PathBuf::from(&ticket_note);
                let ticket = TicketNote::load(&ticket_path)?;
                let mut codex_session_id = run.codex_session_id.clone();

                if codex_session_id.is_none() && !options.dry_run {
                    let resolved_session_id = resolve_ticket_session_id(
                        &ticket,
                        run.launched_at_epoch_ms,
                        state
                            .tickets
                            .get(&ticket_note)
                            .and_then(|runtime| runtime.last_codex_session_id.clone()),
                    )?;
                    if let Some(active_run) = state.active_runs.get_mut(&ticket_note) {
                        active_run.codex_session_id = resolved_session_id.clone();
                    }
                    remember_ticket_runtime(
                        state,
                        &ticket_note,
                        Some(run.namespace.clone()),
                        record_file_option(&run.record_file),
                        resolved_session_id.clone(),
                        "active",
                    )?;
                    codex_session_id = resolved_session_id;
                }

                if let Some(stopped_at_epoch_ms) = stop_hook_epoch_ms(
                    &hook_state,
                    codex_session_id.as_deref(),
                    run.launched_at_epoch_ms,
                )? {
                    info!(
                        "Finalizing Codex run '{}' from stop hook at {}",
                        run.namespace, stopped_at_epoch_ms
                    );
                    finalize_run(
                        boards,
                        state,
                        options,
                        &ticket_note,
                        &run,
                        &ticket,
                        codex_session_id,
                        "Run finished after Codex emitted a stop event.",
                        dirty_boards,
                    )?;
                    completed.push(ticket_note);
                }
            }
            runtime_state @ (RuntimeSessionState::Idle | RuntimeSessionState::Missing) => {
                let ticket_path = PathBuf::from(&ticket_note);
                let ticket = TicketNote::load(&ticket_path)?;
                let codex_session_id = resolve_ticket_session_id(
                    &ticket,
                    run.launched_at_epoch_ms,
                    run.codex_session_id.clone().or_else(|| {
                        state
                            .tickets
                            .get(&ticket_note)
                            .and_then(|runtime| runtime.last_codex_session_id.clone())
                    }),
                )?;

                finalize_run(
                    boards,
                    state,
                    options,
                    &ticket_note,
                    &run,
                    &ticket,
                    codex_session_id,
                    match runtime_state {
                        RuntimeSessionState::Idle => "Run finished after the runtime became idle.",
                        RuntimeSessionState::Missing => "Run finished after the runtime stopped.",
                        _ => unreachable!(),
                    },
                    dirty_boards,
                )?;
                completed.push(ticket_note);
            }
        }
    }

    for ticket_note in completed {
        state.active_runs.remove(&ticket_note);
    }

    Ok(())
}

fn finalize_run(
    boards: &mut BTreeMap<PathBuf, BoardFile>,
    state: &mut DispatchState,
    options: &DispatchOptions,
    ticket_note: &str,
    run: &ActiveRunState,
    ticket: &TicketNote,
    codex_session_id: Option<String>,
    completion_reason: &str,
    dirty_boards: &mut BTreeSet<PathBuf>,
) -> anyhow::Result<()> {
    let completion_column = ticket.completion_column();
    let completion_status = ticket.completion_status();
    let finish_mode = ticket.finish_session_policy()?.to_string();

    info!(
        "Finalizing Codex run '{}' into column '{}' with status '{}'",
        run.namespace, completion_column, completion_status
    );

    if !options.dry_run {
        let board_path = PathBuf::from(&run.board_path);
        let board = boards
            .get_mut(&board_path)
            .ok_or_else(|| anyhow!("board '{}' is not loaded", board_path.display()))?;
        board.move_card(&run.ticket_link, &completion_column)?;
        dirty_boards.insert(board.path.clone());
        append_ticket_progress(
            &ticket.path,
            &format!(
                "{} Board moved to '{}' and status set to '{}'.",
                completion_reason, completion_column, completion_status
            ),
        )?;
        update_ticket_status(&ticket.path, &completion_status)?;
        if finish_mode == "close" {
            delete_runtime_session_if_exists(&run.namespace)?;
        }
    }

    remember_ticket_runtime(
        state,
        ticket_note,
        Some(run.namespace.clone()),
        record_file_option(&run.record_file),
        codex_session_id,
        &completion_status,
    )?;

    Ok(())
}

fn load_boards(board_paths: &[PathBuf]) -> anyhow::Result<BTreeMap<PathBuf, BoardFile>> {
    let mut boards = BTreeMap::new();
    for path in board_paths {
        boards.insert(path.clone(), BoardFile::load(path)?);
    }
    Ok(boards)
}

fn should_load_board(
    board_path: &Path,
    snapshot: Option<&BoardSnapshotState>,
    force: bool,
) -> anyhow::Result<bool> {
    if force || snapshot.is_none() {
        return Ok(true);
    }
    let signature = board_file_signature(board_path)?;
    let snapshot = snapshot.expect("checked is_some above");
    Ok(
        snapshot.file_modified_epoch_ms != Some(signature.modified_epoch_ms)
            || snapshot.file_len_bytes != Some(signature.len_bytes),
    )
}

fn refresh_board_snapshot(
    state: &mut DispatchState,
    board_path: &Path,
    board: &BoardFile,
) -> anyhow::Result<()> {
    let signature = board_file_signature(board_path)?;
    state.boards.insert(
        board_path.display().to_string(),
        BoardSnapshotState {
            updated_at_epoch_ms: now_epoch_ms()?,
            file_modified_epoch_ms: Some(signature.modified_epoch_ms),
            file_len_bytes: Some(signature.len_bytes),
            card_columns: board
                .card_positions()
                .into_iter()
                .collect::<BTreeMap<String, String>>(),
        },
    );
    Ok(())
}

fn board_file_signature(board_path: &Path) -> anyhow::Result<BoardFileSignature> {
    let metadata = fs::metadata(board_path)
        .with_context(|| format!("failed to stat board '{}'", board_path.display()))?;
    let modified = metadata.modified().with_context(|| {
        format!(
            "failed to read modified time for '{}'",
            board_path.display()
        )
    })?;
    let modified_epoch_ms = modified
        .duration_since(UNIX_EPOCH)
        .with_context(|| {
            format!(
                "board '{}' has mtime before UNIX_EPOCH",
                board_path.display()
            )
        })?
        .as_millis();
    Ok(BoardFileSignature {
        modified_epoch_ms,
        len_bytes: metadata.len(),
    })
}

fn resolve_board_paths(options: &DispatchOptions) -> anyhow::Result<Vec<PathBuf>> {
    let mut board_paths = if options.boards.is_empty() {
        discover_default_boards(&options.vault_path)?
    } else {
        options
            .boards
            .iter()
            .map(|path| {
                if path.is_absolute() {
                    path.clone()
                } else {
                    options.vault_path.join(path)
                }
            })
            .collect::<Vec<_>>()
    };

    board_paths.sort();
    board_paths.dedup();
    Ok(board_paths)
}

fn default_state_file(vault_path: &Path) -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let path = PathBuf::from(home)
        .join(".jarvis")
        .join("dispatch")
        .join(format!(
            "{}-state.json",
            vault_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "vault".to_string())
        ));
    Ok(path)
}

fn load_hook_state(vault_path: &Path) -> anyhow::Result<HookStateFile> {
    let path = vault_path.join(".ops").join("hooks").join("state.json");
    if !path.exists() {
        return Ok(HookStateFile::default());
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read hook state '{}'", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse hook state '{}'", path.display()))
}

fn load_state(path: &Path) -> anyhow::Result<DispatchState> {
    if !path.exists() {
        return Ok(DispatchState::default());
    }

    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read state file '{}'", path.display()))?;
    let mut state: DispatchState = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse '{}'", path.display()))?;
    if state.version < 2 {
        state.version = 2;
    }
    Ok(state)
}

fn save_state(path: &Path, state: &DispatchState) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }

    let raw = serde_json::to_string_pretty(state).context("failed to serialize dispatch state")?;
    fs::write(path, raw).with_context(|| format!("failed to write '{}'", path.display()))
}

fn stop_hook_epoch_ms(
    hook_state: &HookStateFile,
    codex_session_id: Option<&str>,
    launched_at_epoch_ms: u128,
) -> anyhow::Result<Option<u128>> {
    let Some(session_id) = codex_session_id else {
        return Ok(None);
    };
    let Some(session) = hook_state.sessions.get(session_id) else {
        return Ok(None);
    };
    if session
        .last_event
        .as_deref()
        .map(str::trim)
        .filter(|event| event.eq_ignore_ascii_case("Stop"))
        .is_none()
    {
        return Ok(None);
    }

    let Some(last_event_at) = session.last_event_at.as_deref() else {
        return Ok(None);
    };
    let stopped_at_epoch_ms = parse_rfc3339_epoch_ms(last_event_at)?;
    if stopped_at_epoch_ms > launched_at_epoch_ms {
        Ok(Some(stopped_at_epoch_ms))
    } else {
        Ok(None)
    }
}

fn parse_rfc3339_epoch_ms(value: &str) -> anyhow::Result<u128> {
    let parsed = OffsetDateTime::parse(value, &Rfc3339)
        .with_context(|| format!("failed to parse RFC3339 timestamp '{}'", value))?;
    let seconds = i128::from(parsed.unix_timestamp());
    let millis = seconds
        .checked_mul(1000)
        .and_then(|base| base.checked_add(i128::from(parsed.millisecond())))
        .ok_or_else(|| anyhow!("timestamp '{}' overflowed epoch milliseconds", value))?;
    u128::try_from(millis)
        .map_err(|_| anyhow!("timestamp '{}' is before UNIX_EPOCH and unsupported", value))
}

fn resolve_ticket_session_id(
    ticket: &TicketNote,
    launched_at_epoch_ms: u128,
    existing_session_id: Option<String>,
) -> anyhow::Result<Option<String>> {
    if existing_session_id.is_some() {
        return Ok(existing_session_id);
    }

    let historical =
        discover_latest_launch_session_id(&ticket.path, &default_namespace_for_ticket(ticket))?;
    if historical.is_some() {
        return Ok(historical);
    }

    let Some(repo_path) = ticket.repo_path() else {
        return Ok(None);
    };

    discover_codex_session_id(&ticket.path, Path::new(repo_path), launched_at_epoch_ms)
}

fn column_for_ticket(board: &BoardFile, ticket_link: &str) -> Option<String> {
    board
        .card_positions()
        .into_iter()
        .find(|(link, _)| link == ticket_link)
        .map(|(_, column)| column)
}

fn remember_ticket_runtime(
    state: &mut DispatchState,
    ticket_key: &str,
    namespace: Option<String>,
    record_file: Option<String>,
    codex_session_id: Option<String>,
    outcome: &str,
) -> anyhow::Result<()> {
    let runtime = state.tickets.entry(ticket_key.to_string()).or_default();
    if let Some(namespace) = namespace {
        runtime.last_namespace = Some(namespace);
    }
    if let Some(record_file) = record_file {
        runtime.last_record_file = Some(record_file);
    }
    if let Some(codex_session_id) = codex_session_id {
        runtime.last_codex_session_id = Some(codex_session_id);
    }
    runtime.last_outcome = Some(outcome.to_string());
    runtime.last_transition_epoch_ms = Some(now_epoch_ms()?);
    Ok(())
}

fn record_file_option(record_file: &str) -> Option<String> {
    if record_file.trim().is_empty() {
        None
    } else {
        Some(record_file.to_string())
    }
}

fn update_ticket_status(path: &Path, new_status: &str) -> anyhow::Result<()> {
    update_ticket_frontmatter_value(path, "status", new_status)
}

fn update_ticket_frontmatter_value(path: &Path, field: &str, value: &str) -> anyhow::Result<()> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read ticket '{}'", path.display()))?;
    let mut lines = raw.lines().map(ToOwned::to_owned).collect::<Vec<_>>();

    if lines.first().map(|line| line.trim()) != Some("---") {
        return Err(anyhow!(
            "ticket '{}' does not start with YAML frontmatter",
            path.display()
        ));
    }

    let prefix = format!("{}:", field);
    let mut found = false;
    let mut insert_index = None;
    for (index, line) in lines.iter_mut().enumerate().skip(1) {
        if line.trim() == "---" {
            insert_index = Some(index);
            break;
        }
        if line.starts_with(&prefix) {
            *line = format!("{} {}", prefix, value);
            found = true;
            break;
        }
    }

    if !found {
        let Some(index) = insert_index else {
            return Err(anyhow!(
                "ticket '{}' has unterminated YAML frontmatter",
                path.display()
            ));
        };
        lines.insert(index, format!("{} {}", prefix, value));
    }

    let mut rendered = lines.join("\n");
    rendered.push('\n');
    fs::write(path, rendered)
        .with_context(|| format!("failed to write ticket '{}'", path.display()))
}

fn append_ticket_progress(path: &Path, message: &str) -> anyhow::Result<()> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read ticket '{}'", path.display()))?;
    let mut lines = raw.lines().map(ToOwned::to_owned).collect::<Vec<_>>();
    let progress_line = format!("- {}", message.trim());

    let mut progress_heading = None;
    for (index, line) in lines.iter().enumerate() {
        if line.trim() == "## Progress" {
            progress_heading = Some(index);
            break;
        }
    }

    match progress_heading {
        Some(heading_index) => {
            let mut insert_index = lines.len();
            for (index, line) in lines.iter().enumerate().skip(heading_index + 1) {
                if line.starts_with("## ") {
                    insert_index = index;
                    break;
                }
            }
            lines.insert(insert_index, progress_line);
        }
        None => {
            if !lines.last().map(|line| line.is_empty()).unwrap_or(false) {
                lines.push(String::new());
            }
            lines.push("## Progress".to_string());
            lines.push(progress_line);
        }
    }

    let mut rendered = lines.join("\n");
    rendered.push('\n');
    fs::write(path, rendered)
        .with_context(|| format!("failed to write ticket '{}'", path.display()))
}

fn now_epoch_ms() -> anyhow::Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?
        .as_millis())
}
