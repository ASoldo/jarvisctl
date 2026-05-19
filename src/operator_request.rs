use crate::control_plane::ControlPlaneOutput;
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_TTL_SECONDS: u64 = 12 * 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorRequestRecord {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<bool>,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub severity: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    pub created_at_epoch_ms: u128,
    pub updated_at_epoch_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at_epoch_ms: Option<u128>,
    pub expires_at_epoch_ms: u128,
}

#[derive(Debug, Clone)]
pub struct OperatorRequestCreateOptions {
    pub title: String,
    pub kind: String,
    pub severity: String,
    pub reason: String,
    pub risk: Option<String>,
    pub requested_by: Option<String>,
    pub namespace: Option<String>,
    pub request_id: Option<String>,
    pub method: Option<String>,
    pub command: Option<String>,
    pub params: Option<Value>,
    pub ttl_seconds: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct OperatorRequestResolveOptions {
    pub status: String,
    pub response: Option<Value>,
    pub error: Option<String>,
    pub decided_by: Option<String>,
    pub decision: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorRequestNotifyReport {
    pub attempted: usize,
    pub delivered: usize,
    pub dry_run: bool,
    pub persistent: bool,
    pub failures: Vec<String>,
}

pub fn create_operator_request(
    options: OperatorRequestCreateOptions,
) -> anyhow::Result<OperatorRequestRecord> {
    let now = now_epoch_ms();
    let ttl = options.ttl_seconds.unwrap_or(DEFAULT_TTL_SECONDS).max(60);
    let id = format!("{}-{}", slugify(&options.title), now);
    let record = OperatorRequestRecord {
        id,
        source_node: None,
        remote: None,
        title: options.title,
        kind: clean_or(options.kind, "operator"),
        status: "pending".to_string(),
        severity: clean_or(options.severity, "medium"),
        reason: options.reason,
        risk: clean_optional(options.risk),
        requested_by: clean_optional(options.requested_by),
        namespace: clean_optional(options.namespace),
        request_id: clean_optional(options.request_id),
        method: clean_optional(options.method),
        command: clean_optional(options.command),
        params: options.params,
        response: None,
        error: None,
        decided_by: None,
        decision: None,
        created_at_epoch_ms: now,
        updated_at_epoch_ms: now,
        resolved_at_epoch_ms: None,
        expires_at_epoch_ms: now + u128::from(ttl) * 1000,
    };
    write_operator_request(&record)?;
    Ok(record)
}

pub fn upsert_server_operator_request(
    namespace: &str,
    request_id: &str,
    method: &str,
    params: Value,
) -> anyhow::Result<OperatorRequestRecord> {
    if let Some(existing) = list_operator_requests()?.into_iter().find(|record| {
        record.namespace.as_deref() == Some(namespace)
            && record.request_id.as_deref() == Some(request_id)
            && record.status == "pending"
    }) {
        return Ok(existing);
    }
    create_operator_request(OperatorRequestCreateOptions {
        title: format!("{namespace} requests {method}"),
        kind: classify_method(method),
        severity: if method.to_ascii_lowercase().contains("permission")
            || method.to_ascii_lowercase().contains("approval")
        {
            "high".to_string()
        } else {
            "medium".to_string()
        },
        reason: format!(
            "Headless Codex app-server requested '{method}' and is paused until an operator responds."
        ),
        risk: Some(
            "Approving may allow the agent to continue with elevated or sensitive action."
                .to_string(),
        ),
        requested_by: Some("codex-app-server".to_string()),
        namespace: Some(namespace.to_string()),
        request_id: Some(request_id.to_string()),
        method: Some(method.to_string()),
        command: None,
        params: Some(params),
        ttl_seconds: Some(DEFAULT_TTL_SECONDS),
    })
}

pub fn list_operator_requests() -> anyhow::Result<Vec<OperatorRequestRecord>> {
    let dir = operator_requests_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in
        fs::read_dir(&dir).with_context(|| format!("failed to read '{}'", dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        let record = parse_operator_request_record(&raw, &path)?;
        records.push(record);
    }
    records.sort_by(|left, right| right.created_at_epoch_ms.cmp(&left.created_at_epoch_ms));
    Ok(records)
}

pub fn show_operator_request(id: &str) -> anyhow::Result<OperatorRequestRecord> {
    let path = operator_request_path(id)?;
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    parse_operator_request_record(&raw, &path)
}

pub fn resolve_operator_request(
    id: &str,
    options: OperatorRequestResolveOptions,
) -> anyhow::Result<OperatorRequestRecord> {
    let mut record = show_operator_request(id)?;
    if record.status != "pending" {
        bail!("operator request '{}' is already {}", id, record.status);
    }
    let now = now_epoch_ms();
    record.status = clean_or(options.status, "resolved");
    record.response = options.response;
    record.error = clean_optional(options.error);
    record.decided_by = clean_optional(options.decided_by);
    record.decision = clean_optional(options.decision);
    record.resolved_at_epoch_ms = Some(now);
    record.updated_at_epoch_ms = now;
    write_operator_request(&record)?;
    Ok(record)
}

pub fn expire_operator_request(id: &str, reason: &str) -> anyhow::Result<OperatorRequestRecord> {
    resolve_operator_request(
        id,
        OperatorRequestResolveOptions {
            status: "expired".to_string(),
            response: None,
            error: Some(reason.to_string()),
            decided_by: Some("jarvisctl".to_string()),
            decision: Some(reason.to_string()),
        },
    )
}

pub fn notify_operator_requests(
    records: &[OperatorRequestRecord],
    persistent: bool,
    dry_run: bool,
) -> anyhow::Result<OperatorRequestNotifyReport> {
    let mut delivered = 0;
    let mut failures = Vec::new();
    for record in records {
        if record.status != "pending" {
            continue;
        }
        if dry_run {
            delivered += 1;
            continue;
        }
        let title = format!("Jarvis needs {}", record.kind);
        let mut body = format!("{}\n\nReason: {}", record.title, record.reason);
        if let Some(risk) = record.risk.as_deref() {
            body.push_str(&format!("\nRisk: {risk}"));
        }
        if let Some(command) = record.command.as_deref() {
            body.push_str(&format!("\nCommand: {command}"));
        }
        body.push_str(&format!(
            "\n\nApprove/deny in Obsidian or run: jarvisctl operator-request show {}",
            record.id
        ));
        let mut command = Command::new("notify-send");
        command
            .arg("--app-name=jarvisctl")
            .arg("--urgency")
            .arg(
                if record.severity == "high" || record.severity == "critical" {
                    "critical"
                } else {
                    "normal"
                },
            )
            .arg("--expire-time")
            .arg(if persistent { "0" } else { "120000" })
            .arg(title)
            .arg(body);
        match command.status() {
            Ok(status) if status.success() => delivered += 1,
            Ok(status) => failures.push(format!("{}: notify-send exited with {status}", record.id)),
            Err(error) => failures.push(format!("{}: {error}", record.id)),
        }
    }
    Ok(OperatorRequestNotifyReport {
        attempted: records
            .iter()
            .filter(|record| record.status == "pending")
            .count(),
        delivered,
        dry_run,
        persistent,
        failures,
    })
}

pub fn render_operator_request_notify_output(
    report: &OperatorRequestNotifyReport,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(report).context("failed to encode notify report")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(report).context("failed to encode notify report")
        }
        ControlPlaneOutput::Table => Ok(format!(
            "ATTEMPTED\tDELIVERED\tPERSISTENT\tDRY_RUN\tFAILURES\n{}\t{}\t{}\t{}\t{}",
            report.attempted,
            report.delivered,
            report.persistent,
            report.dry_run,
            report.failures.join("; ")
        )),
    }
}

pub fn render_operator_requests_output(
    records: &[OperatorRequestRecord],
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(records).context("failed to encode operator requests")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(records).context("failed to encode operator requests")
        }
        ControlPlaneOutput::Table => {
            let mut lines =
                vec!["ID\tSTATUS\tSEVERITY\tKIND\tTITLE\tNAMESPACE\tREQUEST".to_string()];
            for record in records {
                lines.push(format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    record.id,
                    record.status,
                    record.severity,
                    record.kind,
                    record.title,
                    record.namespace.as_deref().unwrap_or("-"),
                    record.request_id.as_deref().unwrap_or("-"),
                ));
            }
            Ok(lines.join("\n"))
        }
    }
}

pub fn render_operator_request_output(
    record: &OperatorRequestRecord,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(record).context("failed to encode operator request")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(record).context("failed to encode operator request")
        }
        ControlPlaneOutput::Table => render_operator_requests_output(&[record.clone()], output),
    }
}

fn write_operator_request(record: &OperatorRequestRecord) -> anyhow::Result<()> {
    let dir = operator_requests_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("failed to create '{}'", dir.display()))?;
    let path = dir.join(format!("{}.json", record.id));
    let tmp = dir.join(format!("{}.json.tmp", record.id));
    let raw = serde_json::to_string_pretty(record).context("failed to encode operator request")?;
    fs::write(&tmp, raw).with_context(|| format!("failed to write '{}'", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("failed to move '{}' to '{}'", tmp.display(), path.display()))
}

fn parse_operator_request_record(raw: &str, path: &Path) -> anyhow::Result<OperatorRequestRecord> {
    match serde_json::from_str(raw) {
        Ok(record) => Ok(record),
        Err(strict_error) => {
            let mut stream =
                serde_json::Deserializer::from_str(raw).into_iter::<OperatorRequestRecord>();
            match stream.next() {
                Some(Ok(record)) => Ok(record),
                Some(Err(stream_error)) => Err(stream_error)
                    .with_context(|| format!("failed to parse '{}'", path.display())),
                None => Err(strict_error)
                    .with_context(|| format!("failed to parse '{}'", path.display())),
            }
        }
    }
}

fn operator_request_path(id: &str) -> anyhow::Result<PathBuf> {
    Ok(operator_requests_dir()?.join(format!("{id}.json")))
}

fn operator_requests_dir() -> anyhow::Result<PathBuf> {
    Ok(jarvis_codex_dir()?.join("operator-requests"))
}

fn jarvis_codex_dir() -> anyhow::Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".jarvis").join("codex"))
}

fn now_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn clean_or(value: String, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn classify_method(method: &str) -> String {
    let lower = method.to_ascii_lowercase();
    if lower.contains("permission") || lower.contains("approval") {
        "permission".to_string()
    } else if lower.contains("elicitation") || lower.contains("input") {
        "input".to_string()
    } else if lower.contains("auth") || lower.contains("credential") {
        "credential".to_string()
    } else {
        "app-server".to_string()
    }
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "operator-request".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_operator_request_record, slugify};
    use std::path::Path;

    #[test]
    fn slugify_operator_request_title() {
        assert_eq!(slugify("Sudo: install package"), "sudo-install-package");
    }

    #[test]
    fn parse_operator_request_ignores_stale_trailing_json() {
        let raw = r#"{
  "id": "demo",
  "title": "Demo",
  "kind": "operator",
  "status": "approved",
  "severity": "medium",
  "reason": "test",
  "created_at_epoch_ms": 1,
  "updated_at_epoch_ms": 2,
  "expires_at_epoch_ms": 3
}ms": 2,
  "expires_at_epoch_ms": 3
}"#;

        let record = parse_operator_request_record(raw, Path::new("demo.json")).unwrap();

        assert_eq!(record.id, "demo");
        assert_eq!(record.status, "approved");
    }
}
