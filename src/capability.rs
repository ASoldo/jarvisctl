use crate::control_plane::{ControlPlaneOutput, NodePairSessionOptions, start_node_pair_session};
use crate::mission::{
    MissionCreateOptions, MissionEventOptions, MissionRecord, append_mission_event,
    complete_mission, create_mission,
};
use crate::operator_request::{
    OperatorRequestRecord, expire_operator_request, list_operator_requests,
    notify_operator_requests,
};
use crate::proposal::ProposalRecord;
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityValidator {
    pub id: String,
    pub kind: String,
    pub target: String,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityArtifactContract {
    pub id: String,
    pub path_hint: String,
    pub required: bool,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityRecord {
    pub id: String,
    pub title: String,
    pub lane: String,
    pub status: String,
    pub confidence: u8,
    pub schedulable: bool,
    pub description: String,
    pub validators: Vec<CapabilityValidator>,
    pub artifact_contracts: Vec<CapabilityArtifactContract>,
    pub evidence: Vec<String>,
    pub gaps: Vec<String>,
    pub created_at_epoch_ms: u128,
    pub updated_at_epoch_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityValidatorResult {
    pub id: String,
    pub kind: String,
    pub target: String,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityValidationReport {
    pub id: String,
    pub status: String,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub results: Vec<CapabilityValidatorResult>,
}

#[derive(Debug, Clone)]
pub struct CapabilityRegisterOptions {
    pub id: String,
    pub title: String,
    pub lane: String,
    pub status: Option<String>,
    pub confidence: Option<u8>,
    pub schedulable: bool,
    pub description: String,
    pub evidence: Vec<String>,
    pub gaps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionSmokeReport {
    pub id: String,
    pub status: String,
    pub dry_run: bool,
    pub first_node: String,
    pub second_node: String,
    pub mission_id: String,
    pub command: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecurringMissionSmokeConfig {
    pub enabled: bool,
    pub first_node: String,
    pub second_node: String,
    pub first_task_note: String,
    pub second_task_note: String,
    pub namespace_prefix: String,
    pub interval_seconds: u64,
    pub execute: bool,
    pub updated_at_epoch_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RecurringMissionSmokeState {
    #[serde(default)]
    pub last_run_epoch_ms: Option<u128>,
    #[serde(default)]
    pub last_status: Option<String>,
    #[serde(default)]
    pub last_mission_id: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub run_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecurringMissionSmokeStatus {
    pub configured: bool,
    pub due: bool,
    pub next_run_epoch_ms: Option<u128>,
    pub config: Option<RecurringMissionSmokeConfig>,
    pub state: RecurringMissionSmokeState,
}

#[derive(Debug, Clone)]
pub struct RecurringMissionSmokeConfigureOptions {
    pub first_node: String,
    pub second_node: String,
    pub first_task_note: Option<PathBuf>,
    pub second_task_note: Option<PathBuf>,
    pub namespace_prefix: Option<String>,
    pub interval_seconds: u64,
    pub execute: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomyReconcileAction {
    pub kind: String,
    pub status: String,
    pub summary: String,
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomyReconcileReport {
    pub status: String,
    pub dry_run: bool,
    pub pending_operator_requests: usize,
    pub pending_proposals: usize,
    pub capability_count: usize,
    pub safe_actions: Vec<AutonomyReconcileAction>,
    pub blocked_actions: Vec<AutonomyReconcileAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub smoke_reports: Vec<MissionSmokeReport>,
    pub notifications_sent: usize,
    pub expired_requests: Vec<String>,
}

pub fn list_capabilities() -> anyhow::Result<Vec<CapabilityRecord>> {
    let mut records = builtin_capabilities();
    let dir = capabilities_dir()?;
    if dir.exists() {
        for entry in
            fs::read_dir(&dir).with_context(|| format!("failed to read '{}'", dir.display()))?
        {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("failed to read '{}'", path.display()))?;
            let record: CapabilityRecord = serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse '{}'", path.display()))?;
            records.retain(|existing| existing.id != record.id);
            records.push(record);
        }
    }
    records.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(records)
}

pub fn show_capability(id: &str) -> anyhow::Result<CapabilityRecord> {
    list_capabilities()?
        .into_iter()
        .find(|record| record.id == id)
        .with_context(|| format!("capability '{id}' was not found"))
}

pub fn register_capability(options: CapabilityRegisterOptions) -> anyhow::Result<CapabilityRecord> {
    let now = now_epoch_ms();
    let id = clean_required(&options.id, "capability id")?;
    let title = clean_required(&options.title, "capability title")?;
    let lane = clean_required(&options.lane, "capability lane")?;
    let record = CapabilityRecord {
        id,
        title,
        lane,
        status: options.status.unwrap_or_else(|| "candidate".to_string()),
        confidence: options.confidence.unwrap_or(50).min(100),
        schedulable: options.schedulable,
        description: clean_required(&options.description, "capability description")?,
        validators: Vec::new(),
        artifact_contracts: Vec::new(),
        evidence: clean_values(options.evidence),
        gaps: clean_values(options.gaps),
        created_at_epoch_ms: now,
        updated_at_epoch_ms: now,
    };
    write_capability(&record)?;
    Ok(record)
}

pub fn validate_capability(id: &str) -> anyhow::Result<CapabilityValidationReport> {
    let capability = show_capability(id)?;
    let mut results = Vec::new();
    for validator in &capability.validators {
        results.push(run_validator(validator));
    }
    let passed = results
        .iter()
        .filter(|result| result.status == "passed")
        .count();
    let failed = results
        .iter()
        .filter(|result| result.status == "failed")
        .count();
    let skipped = results
        .iter()
        .filter(|result| result.status == "skipped")
        .count();
    let status = if failed > 0 {
        "failed"
    } else if passed == 0 {
        "skipped"
    } else if skipped > 0 {
        "partial"
    } else {
        "passed"
    }
    .to_string();
    Ok(CapabilityValidationReport {
        id: capability.id,
        status,
        passed,
        failed,
        skipped,
        results,
    })
}

pub fn validate_capabilities() -> anyhow::Result<Vec<CapabilityValidationReport>> {
    list_capabilities()?
        .into_iter()
        .map(|capability| validate_capability(&capability.id))
        .collect()
}

pub fn render_capabilities_output(
    records: &[CapabilityRecord],
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(records).context("failed to encode capabilities")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(records).context("failed to encode capabilities")
        }
        ControlPlaneOutput::Table => {
            let mut lines = vec!["ID\tLANE\tSTATUS\tCONFIDENCE\tSCHEDULABLE\tGAPS".to_string()];
            for record in records {
                lines.push(format!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    record.id,
                    record.lane,
                    record.status,
                    record.confidence,
                    record.schedulable,
                    record.gaps.join("; ")
                ));
            }
            Ok(lines.join("\n"))
        }
    }
}

pub fn render_capability_output(
    record: &CapabilityRecord,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Table => render_capabilities_output(&[record.clone()], output),
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(record).context("failed to encode capability")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(record).context("failed to encode capability")
        }
    }
}

pub fn render_capability_validation_output(
    reports: &[CapabilityValidationReport],
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(reports).context("failed to encode capability validation")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(reports).context("failed to encode capability validation")
        }
        ControlPlaneOutput::Table => {
            let mut lines = vec!["ID\tSTATUS\tPASSED\tFAILED\tSKIPPED\tDETAIL".to_string()];
            for report in reports {
                let detail = report
                    .results
                    .iter()
                    .map(|result| format!("{}={}", result.id, result.status))
                    .collect::<Vec<_>>()
                    .join("; ");
                lines.push(format!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    report.id, report.status, report.passed, report.failed, report.skipped, detail
                ));
            }
            Ok(lines.join("\n"))
        }
    }
}

pub fn run_two_node_mission_smoke(
    first_node: String,
    second_node: String,
    first_task_note: PathBuf,
    second_task_note: PathBuf,
    namespace_prefix: Option<String>,
    dry_run: bool,
    execute: bool,
    output_command: Vec<String>,
) -> anyhow::Result<MissionSmokeReport> {
    let prefix = namespace_prefix.unwrap_or_else(|| "jarvis-smoke".to_string());
    let mission = create_mission(MissionCreateOptions {
        title: format!("Two-node mission smoke {first_node} {second_node}"),
        template: Some("agent-relay-handoff".to_string()),
        objective: Some(
            "Validate two dedicated nodes can run cooperating Codex workloads and record evidence."
                .to_string(),
        ),
        priority: Some("high".to_string()),
        owner: Some("jarvisctl".to_string()),
        labels: BTreeMap::from([
            ("lane".to_string(), "codex-remote-session".to_string()),
            ("smoke".to_string(), "two-node".to_string()),
        ]),
        tickets: vec![first_task_note.clone(), second_task_note.clone()],
        namespaces: vec![
            format!("{prefix}-{first_node}"),
            format!("{prefix}-{second_node}"),
        ],
        nodes: vec![first_node.clone(), second_node.clone()],
    })?;
    append_mission_event(MissionEventOptions {
        mission_id: mission.id.clone(),
        stage: "sense".to_string(),
        status: "planned".to_string(),
        summary: "Created two-node unattended smoke mission ledger.".to_string(),
        ticket: None,
        namespace: None,
        node: None,
        visit: None,
        approval: None,
        evidence: vec![
            first_task_note.display().to_string(),
            second_task_note.display().to_string(),
        ],
    })?;
    let command = format!(
        "jarvisctl node pair-session --first-node {first_node} --second-node {second_node} --first-task-note {} --second-task-note {} --namespace-prefix {prefix}",
        first_task_note.display(),
        second_task_note.display()
    );
    let mut evidence = vec![command.clone()];
    let status = if execute && !dry_run {
        let result = start_node_pair_session(NodePairSessionOptions {
            first_node: first_node.clone(),
            second_node: second_node.clone(),
            first_task_note,
            second_task_note,
            first_namespace: None,
            second_namespace: None,
            namespace_prefix: Some(prefix),
            message: Some("Two-node mission smoke: coordinate, exchange one status message, and record node-local evidence.".to_string()),
            startup_delay_ms: 1500,
            retries: 1,
            command: output_command,
        })?;
        evidence.push(serde_json::to_string(&result).context("failed to encode pair result")?);
        append_mission_event(MissionEventOptions {
            mission_id: mission.id.clone(),
            stage: "task".to_string(),
            status: "started".to_string(),
            summary: "Started paired node session smoke.".to_string(),
            ticket: None,
            namespace: None,
            node: None,
            visit: None,
            approval: None,
            evidence: evidence.clone(),
        })?;
        "started".to_string()
    } else {
        append_mission_event(MissionEventOptions {
            mission_id: mission.id.clone(),
            stage: "task".to_string(),
            status: "dry-run".to_string(),
            summary: "Dry-run smoke recorded without launching remote sessions.".to_string(),
            ticket: None,
            namespace: None,
            node: None,
            visit: None,
            approval: None,
            evidence: evidence.clone(),
        })?;
        complete_mission(
            &mission.id,
            "completed",
            "Dry-run two-node mission smoke command rendered and evidence recorded.",
            evidence.clone(),
        )?;
        "dry-run".to_string()
    };
    Ok(MissionSmokeReport {
        id: format!("two-node-smoke-{}", now_epoch_ms()),
        status,
        dry_run: dry_run || !execute,
        first_node,
        second_node,
        mission_id: mission.id,
        command,
        evidence,
    })
}

pub fn configure_recurring_mission_smoke(
    options: RecurringMissionSmokeConfigureOptions,
) -> anyhow::Result<RecurringMissionSmokeStatus> {
    let first_node = clean_required(&options.first_node, "first node")?;
    let second_node = clean_required(&options.second_node, "second node")?;
    let first_task_note = options.first_task_note.unwrap_or(default_smoke_ticket_path(
        &first_node,
        &second_node,
        "first",
    )?);
    let second_task_note = options
        .second_task_note
        .unwrap_or(default_smoke_ticket_path(
            &first_node,
            &second_node,
            "second",
        )?);
    ensure_smoke_ticket(
        &first_task_note,
        &first_node,
        &second_node,
        "First node should report local readiness, exchange one concise partner message, and record evidence.",
    )?;
    ensure_smoke_ticket(
        &second_task_note,
        &second_node,
        &first_node,
        "Second node should report local readiness, exchange one concise partner message, and record evidence.",
    )?;
    let config = RecurringMissionSmokeConfig {
        enabled: options.enabled,
        first_node,
        second_node,
        first_task_note: first_task_note.display().to_string(),
        second_task_note: second_task_note.display().to_string(),
        namespace_prefix: options
            .namespace_prefix
            .unwrap_or_else(|| "jarvis-smoke".to_string()),
        interval_seconds: options.interval_seconds.max(3600),
        execute: options.execute,
        updated_at_epoch_ms: now_epoch_ms(),
    };
    write_recurring_smoke_config(&config)?;
    recurring_mission_smoke_status()
}

pub fn recurring_mission_smoke_status() -> anyhow::Result<RecurringMissionSmokeStatus> {
    let config = load_recurring_smoke_config()?;
    let state = load_recurring_smoke_state()?;
    let now = now_epoch_ms();
    let (due, next_run_epoch_ms) =
        if let Some(config) = config.as_ref().filter(|config| config.enabled) {
            let interval_ms = u128::from(config.interval_seconds) * 1000;
            let next = state
                .last_run_epoch_ms
                .map(|last| last.saturating_add(interval_ms))
                .unwrap_or(now);
            (next <= now, Some(next))
        } else {
            (false, None)
        };
    Ok(RecurringMissionSmokeStatus {
        configured: config.is_some(),
        due,
        next_run_epoch_ms,
        config,
        state,
    })
}

pub fn run_recurring_mission_smoke(force: bool) -> anyhow::Result<Option<MissionSmokeReport>> {
    let status = recurring_mission_smoke_status()?;
    let Some(config) = status.config else {
        return Ok(None);
    };
    if !config.enabled || (!status.due && !force) {
        return Ok(None);
    }
    let mut state = status.state;
    match run_two_node_mission_smoke(
        config.first_node.clone(),
        config.second_node.clone(),
        PathBuf::from(&config.first_task_note),
        PathBuf::from(&config.second_task_note),
        Some(config.namespace_prefix.clone()),
        !config.execute,
        config.execute,
        Vec::new(),
    ) {
        Ok(report) => {
            state.last_run_epoch_ms = Some(now_epoch_ms());
            state.last_status = Some(report.status.clone());
            state.last_mission_id = Some(report.mission_id.clone());
            state.last_error = None;
            state.run_count = state.run_count.saturating_add(1);
            write_recurring_smoke_state(&state)?;
            Ok(Some(report))
        }
        Err(error) => {
            state.last_run_epoch_ms = Some(now_epoch_ms());
            state.last_status = Some("failed".to_string());
            state.last_error = Some(error.to_string());
            state.run_count = state.run_count.saturating_add(1);
            write_recurring_smoke_state(&state)?;
            Err(error)
        }
    }
}

pub fn run_due_recurring_mission_smoke(
    dry_run: bool,
) -> anyhow::Result<Option<MissionSmokeReport>> {
    let status = recurring_mission_smoke_status()?;
    let Some(config) = status.config.as_ref() else {
        return Ok(None);
    };
    if !config.enabled || !status.due {
        return Ok(None);
    }
    if dry_run {
        return Ok(Some(MissionSmokeReport {
            id: format!("two-node-smoke-planned-{}", now_epoch_ms()),
            status: "planned".to_string(),
            dry_run: true,
            first_node: config.first_node.clone(),
            second_node: config.second_node.clone(),
            mission_id: "-".to_string(),
            command: format!("jarvisctl mission smoke-run --force --output table"),
            evidence: vec!["recurring smoke is due".to_string()],
        }));
    }
    run_recurring_mission_smoke(false)
}

pub fn render_recurring_mission_smoke_status(
    status: &RecurringMissionSmokeStatus,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(status).context("failed to encode mission smoke status")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(status).context("failed to encode mission smoke status")
        }
        ControlPlaneOutput::Table => Ok(format!(
            "CONFIGURED\tENABLED\tDUE\tNEXT_RUN\tLAST_STATUS\tLAST_MISSION\tRUNS\n{}\t{}\t{}\t{}\t{}\t{}\t{}",
            status.configured,
            status
                .config
                .as_ref()
                .map(|config| config.enabled)
                .unwrap_or(false),
            status.due,
            status
                .next_run_epoch_ms
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string()),
            status.state.last_status.as_deref().unwrap_or("-"),
            status.state.last_mission_id.as_deref().unwrap_or("-"),
            status.state.run_count
        )),
    }
}

pub fn render_mission_smoke_output(
    report: &MissionSmokeReport,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(report).context("failed to encode mission smoke report")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(report).context("failed to encode mission smoke report")
        }
        ControlPlaneOutput::Table => Ok(format!(
            "ID\tSTATUS\tDRY_RUN\tMISSION\tFIRST\tSECOND\tCOMMAND\n{}\t{}\t{}\t{}\t{}\t{}\t{}",
            report.id,
            report.status,
            report.dry_run,
            report.mission_id,
            report.first_node,
            report.second_node,
            report.command
        )),
    }
}

pub fn reconcile_autonomy(
    missions: &[MissionRecord],
    proposals: &[ProposalRecord],
    notify: bool,
    dry_run: bool,
) -> anyhow::Result<AutonomyReconcileReport> {
    let now = now_epoch_ms();
    let pending_requests = list_operator_requests()?
        .into_iter()
        .filter(|record| record.status == "pending")
        .collect::<Vec<_>>();
    let mut expired_requests = Vec::new();
    let mut safe_actions = Vec::new();
    for request in &pending_requests {
        if request.expires_at_epoch_ms <= now {
            if !dry_run {
                expire_operator_request(&request.id, "Expired by autonomy reconciler")?;
            }
            expired_requests.push(request.id.clone());
            safe_actions.push(AutonomyReconcileAction {
                kind: "expire-request".to_string(),
                status: if dry_run { "planned" } else { "completed" }.to_string(),
                summary: format!("Expired stale operator request '{}'.", request.title),
                command: Some(format!("jarvisctl operator-request show {}", request.id)),
            });
        }
    }
    let notify_report = if notify {
        let live_pending = pending_requests
            .iter()
            .filter(|request| request.expires_at_epoch_ms > now)
            .cloned()
            .collect::<Vec<_>>();
        let report = notify_operator_requests(&live_pending, true, dry_run)?;
        if report.delivered > 0 || dry_run {
            safe_actions.push(AutonomyReconcileAction {
                kind: "notify-operator".to_string(),
                status: if dry_run { "planned" } else { "completed" }.to_string(),
                summary: format!(
                    "Sent or planned {} operator desktop notification(s).",
                    report.delivered
                ),
                command: Some("jarvisctl operator-request notify --persistent".to_string()),
            });
        }
        Some(report)
    } else {
        None
    };
    let capabilities = list_capabilities()?;
    let validation = validate_capabilities()?;
    let failed_validation = validation
        .iter()
        .filter(|report| report.status != "passed")
        .count();
    safe_actions.push(AutonomyReconcileAction {
        kind: "capability-validate".to_string(),
        status: if failed_validation == 0 {
            "completed"
        } else {
            "attention"
        }
        .to_string(),
        summary: format!(
            "Validated {} capability lane(s); {} need attention.",
            capabilities.len(),
            failed_validation
        ),
        command: Some("jarvisctl capability validate".to_string()),
    });
    if missions.iter().any(|mission| mission.status == "planned") {
        safe_actions.push(AutonomyReconcileAction {
            kind: "mission-plan".to_string(),
            status: "ready".to_string(),
            summary: "Planned missions are ready for scheduler selection.".to_string(),
            command: Some("jarvisctl mission plan".to_string()),
        });
    }
    let mut smoke_reports = Vec::new();
    if let Some(report) = run_due_recurring_mission_smoke(dry_run)? {
        safe_actions.push(AutonomyReconcileAction {
            kind: "mission-smoke".to_string(),
            status: report.status.clone(),
            summary: format!(
                "Recurring two-node mission smoke {} for {} and {}.",
                report.status, report.first_node, report.second_node
            ),
            command: Some("jarvisctl mission smoke-run --force".to_string()),
        });
        smoke_reports.push(report);
    }
    let pending_proposals = proposals
        .iter()
        .filter(|proposal| proposal.status == "pending")
        .count();
    let mut blocked_actions = Vec::new();
    for request in pending_requests
        .iter()
        .filter(|request| request.expires_at_epoch_ms > now)
    {
        blocked_actions.push(AutonomyReconcileAction {
            kind: "operator-request".to_string(),
            status: "blocked".to_string(),
            summary: format!(
                "{} requires operator decision: {}",
                request.title, request.reason
            ),
            command: Some(format!(
                "jarvisctl operator-request resolve {} --status approved --decision '<reason>'",
                request.id
            )),
        });
    }
    for proposal in proposals
        .iter()
        .filter(|proposal| proposal.status == "pending")
    {
        blocked_actions.push(AutonomyReconcileAction {
            kind: "proposal".to_string(),
            status: "blocked".to_string(),
            summary: format!("{} requires proposal decision.", proposal.title),
            command: Some(format!(
                "jarvisctl proposal decide {} --status approved --decision '<reason>'",
                proposal.id
            )),
        });
    }
    let status = if blocked_actions.is_empty() {
        "clear"
    } else {
        "blocked"
    }
    .to_string();
    Ok(AutonomyReconcileReport {
        status,
        dry_run,
        pending_operator_requests: pending_requests.len(),
        pending_proposals,
        capability_count: capabilities.len(),
        safe_actions,
        blocked_actions,
        smoke_reports,
        notifications_sent: notify_report.map(|report| report.delivered).unwrap_or(0),
        expired_requests,
    })
}

pub fn render_autonomy_reconcile_output(
    report: &AutonomyReconcileReport,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => serde_json::to_string_pretty(report)
            .context("failed to encode autonomy reconcile report"),
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(report).context("failed to encode autonomy reconcile report")
        }
        ControlPlaneOutput::Table => {
            let mut lines = vec!["STATUS\tPENDING_REQUESTS\tPENDING_PROPOSALS\tCAPABILITIES\tSAFE\tBLOCKED\tNOTIFIED".to_string()];
            lines.push(format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                report.status,
                report.pending_operator_requests,
                report.pending_proposals,
                report.capability_count,
                report.safe_actions.len(),
                report.blocked_actions.len(),
                report.notifications_sent
            ));
            for action in &report.blocked_actions {
                lines.push(format!(
                    "BLOCKED\t{}\t{}\t-\t-\t-\t{}",
                    action.kind, action.status, action.summary
                ));
            }
            Ok(lines.join("\n"))
        }
    }
}

fn builtin_capabilities() -> Vec<CapabilityRecord> {
    let now = now_epoch_ms();
    vec![
        CapabilityRecord {
            id: "codex-remote-session".to_string(),
            title: "Codex remote session".to_string(),
            lane: "cross-node-agent-runtime".to_string(),
            status: "usable".to_string(),
            confidence: 82,
            schedulable: true,
            description: "Start, resume, steer, and close Codex app-server sessions across registered nodes.".to_string(),
            validators: vec![
                command_validator("jarvisctl-version", "jarvisctl --version", true),
                command_validator("node-index", "jarvisctl node index --output json", true),
            ],
            artifact_contracts: vec![artifact_contract(
                "runtime-event-log",
                "~/.jarvis/codex/sessions/<namespace>/events.jsonl",
                true,
                "Every session must expose an event log or transcript path for dashboard evidence.",
            )],
            evidence: vec![
                "node pair-session can inject partner context into two Codex sessions".to_string(),
                "cluster index reports local and remote sessions".to_string(),
            ],
            gaps: vec!["run recurring two-node mission smoke from cron or systemd".to_string()],
            created_at_epoch_ms: now,
            updated_at_epoch_ms: now,
        },
        CapabilityRecord {
            id: "bounded-worker-offload".to_string(),
            title: "Bounded worker offload".to_string(),
            lane: "typed-worker-lane".to_string(),
            status: "candidate".to_string(),
            confidence: 70,
            schedulable: true,
            description: "Route narrow tasks into worker lanes with validators and explicit artifact contracts.".to_string(),
            validators: vec![
                command_validator(
                    "worker-lane-validate",
                    "jarvisctl worker validate --output json",
                    false,
                ),
                command_validator(
                    "worker-model-availability",
                    "jarvisctl worker validate-models --all --output json",
                    true,
                ),
            ],
            artifact_contracts: vec![artifact_contract(
                "validated-artifact",
                "ticket-defined output path",
                true,
                "Worker output must be schema/test checked before being promoted into the mission ledger.",
            )],
            evidence: vec!["dashboard can start bounded offload from a namespace".to_string()],
            gaps: vec!["collect pass/fail rates per worker lane".to_string()],
            created_at_epoch_ms: now,
            updated_at_epoch_ms: now,
        },
        CapabilityRecord {
            id: "operator-proposal-gate".to_string(),
            title: "Operator proposal gate".to_string(),
            lane: "governed-autonomy".to_string(),
            status: "usable".to_string(),
            confidence: 78,
            schedulable: true,
            description: "Pause high-risk work behind proposals and durable operator requests with dashboard resolution.".to_string(),
            validators: vec![command_validator(
                "operator-request-list",
                "jarvisctl operator-request list --all --output json",
                true,
            )],
            artifact_contracts: vec![artifact_contract(
                "decision-record",
                "~/.jarvis/codex/operator-requests/<id>.json",
                true,
                "Every approval/denial must preserve reason, requester, status, and response.",
            )],
            evidence: vec![
                "app-server requests mirror into durable operator queue".to_string(),
                "dashboard approve/deny controls resolve linked runtime requests".to_string(),
            ],
            gaps: vec!["add recurring approve/reject smoke through dashboard automation".to_string()],
            created_at_epoch_ms: now,
            updated_at_epoch_ms: now,
        },
    ]
}

fn run_validator(validator: &CapabilityValidator) -> CapabilityValidatorResult {
    match validator.kind.as_str() {
        "command" => {
            let words = match shell_words::split(&validator.target) {
                Ok(words) if !words.is_empty() => words,
                Ok(_) | Err(_) => {
                    return CapabilityValidatorResult {
                        id: validator.id.clone(),
                        kind: validator.kind.clone(),
                        target: validator.target.clone(),
                        status: if validator.required {
                            "failed"
                        } else {
                            "skipped"
                        }
                        .to_string(),
                        detail: "validator command could not be parsed".to_string(),
                    };
                }
            };
            let mut command = Command::new(&words[0]);
            command.args(&words[1..]);
            match command.output() {
                Ok(output) if output.status.success() => CapabilityValidatorResult {
                    id: validator.id.clone(),
                    kind: validator.kind.clone(),
                    target: validator.target.clone(),
                    status: "passed".to_string(),
                    detail: "command exited successfully".to_string(),
                },
                Ok(output) => CapabilityValidatorResult {
                    id: validator.id.clone(),
                    kind: validator.kind.clone(),
                    target: validator.target.clone(),
                    status: if validator.required {
                        "failed"
                    } else {
                        "skipped"
                    }
                    .to_string(),
                    detail: format!("command exited with {}", output.status),
                },
                Err(error) => CapabilityValidatorResult {
                    id: validator.id.clone(),
                    kind: validator.kind.clone(),
                    target: validator.target.clone(),
                    status: if validator.required {
                        "failed"
                    } else {
                        "skipped"
                    }
                    .to_string(),
                    detail: error.to_string(),
                },
            }
        }
        "path" => {
            let path = expand_home(&validator.target);
            let exists = Path::new(&path).exists();
            CapabilityValidatorResult {
                id: validator.id.clone(),
                kind: validator.kind.clone(),
                target: validator.target.clone(),
                status: if exists {
                    "passed"
                } else if validator.required {
                    "failed"
                } else {
                    "skipped"
                }
                .to_string(),
                detail: if exists {
                    "path exists".to_string()
                } else {
                    "path missing".to_string()
                },
            }
        }
        other => CapabilityValidatorResult {
            id: validator.id.clone(),
            kind: validator.kind.clone(),
            target: validator.target.clone(),
            status: if validator.required {
                "failed"
            } else {
                "skipped"
            }
            .to_string(),
            detail: format!("unknown validator kind '{other}'"),
        },
    }
}

fn command_validator(id: &str, target: &str, required: bool) -> CapabilityValidator {
    CapabilityValidator {
        id: id.to_string(),
        kind: "command".to_string(),
        target: target.to_string(),
        required,
    }
}

fn artifact_contract(
    id: &str,
    path_hint: &str,
    required: bool,
    description: &str,
) -> CapabilityArtifactContract {
    CapabilityArtifactContract {
        id: id.to_string(),
        path_hint: path_hint.to_string(),
        required,
        description: description.to_string(),
    }
}

fn write_capability(record: &CapabilityRecord) -> anyhow::Result<()> {
    let dir = capabilities_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("failed to create '{}'", dir.display()))?;
    let path = dir.join(format!("{}.json", record.id));
    let tmp = dir.join(format!("{}.json.tmp", record.id));
    let raw = serde_json::to_string_pretty(record).context("failed to encode capability")?;
    fs::write(&tmp, raw).with_context(|| format!("failed to write '{}'", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("failed to move '{}' to '{}'", tmp.display(), path.display()))
}

fn load_recurring_smoke_config() -> anyhow::Result<Option<RecurringMissionSmokeConfig>> {
    let path = recurring_smoke_config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    let config = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse '{}'", path.display()))?;
    Ok(Some(config))
}

fn write_recurring_smoke_config(config: &RecurringMissionSmokeConfig) -> anyhow::Result<()> {
    let path = recurring_smoke_config_path()?;
    write_json_file(&path, config)
}

fn load_recurring_smoke_state() -> anyhow::Result<RecurringMissionSmokeState> {
    let path = recurring_smoke_state_path()?;
    if !path.exists() {
        return Ok(RecurringMissionSmokeState::default());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse '{}'", path.display()))
}

fn write_recurring_smoke_state(state: &RecurringMissionSmokeState) -> anyhow::Result<()> {
    let path = recurring_smoke_state_path()?;
    write_json_file(&path, state)
}

fn recurring_smoke_config_path() -> anyhow::Result<PathBuf> {
    Ok(jarvis_codex_dir()?
        .join("mission-smoke")
        .join("config.json"))
}

fn recurring_smoke_state_path() -> anyhow::Result<PathBuf> {
    Ok(jarvis_codex_dir()?.join("mission-smoke").join("state.json"))
}

fn default_smoke_ticket_path(
    first_node: &str,
    second_node: &str,
    side: &str,
) -> anyhow::Result<PathBuf> {
    Ok(jarvis_codex_dir()?
        .join("mission-smoke")
        .join(format!("two-node-{first_node}-{second_node}-{side}.md")))
}

fn ensure_smoke_ticket(
    path: &Path,
    node: &str,
    partner: &str,
    objective: &str,
) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    let body = format!(
        "---\ntype: ticket\nstatus: ready_for_codex\npriority: medium\nowner: jarvisctl\nlabels:\n  - smoke\n  - two-node\n---\n\n# Two-node smoke: {node}\n\n## Objective\n{objective}\n\n## Protocol\n- Work only from node `{node}`.\n- Coordinate with partner node `{partner}` using the paired session protocol.\n- Report local Jarvis/Codex readiness, timer state, and one concise partner message.\n- Record final evidence in the mission ledger or ticket outcome.\n"
    );
    fs::write(path, body).with_context(|| format!("failed to write '{}'", path.display()))
}

fn write_json_file<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let raw = serde_json::to_string_pretty(value).context("failed to encode JSON")?;
    fs::write(&tmp, raw).with_context(|| format!("failed to write '{}'", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to move '{}' to '{}'", tmp.display(), path.display()))
}

fn capabilities_dir() -> anyhow::Result<PathBuf> {
    Ok(jarvis_codex_dir()?.join("capabilities"))
}

fn jarvis_codex_dir() -> anyhow::Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".jarvis").join("codex"))
}

fn clean_required(value: &str, label: &str) -> anyhow::Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{label} must not be empty");
    }
    Ok(trimmed.to_string())
}

fn clean_values(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

fn expand_home(value: &str) -> String {
    if let Some(stripped) = value.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return format!("{home}/{stripped}");
        }
    }
    value.to_string()
}

fn now_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[allow(dead_code)]
fn _keep_operator_request_type(_: &OperatorRequestRecord) {}
