use crate::SessionBackend;
use crate::codex_app::{CodexAppLaunchManifest, spawn_codex_app_session};
use crate::native::{NativeSessionMetadata, RuntimeContextMetadata};
use crate::ticket::{TicketNote, slugify};
use anyhow::{Context, ensure};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

#[derive(Debug, Clone, Copy, Eq, PartialEq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexRuntimeDriver {
    CliPty,
    AppServer,
}

#[derive(Debug, Clone)]
pub struct CodexLaunchOptions {
    pub backend: SessionBackend,
    pub driver: CodexRuntimeDriver,
    pub task_note: PathBuf,
    pub namespace: Option<String>,
    pub agents: usize,
    pub agent: String,
    pub fresh_session: bool,
    pub resume_session_id: Option<String>,
    pub working_directory: Option<PathBuf>,
    pub prompt_file: Option<PathBuf>,
    pub operator_message: Option<String>,
    pub images: Vec<PathBuf>,
    pub environment: BTreeMap<String, String>,
    pub context_overlay: RuntimeContextMetadata,
    pub extra_runtime_args: Vec<String>,
    pub startup_delay_ms: u64,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexLaunchRecord {
    pub version: u32,
    pub launched_at_epoch_ms: u128,
    pub task_id: String,
    pub title: String,
    pub task_note: String,
    pub repo_path: String,
    pub namespace: String,
    pub agent: String,
    pub agents: usize,
    pub runtime_backend: String,
    pub command: Vec<String>,
    pub launch_mode: String,
    pub codex_session_id: Option<String>,
    pub finish_mode: String,
    pub prompt_file: String,
    pub record_file: String,
    pub readiness_warnings: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SessionMetaEnvelope {
    #[serde(rename = "type")]
    entry_type: String,
    payload: SessionMetaPayload,
}

#[derive(Debug, Deserialize)]
struct SessionMetaPayload {
    id: String,
    cwd: String,
    timestamp: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct HistoricalLaunchRecord {
    task_note: String,
    namespace: String,
    launched_at_epoch_ms: u128,
    codex_session_id: Option<String>,
}

pub fn default_namespace_for_ticket(ticket: &TicketNote) -> String {
    format!("codex-{}", default_namespace_suffix(ticket))
}

pub fn launch_codex_ticket(options: CodexLaunchOptions) -> anyhow::Result<CodexLaunchRecord> {
    let ticket = TicketNote::load(&options.task_note)?;
    ticket.validate_codex_minimum()?;

    let repo_path = options
        .working_directory
        .clone()
        .unwrap_or_else(|| PathBuf::from(ticket.repo_path().expect("validated repo_path")));
    ensure!(
        repo_path.exists(),
        "working directory '{}' does not exist",
        repo_path.display()
    );

    let namespace = options
        .namespace
        .clone()
        .unwrap_or_else(|| default_namespace_for_ticket(&ticket));
    let resume_session_id = resolve_resume_session_id(&ticket, &namespace, &options)?;
    let runtime_args = ticket.codex_cli_args()?;
    let finish_mode = ticket.finish_session_policy()?.to_string();
    let command = match options.driver {
        CodexRuntimeDriver::CliPty => {
            let mut runtime_args_with_images = runtime_args.clone();
            runtime_args_with_images.extend(options.extra_runtime_args.clone());
            runtime_args_with_images.extend(codex_image_args(&options.images)?);
            let base_command = build_base_command(&options.command, resume_session_id.as_deref());
            let command = with_codex_runtime_args(base_command, &runtime_args_with_images);
            with_environment(command, &options.environment)
        }
        CodexRuntimeDriver::AppServer => {
            let mut runtime_args_with_extras = runtime_args.clone();
            runtime_args_with_extras.extend(options.extra_runtime_args.clone());
            let base_command = build_app_server_command(&options.command);
            let command = with_codex_runtime_args(base_command, &runtime_args_with_extras);
            with_environment(command, &options.environment)
        }
    };
    let prompt = build_codex_prompt(
        &ticket,
        options.prompt_file.as_deref(),
        options.operator_message.as_deref(),
        resume_session_id.is_some(),
    )?;

    let jarvis_home = jarvis_home()?;
    let prompts_dir = jarvis_home.join("codex").join("prompts");
    let runs_dir = jarvis_home.join("codex").join("runs");
    fs::create_dir_all(&prompts_dir)
        .with_context(|| format!("failed to create '{}'", prompts_dir.display()))?;
    fs::create_dir_all(&runs_dir)
        .with_context(|| format!("failed to create '{}'", runs_dir.display()))?;

    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?
        .as_millis();
    let prompt_path = prompts_dir.join(format!(
        "{}-{}-{}.md",
        namespace,
        slugify(&ticket.title),
        timestamp_ms
    ));
    fs::write(&prompt_path, &prompt)
        .with_context(|| format!("failed to write prompt bundle '{}'", prompt_path.display()))?;

    let record_path = runs_dir.join(format!("{}-{}.json", namespace, timestamp_ms));

    ensure_project_trusted(&repo_path)?;

    let runtime_context = merge_runtime_context(
        RuntimeContextMetadata {
            workload: Some("codex".to_string()),
            task_id: Some(ticket.effective_id()),
            task_title: Some(ticket.title.clone()),
            task_note: Some(ticket.path.display().to_string()),
            launch_mode: Some(if resume_session_id.is_some() {
                "resume".to_string()
            } else {
                "fresh".to_string()
            }),
            codex_session_id: resume_session_id.clone(),
            prompt_file: Some(prompt_path.display().to_string()),
            record_file: Some(record_path.display().to_string()),
            transcript_path: None,
            event_log_path: None,
            thread_id: None,
            thread_status: None,
            turn_id: None,
            turn_status: None,
            live_message: None,
            last_activity: None,
            last_error: None,
            control_namespace: None,
            deployment: None,
            labels: BTreeMap::new(),
            config_maps: Vec::new(),
            secrets: Vec::new(),
            volumes: Vec::new(),
            recent_events: Vec::new(),
            subagents: Vec::new(),
        },
        options.context_overlay.clone(),
    );
    let (runtime_backend, codex_session_id) = match options.driver {
        CodexRuntimeDriver::CliPty => {
            let mut launch_command = if launches_codex(&command) {
                let mut wrapped = vec!["env".to_string(), "NO_UPDATE_NOTIFIER=1".to_string()];
                wrapped.extend(command.clone());
                wrapped
            } else {
                command.clone()
            };
            launch_command.push(prompt.clone());

            let wrapped_command = wrap_launch_command(&finish_mode, &launch_command);
            let resume_transcript_path = match resume_session_id.as_deref() {
                Some(session_id) => transcript_path_for_session_id(session_id)?,
                None => None,
            };
            super::run_session_shell_with_context(
                options.backend,
                &namespace,
                options.agents,
                &Some(repo_path.display().to_string()),
                &wrapped_command,
                Some(RuntimeContextMetadata {
                    transcript_path: resume_transcript_path,
                    ..runtime_context.clone()
                }),
            )?;

            if options.startup_delay_ms > 0 {
                thread::sleep(Duration::from_millis(options.startup_delay_ms));
            }

            (
                "native".to_string(),
                match resume_session_id.clone() {
                    Some(session_id) => Some(session_id),
                    None => discover_codex_session_id(&ticket.path, &repo_path, timestamp_ms)?,
                },
            )
        }
        CodexRuntimeDriver::AppServer => {
            let metadata = spawn_codex_app_session(CodexAppLaunchManifest {
                namespace: namespace.clone(),
                working_directory: Some(repo_path.display().to_string()),
                shell_command: shell_words::join(&command),
                startup_prompt: prompt.clone(),
                images: options
                    .images
                    .iter()
                    .map(|image| image.display().to_string())
                    .collect(),
                environment: options.environment.clone(),
                resume_session_id: resume_session_id.clone(),
                created_at_epoch_ms: timestamp_ms,
                context: runtime_context.clone(),
            })?;
            let context = metadata.context.unwrap_or_default();
            (
                "codex-app".to_string(),
                context.codex_session_id.or(context.thread_id),
            )
        }
    };

    let readiness_warnings = ticket.readiness_warnings();
    let record = CodexLaunchRecord {
        version: 3,
        launched_at_epoch_ms: timestamp_ms,
        task_id: ticket.effective_id(),
        title: ticket.title.clone(),
        task_note: ticket.path.display().to_string(),
        repo_path: repo_path.display().to_string(),
        namespace: namespace.clone(),
        agent: options.agent.clone(),
        agents: options.agents,
        runtime_backend,
        command,
        launch_mode: if resume_session_id.is_some() {
            "resume".to_string()
        } else {
            "fresh".to_string()
        },
        codex_session_id,
        finish_mode,
        prompt_file: prompt_path.display().to_string(),
        record_file: record_path.display().to_string(),
        readiness_warnings,
    };

    let record_json =
        serde_json::to_string_pretty(&record).context("failed to serialize launch record")?;
    fs::write(&record_path, record_json)
        .with_context(|| format!("failed to write launch record '{}'", record_path.display()))?;

    Ok(record)
}

fn jarvis_home() -> anyhow::Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".jarvis"))
}

fn default_namespace_suffix(ticket: &TicketNote) -> String {
    let stem = ticket
        .path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| ticket.effective_id());
    slugify(&stem)
}

fn resolve_resume_session_id(
    ticket: &TicketNote,
    namespace: &str,
    options: &CodexLaunchOptions,
) -> anyhow::Result<Option<String>> {
    if options.fresh_session {
        return Ok(None);
    }
    if options.resume_session_id.is_some() {
        return Ok(options.resume_session_id.clone());
    }

    discover_latest_launch_session_id(&ticket.path, namespace)
}

fn build_codex_prompt(
    ticket: &TicketNote,
    prompt_file: Option<&Path>,
    operator_message: Option<&str>,
    resuming: bool,
) -> anyhow::Result<String> {
    let trimmed_message = operator_message
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let mut prompt = if let Some(prompt_file) = prompt_file {
        fs::read_to_string(prompt_file)
            .with_context(|| format!("failed to read prompt file '{}'", prompt_file.display()))?
    } else if resuming {
        if let Some(message) = trimmed_message {
            format!(
                "Continue work for '{}' at '{}'.\n\nOperator update:\n{}",
                ticket.title,
                ticket.path.display(),
                message
            )
        } else {
            ticket.render_codex_prompt()
        }
    } else {
        ticket.render_codex_prompt()
    };

    if let Some(message) = trimmed_message {
        if prompt_file.is_some() || !resuming {
            if !prompt.ends_with('\n') {
                prompt.push('\n');
            }
            prompt.push_str("\nOperator update:\n");
            prompt.push_str(message);
        }
    }

    Ok(prompt)
}

fn codex_image_args(images: &[PathBuf]) -> anyhow::Result<Vec<String>> {
    let mut args = Vec::new();
    for image in images {
        ensure!(image.exists(), "image '{}' does not exist", image.display());
        args.push("--image".to_string());
        args.push(image.display().to_string());
    }
    Ok(args)
}

fn wrap_launch_command(finish_policy: &str, launch_command: &[String]) -> String {
    let joined = shell_words::join(launch_command);
    match finish_policy {
        "keep" => format!(
            "{}; status=$?; echo; echo \"[jarvisctl] Codex exited with status $status. Leaving an interactive shell open.\"; exec bash -li",
            joined
        ),
        _ => joined,
    }
}

fn build_base_command(command: &[String], resume_session_id: Option<&str>) -> Vec<String> {
    let mut base_command = if command.is_empty() {
        vec!["codex".to_string()]
    } else {
        command.to_vec()
    };

    let Some(session_id) = resume_session_id else {
        return base_command;
    };

    let Some(codex_index) = base_command.iter().position(|part| part == "codex") else {
        return base_command;
    };
    let subcommand_index = codex_index + 1;

    match base_command.get(subcommand_index).map(String::as_str) {
        Some("resume") => {
            if base_command.get(subcommand_index + 1).is_none() {
                base_command.insert(subcommand_index + 1, session_id.to_string());
            }
        }
        _ => {
            base_command.insert(subcommand_index, "resume".to_string());
            base_command.insert(subcommand_index + 1, session_id.to_string());
        }
    }

    base_command
}

fn build_app_server_command(command: &[String]) -> Vec<String> {
    let mut base_command = if command.is_empty() {
        vec!["codex".to_string()]
    } else {
        command.to_vec()
    };

    let Some(insert_index) = codex_subcommand_insert_index(&base_command) else {
        return base_command;
    };

    if base_command.get(insert_index).map(String::as_str) != Some("app-server") {
        base_command.insert(insert_index, "app-server".to_string());
    }

    base_command
}

fn with_codex_runtime_args(command: Vec<String>, runtime_args: &[String]) -> Vec<String> {
    if runtime_args.is_empty() {
        return command;
    }

    let Some(insert_index) = codex_insert_index(&command) else {
        return command;
    };

    let mut combined = command;
    combined.splice(insert_index..insert_index, runtime_args.iter().cloned());
    combined
}

fn with_environment(command: Vec<String>, environment: &BTreeMap<String, String>) -> Vec<String> {
    if environment.is_empty() {
        return command;
    }

    let mut wrapped = vec!["env".to_string()];
    for (key, value) in environment {
        wrapped.push(format!("{}={}", key, value));
    }
    wrapped.extend(command);
    wrapped
}

fn merge_runtime_context(
    mut base: RuntimeContextMetadata,
    overlay: RuntimeContextMetadata,
) -> RuntimeContextMetadata {
    if overlay.workload.is_some() {
        base.workload = overlay.workload;
    }
    if overlay.task_id.is_some() {
        base.task_id = overlay.task_id;
    }
    if overlay.task_title.is_some() {
        base.task_title = overlay.task_title;
    }
    if overlay.task_note.is_some() {
        base.task_note = overlay.task_note;
    }
    if overlay.launch_mode.is_some() {
        base.launch_mode = overlay.launch_mode;
    }
    if overlay.codex_session_id.is_some() {
        base.codex_session_id = overlay.codex_session_id;
    }
    if overlay.prompt_file.is_some() {
        base.prompt_file = overlay.prompt_file;
    }
    if overlay.record_file.is_some() {
        base.record_file = overlay.record_file;
    }
    if overlay.transcript_path.is_some() {
        base.transcript_path = overlay.transcript_path;
    }
    if overlay.event_log_path.is_some() {
        base.event_log_path = overlay.event_log_path;
    }
    if overlay.thread_id.is_some() {
        base.thread_id = overlay.thread_id;
    }
    if overlay.thread_status.is_some() {
        base.thread_status = overlay.thread_status;
    }
    if overlay.turn_id.is_some() {
        base.turn_id = overlay.turn_id;
    }
    if overlay.turn_status.is_some() {
        base.turn_status = overlay.turn_status;
    }
    if overlay.live_message.is_some() {
        base.live_message = overlay.live_message;
    }
    if overlay.last_activity.is_some() {
        base.last_activity = overlay.last_activity;
    }
    if overlay.last_error.is_some() {
        base.last_error = overlay.last_error;
    }
    if overlay.control_namespace.is_some() {
        base.control_namespace = overlay.control_namespace;
    }
    if overlay.deployment.is_some() {
        base.deployment = overlay.deployment;
    }
    if !overlay.labels.is_empty() {
        base.labels.extend(overlay.labels);
    }
    if !overlay.config_maps.is_empty() {
        base.config_maps = overlay.config_maps;
    }
    if !overlay.secrets.is_empty() {
        base.secrets = overlay.secrets;
    }
    if !overlay.volumes.is_empty() {
        base.volumes = overlay.volumes;
    }
    if !overlay.recent_events.is_empty() {
        base.recent_events = overlay.recent_events;
    }
    if !overlay.subagents.is_empty() {
        base.subagents = overlay.subagents;
    }
    base
}

fn launches_codex(command: &[String]) -> bool {
    codex_insert_index(command).is_some()
}

fn codex_insert_index(command: &[String]) -> Option<usize> {
    let codex_index = match command.first().map(String::as_str) {
        Some("codex") => Some(0),
        Some("env") => command.iter().position(|part| part == "codex"),
        _ => None,
    }?;
    let after_codex = codex_index + 1;
    if command.get(after_codex).map(String::as_str) == Some("resume") {
        Some(after_codex + 1)
    } else {
        Some(after_codex)
    }
}

fn codex_subcommand_insert_index(command: &[String]) -> Option<usize> {
    let codex_index = match command.first().map(String::as_str) {
        Some("codex") => Some(0),
        Some("env") => command.iter().position(|part| part == "codex"),
        _ => None,
    }?;
    Some(codex_index + 1)
}

fn ensure_project_trusted(repo_path: &Path) -> anyhow::Result<()> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    let config_path = PathBuf::from(home).join(".codex").join("config.toml");
    if !config_path.exists() {
        return Ok(());
    }

    let raw = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read '{}'", config_path.display()))?;
    let project_key = repo_path.to_string_lossy().to_string();
    let section_header = format!("[projects.\"{}\"]", project_key);

    let original = raw.clone();
    let updated = if let Some(section_start) = raw.find(&section_header) {
        let section_body_start = raw[section_start..]
            .find('\n')
            .map(|offset| section_start + offset + 1)
            .unwrap_or(raw.len());
        let section_end = raw[section_body_start..]
            .find("\n[")
            .map(|offset| section_body_start + offset)
            .unwrap_or(raw.len());
        let section_body = &raw[section_body_start..section_end];

        if section_body.contains("trust_level = \"trusted\"") {
            raw
        } else if section_body.contains("trust_level = ") {
            let replaced = section_body
                .lines()
                .map(|line| {
                    if line.trim_start().starts_with("trust_level = ") {
                        "trust_level = \"trusted\"".to_string()
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "{}{}{}",
                &raw[..section_body_start],
                replaced,
                &raw[section_end..]
            )
        } else {
            format!(
                "{}trust_level = \"trusted\"\n{}",
                &raw[..section_body_start],
                &raw[section_body_start..]
            )
        }
    } else {
        let mut appended = raw;
        if !appended.ends_with('\n') {
            appended.push('\n');
        }
        appended.push('\n');
        appended.push_str(&section_header);
        appended.push('\n');
        appended.push_str("trust_level = \"trusted\"\n");
        appended
    };

    if updated != original {
        fs::write(&config_path, updated)
            .with_context(|| format!("failed to write '{}'", config_path.display()))?;
    }

    Ok(())
}

pub(crate) fn discover_codex_session_id(
    ticket_path: &Path,
    repo_path: &Path,
    launched_at_epoch_ms: u128,
) -> anyhow::Result<Option<String>> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    let sessions_dir = PathBuf::from(home).join(".codex").join("sessions");
    if !sessions_dir.exists() {
        return Ok(None);
    }

    let mut files = Vec::new();
    collect_session_files(&sessions_dir, &mut files)?;
    files.sort();

    let ticket_marker = ticket_path.display().to_string();
    let repo_marker = repo_path.display().to_string();
    let mut best_match: Option<(bool, u128, String)> = None;

    for file in files {
        let raw = match fs::read_to_string(&file) {
            Ok(raw) => raw,
            Err(_) => continue,
        };

        let Some(first_line) = raw.lines().next() else {
            continue;
        };
        let envelope: SessionMetaEnvelope = match serde_json::from_str(first_line) {
            Ok(envelope) => envelope,
            Err(_) => continue,
        };
        if envelope.entry_type != "session_meta" || envelope.payload.cwd != repo_marker {
            continue;
        }

        let session_started_at_epoch_ms = match parse_rfc3339_epoch_ms(&envelope.payload.timestamp)
        {
            Some(epoch_ms) => epoch_ms,
            None => continue,
        };
        let delta = session_started_at_epoch_ms.abs_diff(launched_at_epoch_ms);
        if delta > 300_000 {
            continue;
        }

        let has_ticket_marker = raw.contains(&ticket_marker);
        let replace = match best_match.as_ref() {
            Some((best_has_ticket_marker, best_delta, _)) => {
                (has_ticket_marker && !best_has_ticket_marker)
                    || (has_ticket_marker == *best_has_ticket_marker && delta <= *best_delta)
            }
            None => true,
        };
        if replace {
            best_match = Some((has_ticket_marker, delta, envelope.payload.id));
        }
    }

    Ok(best_match.map(|(_, _, session_id)| session_id))
}

fn collect_session_files(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read '{}'", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_session_files(&path, out)?;
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }

    Ok(())
}

fn parse_rfc3339_epoch_ms(raw: &str) -> Option<u128> {
    let datetime = OffsetDateTime::parse(raw, &Rfc3339).ok()?;
    let seconds = i128::from(datetime.unix_timestamp());
    let milliseconds = i128::from(datetime.millisecond());
    let epoch_ms = seconds.checked_mul(1_000)?.checked_add(milliseconds)?;
    u128::try_from(epoch_ms).ok()
}

pub(crate) fn discover_latest_launch_session_id(
    ticket_path: &Path,
    namespace: &str,
) -> anyhow::Result<Option<String>> {
    let runs_dir = jarvis_home()?.join("codex").join("runs");
    discover_latest_launch_session_id_in_dir(&runs_dir, ticket_path, namespace)
}

fn discover_latest_launch_session_id_in_dir(
    runs_dir: &Path,
    ticket_path: &Path,
    namespace: &str,
) -> anyhow::Result<Option<String>> {
    if !runs_dir.exists() {
        return Ok(None);
    }

    let ticket_marker = canonical_path_string(ticket_path);
    let mut best_match: Option<(u128, String)> = None;

    for entry in fs::read_dir(runs_dir)
        .with_context(|| format!("failed to read '{}'", runs_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }

        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        let record: HistoricalLaunchRecord = match serde_json::from_str(&raw) {
            Ok(record) => record,
            Err(_) => continue,
        };
        let Some(session_id) = record.codex_session_id else {
            continue;
        };

        let task_matches = canonical_path_string(Path::new(&record.task_note)) == ticket_marker;
        let namespace_matches = record.namespace == namespace;
        if !task_matches && !namespace_matches {
            continue;
        }

        let replace = match best_match.as_ref() {
            Some((best_epoch_ms, _)) => record.launched_at_epoch_ms >= *best_epoch_ms,
            None => true,
        };
        if replace {
            best_match = Some((record.launched_at_epoch_ms, session_id));
        }
    }

    Ok(best_match.map(|(_, session_id)| session_id))
}

fn canonical_path_string(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

pub fn enrich_native_sessions(sessions: &mut [NativeSessionMetadata]) -> anyhow::Result<()> {
    let launch_records = latest_launch_records_by_namespace()?;
    let transcript_paths = transcript_paths_by_session_id()?;

    for session in sessions {
        let looks_like_codex = session
            .context
            .as_ref()
            .and_then(|context| context.workload.as_deref())
            == Some("codex")
            || session.shell_command.contains("codex");

        if !looks_like_codex {
            continue;
        }

        let Some(record) = launch_records.get(&session.namespace) else {
            if let Some(context) = session.context.as_mut() {
                fill_transcript_path(context, &transcript_paths);
            }
            continue;
        };

        if !launch_record_matches_session(record, session) {
            continue;
        }

        let context = session
            .context
            .get_or_insert_with(RuntimeContextMetadata::default);
        if context.workload.is_none() {
            context.workload = Some("codex".to_string());
        }
        if context.task_id.is_none() {
            context.task_id = Some(record.task_id.clone());
        }
        if context.task_title.is_none() {
            context.task_title = Some(record.title.clone());
        }
        if context.task_note.is_none() {
            context.task_note = Some(record.task_note.clone());
        }
        if context.launch_mode.is_none() {
            context.launch_mode = Some(record.launch_mode.clone());
        }
        if context.codex_session_id.is_none() {
            context.codex_session_id = record.codex_session_id.clone();
        }
        if context.prompt_file.is_none() {
            context.prompt_file = Some(record.prompt_file.clone());
        }
        if context.record_file.is_none() {
            context.record_file = Some(record.record_file.clone());
        }
        fill_transcript_path(context, &transcript_paths);
    }

    Ok(())
}

pub fn transcript_path_for_session_id(session_id: &str) -> anyhow::Result<Option<String>> {
    let mut transcripts = transcript_paths_by_session_id()?;
    Ok(transcripts.remove(session_id))
}

fn latest_launch_records_by_namespace() -> anyhow::Result<BTreeMap<String, CodexLaunchRecord>> {
    let runs_dir = jarvis_home()?.join("codex").join("runs");
    if !runs_dir.exists() {
        return Ok(BTreeMap::new());
    }

    let mut records: BTreeMap<String, CodexLaunchRecord> = BTreeMap::new();
    for entry in fs::read_dir(&runs_dir)
        .with_context(|| format!("failed to read '{}'", runs_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }

        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        let record: CodexLaunchRecord = match serde_json::from_str(&raw) {
            Ok(record) => record,
            Err(_) => continue,
        };

        let replace = match records.get(&record.namespace) {
            Some(existing) => record.launched_at_epoch_ms >= existing.launched_at_epoch_ms,
            None => true,
        };
        if replace {
            records.insert(record.namespace.clone(), record);
        }
    }

    Ok(records)
}

fn transcript_paths_by_session_id() -> anyhow::Result<BTreeMap<String, String>> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    let sessions_dir = PathBuf::from(home).join(".codex").join("sessions");
    if !sessions_dir.exists() {
        return Ok(BTreeMap::new());
    }

    let mut files = Vec::new();
    collect_session_files(&sessions_dir, &mut files)?;
    files.sort();

    let mut transcripts = BTreeMap::new();
    for file in files {
        let Some(envelope) = read_session_meta(&file) else {
            continue;
        };
        if envelope.entry_type != "session_meta" {
            continue;
        }
        transcripts.insert(envelope.payload.id, file.display().to_string());
    }

    Ok(transcripts)
}

fn fill_transcript_path(
    context: &mut RuntimeContextMetadata,
    transcript_paths: &BTreeMap<String, String>,
) {
    if context.transcript_path.is_some() {
        return;
    }
    let Some(session_id) = context.codex_session_id.as_deref() else {
        return;
    };
    if let Some(path) = transcript_paths.get(session_id) {
        context.transcript_path = Some(path.clone());
    }
}

fn launch_record_matches_session(
    record: &CodexLaunchRecord,
    session: &NativeSessionMetadata,
) -> bool {
    if record
        .launched_at_epoch_ms
        .abs_diff(session.created_at_epoch_ms)
        > 600_000
    {
        return false;
    }

    let Some(working_directory) = session.working_directory.as_deref() else {
        return true;
    };
    canonical_path_string(Path::new(&record.repo_path))
        == canonical_path_string(Path::new(working_directory))
}

fn read_session_meta(path: &Path) -> Option<SessionMetaEnvelope> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut first_line = String::new();
    if reader.read_line(&mut first_line).ok()? == 0 {
        return None;
    }
    let first_line = first_line.trim_end_matches(['\r', '\n']);
    serde_json::from_str(first_line).ok()
}

#[cfg(test)]
mod tests {
    use super::{
        HistoricalLaunchRecord, build_base_command, build_codex_prompt, canonical_path_string,
        discover_latest_launch_session_id_in_dir,
    };
    use crate::ticket::TicketNote;
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn build_base_command_inserts_resume_id_when_missing() {
        let command = build_base_command(&["codex".to_string()], Some("session-123"));
        assert_eq!(command, vec!["codex", "resume", "session-123"]);
    }

    #[test]
    fn build_codex_prompt_uses_operator_update_for_resume() {
        let root = unique_temp_dir("jarvisctl-codex-prompt");
        fs::create_dir_all(&root).unwrap();
        let ticket_path = root.join("ticket.md");
        fs::write(
            &ticket_path,
            r#"---
title: Demo Ticket
type: ticket
repo_path: /tmp/repo
---

# Demo Ticket
"#,
        )
        .unwrap();

        let ticket = TicketNote::load(&ticket_path).unwrap();
        let prompt =
            build_codex_prompt(&ticket, None, Some("Need one more validation pass."), true)
                .unwrap();

        assert!(prompt.contains("Continue work for 'Demo Ticket'"));
        assert!(prompt.contains("Operator update:\nNeed one more validation pass."));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn discover_latest_launch_session_id_prefers_latest_matching_record() {
        let root = unique_temp_dir("jarvisctl-codex-runs");
        let runs_dir = root.join("runs");
        fs::create_dir_all(&runs_dir).unwrap();
        let ticket_path = root.join("ticket.md");
        fs::write(&ticket_path, "# Ticket\n").unwrap();

        let older = HistoricalLaunchRecord {
            task_note: canonical_path_string(&ticket_path),
            namespace: "codex-ticket".to_string(),
            launched_at_epoch_ms: 10,
            codex_session_id: Some("session-old".to_string()),
        };
        let newer = HistoricalLaunchRecord {
            task_note: canonical_path_string(&ticket_path),
            namespace: "codex-ticket".to_string(),
            launched_at_epoch_ms: 20,
            codex_session_id: Some("session-new".to_string()),
        };
        fs::write(
            runs_dir.join("older.json"),
            serde_json::to_string(&older).unwrap(),
        )
        .unwrap();
        fs::write(
            runs_dir.join("newer.json"),
            serde_json::to_string(&newer).unwrap(),
        )
        .unwrap();

        let resolved = discover_latest_launch_session_id_in_dir(
            &runs_dir,
            Path::new(&ticket_path),
            "codex-ticket",
        )
        .unwrap();
        assert_eq!(resolved.as_deref(), Some("session-new"));
        let _ = fs::remove_dir_all(root);
    }

    fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{}-{}", prefix, nanos))
    }
}
