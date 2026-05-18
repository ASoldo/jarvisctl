use crate::control_plane::ControlPlaneOutput;
use anyhow::{Context, anyhow, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalRecord {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<String>,
    pub status: String,
    pub action: String,
    pub rationale: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    pub created_at_epoch_ms: i64,
    pub updated_at_epoch_ms: i64,
}

#[derive(Debug, Clone)]
pub struct ProposalCreateOptions {
    pub title: String,
    pub mission_id: Option<String>,
    pub action: String,
    pub rationale: String,
    pub risk: Option<String>,
    pub proposed_by: Option<String>,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ProposalDecisionOptions {
    pub id: String,
    pub status: String,
    pub decision: String,
    pub decided_by: Option<String>,
}

pub fn create_proposal(options: ProposalCreateOptions) -> anyhow::Result<ProposalRecord> {
    let title = required_clean("title", &options.title)?;
    let now = now_epoch_ms();
    let proposal = ProposalRecord {
        id: format!("{}-{}", slugify(&title), now),
        title,
        mission_id: clean_optional(options.mission_id),
        status: "pending".to_string(),
        action: required_clean("action", &options.action)?,
        rationale: required_clean("rationale", &options.rationale)?,
        risk: clean_optional(options.risk),
        proposed_by: clean_optional(options.proposed_by),
        evidence: normalize_values(options.evidence),
        decision: None,
        decided_by: None,
        created_at_epoch_ms: now,
        updated_at_epoch_ms: now,
    };
    save_proposal(&proposal)?;
    Ok(proposal)
}

pub fn list_proposals() -> anyhow::Result<Vec<ProposalRecord>> {
    let dir = proposals_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut proposals = Vec::new();
    for entry in
        fs::read_dir(&dir).with_context(|| format!("failed to read '{}'", dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry.path().extension().and_then(|value| value.to_str()) == Some("json")
        {
            proposals.push(load_proposal_from_path(&entry.path())?);
        }
    }
    proposals.sort_by(|left, right| right.updated_at_epoch_ms.cmp(&left.updated_at_epoch_ms));
    Ok(proposals)
}

pub fn show_proposal(id: &str) -> anyhow::Result<ProposalRecord> {
    load_proposal(id)
}

pub fn decide_proposal(options: ProposalDecisionOptions) -> anyhow::Result<ProposalRecord> {
    let mut proposal = load_proposal(&options.id)?;
    proposal.status = normalize_decision_status(&options.status)?;
    proposal.decision = Some(required_clean("decision", &options.decision)?);
    proposal.decided_by = clean_optional(options.decided_by);
    proposal.updated_at_epoch_ms = now_epoch_ms();
    save_proposal(&proposal)?;
    Ok(proposal)
}

pub fn render_proposals_output(
    proposals: &[ProposalRecord],
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(proposals).context("failed to encode proposals")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(proposals).context("failed to encode proposals")
        }
        ControlPlaneOutput::Table => {
            let mut lines = vec!["ID\tSTATUS\tMISSION\tTITLE\tACTION".to_string()];
            for proposal in proposals {
                lines.push(format!(
                    "{}\t{}\t{}\t{}\t{}",
                    proposal.id,
                    proposal.status,
                    proposal.mission_id.as_deref().unwrap_or("-"),
                    proposal.title,
                    proposal.action.replace('\n', "\\n")
                ));
            }
            Ok(lines.join("\n"))
        }
    }
}

pub fn render_proposal_output(
    proposal: &ProposalRecord,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(proposal).context("failed to encode proposal")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(proposal).context("failed to encode proposal")
        }
        ControlPlaneOutput::Table => Ok([
            "FIELD\tVALUE".to_string(),
            format!("id\t{}", proposal.id),
            format!("title\t{}", proposal.title),
            format!("mission\t{}", proposal.mission_id.as_deref().unwrap_or("-")),
            format!("status\t{}", proposal.status),
            format!("action\t{}", proposal.action),
            format!("rationale\t{}", proposal.rationale),
            format!("risk\t{}", proposal.risk.as_deref().unwrap_or("-")),
            format!(
                "proposed_by\t{}",
                proposal.proposed_by.as_deref().unwrap_or("-")
            ),
            format!("evidence\t{}", proposal.evidence.join(", ")),
            format!("decision\t{}", proposal.decision.as_deref().unwrap_or("-")),
            format!(
                "decided_by\t{}",
                proposal.decided_by.as_deref().unwrap_or("-")
            ),
        ]
        .join("\n")),
    }
}

fn load_proposal(id: &str) -> anyhow::Result<ProposalRecord> {
    load_proposal_from_path(&proposal_path(id)?)
}

fn load_proposal_from_path(path: &Path) -> anyhow::Result<ProposalRecord> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse '{}'", path.display()))
}

fn save_proposal(proposal: &ProposalRecord) -> anyhow::Result<()> {
    let path = proposal_path(&proposal.id)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    let raw = serde_json::to_string_pretty(proposal).context("failed to encode proposal")?;
    atomic_write_string(&path, &raw)
}

fn proposal_path(id: &str) -> anyhow::Result<PathBuf> {
    Ok(proposals_dir()?.join(format!("{}.json", sanitize_id(id)?)))
}

fn proposals_dir() -> anyhow::Result<PathBuf> {
    Ok(jarvis_codex_dir()?.join("proposals"))
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
            .unwrap_or("proposal"),
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
        bail!("invalid proposal id '{}'", id);
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

fn normalize_decision_status(value: &str) -> anyhow::Result<String> {
    let value = required_clean("status", value)?.to_ascii_lowercase();
    match value.as_str() {
        "approved" | "rejected" | "superseded" => Ok(value),
        _ => bail!("status must be approved, rejected, or superseded"),
    }
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn normalize_values(values: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    for value in values {
        let value = value.trim().to_string();
        if !value.is_empty() && !normalized.iter().any(|existing| existing == &value) {
            normalized.push(value);
        }
    }
    normalized
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
    let output = output.trim_matches('-').to_string();
    if output.is_empty() {
        "proposal".to_string()
    } else {
        output
    }
}

fn now_epoch_ms() -> i64 {
    Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempJarvisCodexGuard;

    #[test]
    fn proposal_lifecycle_requires_human_decision() {
        let _home = TempJarvisCodexGuard::new("jarvisctl-proposals");
        let proposal = create_proposal(ProposalCreateOptions {
            title: "Apply shortlist".to_string(),
            mission_id: Some("mission-1".to_string()),
            action: "Move three candidates to HR review.".to_string(),
            rationale: "They match the required Rust and operations profile.".to_string(),
            risk: Some("false positive candidate fit".to_string()),
            proposed_by: Some("agent0".to_string()),
            evidence: vec!["report:/tmp/shortlist.md".to_string()],
        })
        .unwrap();
        assert_eq!(proposal.status, "pending");
        assert_eq!(list_proposals().unwrap().len(), 1);

        let decided = decide_proposal(ProposalDecisionOptions {
            id: proposal.id.clone(),
            status: "approved".to_string(),
            decision: "Operator approved.".to_string(),
            decided_by: Some("rootster".to_string()),
        })
        .unwrap();
        assert_eq!(decided.status, "approved");
        assert_eq!(decided.decision.as_deref(), Some("Operator approved."));
        assert_eq!(decided.decided_by.as_deref(), Some("rootster"));
    }
}
