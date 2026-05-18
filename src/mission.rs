use crate::control_plane::ControlPlaneOutput;
use anyhow::{Context, anyhow, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MissionRecord {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tickets: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub visits: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    pub created_at_epoch_ms: i64,
    pub updated_at_epoch_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionEvent {
    pub id: String,
    pub mission_id: String,
    pub stage: String,
    pub status: String,
    pub summary: String,
    pub timestamp_epoch_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionDetail {
    pub mission: MissionRecord,
    pub events: Vec<MissionEvent>,
}

#[derive(Debug, Clone)]
pub struct MissionCreateOptions {
    pub title: String,
    pub objective: Option<String>,
    pub priority: Option<String>,
    pub owner: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub tickets: Vec<PathBuf>,
    pub namespaces: Vec<String>,
    pub nodes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct MissionEventOptions {
    pub mission_id: String,
    pub stage: String,
    pub status: String,
    pub summary: String,
    pub ticket: Option<PathBuf>,
    pub namespace: Option<String>,
    pub node: Option<String>,
    pub visit: Option<String>,
    pub approval: Option<String>,
    pub evidence: Vec<String>,
}

pub fn create_mission(options: MissionCreateOptions) -> anyhow::Result<MissionRecord> {
    let title = options.title.trim();
    if title.is_empty() {
        bail!("mission title must not be empty");
    }
    let now = now_epoch_ms();
    let mission = MissionRecord {
        id: format!("{}-{}", slugify(title), now),
        title: title.to_string(),
        objective: clean_optional(options.objective),
        status: "planned".to_string(),
        priority: clean_optional(options.priority),
        owner: clean_optional(options.owner),
        labels: options.labels,
        tickets: normalize_paths(options.tickets),
        namespaces: normalize_values(options.namespaces),
        nodes: normalize_values(options.nodes),
        visits: Vec::new(),
        approvals: Vec::new(),
        evidence: Vec::new(),
        outcome: None,
        created_at_epoch_ms: now,
        updated_at_epoch_ms: now,
    };
    save_mission(&mission)?;
    Ok(mission)
}

pub fn list_missions() -> anyhow::Result<Vec<MissionRecord>> {
    let dir = missions_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut missions = Vec::new();
    for entry in
        fs::read_dir(&dir).with_context(|| format!("failed to read '{}'", dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        missions.push(load_mission_from_path(&path)?);
    }
    missions.sort_by(|left, right| right.updated_at_epoch_ms.cmp(&left.updated_at_epoch_ms));
    Ok(missions)
}

pub fn show_mission(id: &str) -> anyhow::Result<MissionDetail> {
    Ok(MissionDetail {
        mission: load_mission(id)?,
        events: read_mission_events(id)?,
    })
}

pub fn append_mission_event(options: MissionEventOptions) -> anyhow::Result<MissionDetail> {
    let mut mission = load_mission(&options.mission_id)?;
    let now = now_epoch_ms();
    let event = MissionEvent {
        id: format!("{}-{}", slugify(&options.stage), now),
        mission_id: mission.id.clone(),
        stage: required_clean("stage", &options.stage)?,
        status: required_clean("status", &options.status)?,
        summary: required_clean("summary", &options.summary)?,
        timestamp_epoch_ms: now,
        ticket: options.ticket.map(normalize_path),
        namespace: clean_optional(options.namespace),
        node: clean_optional(options.node),
        visit: clean_optional(options.visit),
        approval: clean_optional(options.approval),
        evidence: normalize_values(options.evidence),
    };
    apply_event_links(&mut mission, &event);
    mission.updated_at_epoch_ms = now;
    save_mission(&mission)?;
    append_event(&event)?;
    show_mission(&mission.id)
}

pub fn complete_mission(
    id: &str,
    status: &str,
    outcome: &str,
    evidence: Vec<String>,
) -> anyhow::Result<MissionDetail> {
    let mut mission = load_mission(id)?;
    let now = now_epoch_ms();
    mission.status = required_clean("status", status)?;
    mission.outcome = Some(required_clean("outcome", outcome)?);
    push_unique_many(&mut mission.evidence, normalize_values(evidence));
    mission.updated_at_epoch_ms = now;
    save_mission(&mission)?;
    let event = MissionEvent {
        id: format!("complete-{}", now),
        mission_id: mission.id.clone(),
        stage: "verify".to_string(),
        status: mission.status.clone(),
        summary: mission.outcome.clone().unwrap_or_default(),
        timestamp_epoch_ms: now,
        ticket: None,
        namespace: None,
        node: None,
        visit: None,
        approval: None,
        evidence: mission.evidence.clone(),
    };
    append_event(&event)?;
    show_mission(&mission.id)
}

pub fn render_missions_output(
    missions: &[MissionRecord],
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(missions).context("failed to encode missions")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(missions).context("failed to encode missions")
        }
        ControlPlaneOutput::Table => Ok(render_missions_table(missions)),
    }
}

pub fn render_mission_detail_output(
    detail: &MissionDetail,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(detail).context("failed to encode mission")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(detail).context("failed to encode mission")
        }
        ControlPlaneOutput::Table => Ok(render_mission_detail_table(detail)),
    }
}

fn render_missions_table(missions: &[MissionRecord]) -> String {
    let mut lines = vec!["ID\tSTATUS\tPRIORITY\tTITLE\tUPDATED\tLINKS".to_string()];
    for mission in missions {
        lines.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            mission.id,
            mission.status,
            mission.priority.as_deref().unwrap_or("-"),
            mission.title,
            mission.updated_at_epoch_ms,
            mission_link_count(mission)
        ));
    }
    lines.join("\n")
}

fn render_mission_detail_table(detail: &MissionDetail) -> String {
    let mission = &detail.mission;
    let mut lines = vec![
        "FIELD\tVALUE".to_string(),
        format!("id\t{}", mission.id),
        format!("title\t{}", mission.title),
        format!("status\t{}", mission.status),
        format!("priority\t{}", mission.priority.as_deref().unwrap_or("-")),
        format!("owner\t{}", mission.owner.as_deref().unwrap_or("-")),
        format!("objective\t{}", mission.objective.as_deref().unwrap_or("-")),
        format!("tickets\t{}", mission.tickets.join(", ")),
        format!("namespaces\t{}", mission.namespaces.join(", ")),
        format!("nodes\t{}", mission.nodes.join(", ")),
        format!("evidence\t{}", mission.evidence.join(", ")),
        format!("outcome\t{}", mission.outcome.as_deref().unwrap_or("-")),
        "".to_string(),
        "EVENT\tSTAGE\tSTATUS\tSUMMARY".to_string(),
    ];
    for event in &detail.events {
        lines.push(format!(
            "{}\t{}\t{}\t{}",
            event.id, event.stage, event.status, event.summary
        ));
    }
    lines.join("\n")
}

fn mission_link_count(mission: &MissionRecord) -> usize {
    mission.tickets.len()
        + mission.namespaces.len()
        + mission.nodes.len()
        + mission.visits.len()
        + mission.approvals.len()
        + mission.evidence.len()
}

fn load_mission(id: &str) -> anyhow::Result<MissionRecord> {
    let path = mission_path(id)?;
    load_mission_from_path(&path)
}

fn load_mission_from_path(path: &Path) -> anyhow::Result<MissionRecord> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse '{}'", path.display()))
}

fn save_mission(mission: &MissionRecord) -> anyhow::Result<()> {
    let path = mission_path(&mission.id)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    let raw = serde_json::to_string_pretty(mission).context("failed to encode mission")?;
    atomic_write_string(&path, &raw)
}

fn append_event(event: &MissionEvent) -> anyhow::Result<()> {
    let path = mission_events_path(&event.mission_id)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    let raw = serde_json::to_string(event).context("failed to encode mission event")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open '{}'", path.display()))?;
    writeln!(file, "{raw}").with_context(|| format!("failed to append '{}'", path.display()))
}

fn read_mission_events(id: &str) -> anyhow::Result<Vec<MissionEvent>> {
    let path = mission_events_path(id)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    let mut events = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        events.push(serde_json::from_str::<MissionEvent>(line).with_context(|| {
            format!(
                "failed to parse mission event line {} in '{}'",
                index + 1,
                path.display()
            )
        })?);
    }
    events.sort_by_key(|event| event.timestamp_epoch_ms);
    Ok(events)
}

fn apply_event_links(mission: &mut MissionRecord, event: &MissionEvent) {
    if let Some(ticket) = &event.ticket {
        push_unique(&mut mission.tickets, ticket.clone());
    }
    if let Some(namespace) = &event.namespace {
        push_unique(&mut mission.namespaces, namespace.clone());
    }
    if let Some(node) = &event.node {
        push_unique(&mut mission.nodes, node.clone());
    }
    if let Some(visit) = &event.visit {
        push_unique(&mut mission.visits, visit.clone());
    }
    if let Some(approval) = &event.approval {
        push_unique(&mut mission.approvals, approval.clone());
    }
    push_unique_many(&mut mission.evidence, event.evidence.clone());
    if event.status.eq_ignore_ascii_case("failed") || event.status.eq_ignore_ascii_case("blocked") {
        mission.status = event.status.clone();
    } else if mission.status == "planned" {
        mission.status = "active".to_string();
    }
}

fn mission_path(id: &str) -> anyhow::Result<PathBuf> {
    Ok(missions_dir()?.join(format!("{}.json", sanitize_id(id)?)))
}

fn mission_events_path(id: &str) -> anyhow::Result<PathBuf> {
    Ok(mission_events_dir()?.join(format!("{}.jsonl", sanitize_id(id)?)))
}

fn missions_dir() -> anyhow::Result<PathBuf> {
    Ok(jarvis_codex_dir()?.join("missions"))
}

fn mission_events_dir() -> anyhow::Result<PathBuf> {
    Ok(jarvis_codex_dir()?.join("mission-events"))
}

fn jarvis_codex_dir() -> anyhow::Result<PathBuf> {
    if let Some(path) = env::var_os("JARVIS_CODEX_DIR") {
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".jarvis").join("codex"))
}

fn atomic_write_string(path: &Path, raw: &str) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path '{}' has no parent", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create '{}'", parent.display()))?;
    let temp = parent.join(format!(
        ".{}.tmp.{}.{}",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("mission"),
        std::process::id(),
        now_epoch_ms()
    ));
    fs::write(&temp, raw).with_context(|| format!("failed to write '{}'", temp.display()))?;
    fs::rename(&temp, path).with_context(|| {
        format!(
            "failed to rename '{}' to '{}'",
            temp.display(),
            path.display()
        )
    })
}

fn sanitize_id(id: &str) -> anyhow::Result<String> {
    let value = id.trim();
    if value.is_empty()
        || value.contains('/')
        || value.contains('\\')
        || value == "."
        || value == ".."
    {
        bail!("invalid mission id '{}'", id);
    }
    Ok(value.to_string())
}

fn required_clean(field: &str, value: &str) -> anyhow::Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(value.to_string())
}

fn normalize_paths(paths: Vec<PathBuf>) -> Vec<String> {
    paths.into_iter().map(normalize_path).collect()
}

fn normalize_path(path: PathBuf) -> String {
    path.to_string_lossy().trim().to_string()
}

fn normalize_values(values: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    push_unique_many(
        &mut normalized,
        values.into_iter().filter_map(clean_value).collect(),
    );
    normalized
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn clean_value(value: String) -> Option<String> {
    let value = value.trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

fn push_unique_many(target: &mut Vec<String>, values: Vec<String>) {
    for value in values {
        push_unique(target, value);
    }
}

fn push_unique(target: &mut Vec<String>, value: String) {
    if !target.iter().any(|existing| existing == &value) {
        target.push(value);
    }
}

fn slugify(value: &str) -> String {
    let mut output = String::new();
    let mut last_dash = false;
    for ch in value.chars().map(|ch| ch.to_ascii_lowercase()) {
        if ch.is_ascii_alphanumeric() {
            output.push(ch);
            last_dash = false;
        } else if !last_dash {
            output.push('-');
            last_dash = true;
        }
    }
    output.trim_matches('-').to_string().if_empty("mission")
}

trait IfEmpty {
    fn if_empty(self, fallback: &str) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.to_string()
        } else {
            self
        }
    }
}

fn now_epoch_ms() -> i64 {
    Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempJarvisCodexGuard {
        original_dir: Option<std::ffi::OsString>,
        root: PathBuf,
    }

    impl TempJarvisCodexGuard {
        fn new(prefix: &str) -> Self {
            let root = env::temp_dir().join(format!("{}-{}", prefix, now_epoch_ms()));
            fs::create_dir_all(&root).unwrap();
            let original_dir = env::var_os("JARVIS_CODEX_DIR");
            unsafe {
                env::set_var("JARVIS_CODEX_DIR", &root);
            }
            Self { original_dir, root }
        }
    }

    impl Drop for TempJarvisCodexGuard {
        fn drop(&mut self) {
            match &self.original_dir {
                Some(path) => unsafe {
                    env::set_var("JARVIS_CODEX_DIR", path);
                },
                None => unsafe {
                    env::remove_var("JARVIS_CODEX_DIR");
                },
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn mission_lifecycle_records_links_and_events() {
        let _home = TempJarvisCodexGuard::new("jarvisctl-mission-ledger");
        let mission = create_mission(MissionCreateOptions {
            title: "CV triage automation".to_string(),
            objective: Some("Rank candidates for HR review.".to_string()),
            priority: Some("high".to_string()),
            owner: Some("ops".to_string()),
            labels: BTreeMap::from([("domain".to_string(), "hr".to_string())]),
            tickets: vec![PathBuf::from("/tmp/cv-triage.md")],
            namespaces: vec![],
            nodes: vec!["archiebald".to_string()],
        })
        .unwrap();

        let detail = append_mission_event(MissionEventOptions {
            mission_id: mission.id.clone(),
            stage: "task".to_string(),
            status: "running".to_string(),
            summary: "Started remote namespace.".to_string(),
            ticket: None,
            namespace: Some("cv-triage".to_string()),
            node: Some("archiebald".to_string()),
            visit: None,
            approval: None,
            evidence: vec!["transcript:/tmp/cv.jsonl".to_string()],
        })
        .unwrap();

        assert_eq!(detail.mission.status, "active");
        assert_eq!(detail.mission.namespaces, vec!["cv-triage"]);
        assert_eq!(detail.mission.nodes, vec!["archiebald"]);
        assert_eq!(detail.events.len(), 1);

        let completed =
            complete_mission(&mission.id, "completed", "Shortlist delivered.", Vec::new()).unwrap();
        assert_eq!(completed.mission.status, "completed");
        assert_eq!(
            completed.mission.outcome.as_deref(),
            Some("Shortlist delivered.")
        );
        assert_eq!(completed.events.len(), 2);
        assert_eq!(list_missions().unwrap().len(), 1);
    }
}
