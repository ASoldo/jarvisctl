use crate::control_plane::ControlPlaneOutput;
use crate::mission::MissionRecord;
use crate::proposal::ProposalRecord;
use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomyPolicyRule {
    pub id: String,
    pub action_class: String,
    pub decision: String,
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionPlanAction {
    pub kind: String,
    pub stage: String,
    pub summary: String,
    pub command: Option<String>,
    pub requires_proposal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionAutonomyPlan {
    pub mission_id: String,
    pub title: String,
    pub status: String,
    pub next_stage: String,
    pub autonomy_level: String,
    pub pending_proposals: usize,
    pub risk: String,
    pub actions: Vec<MissionPlanAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerLaneScorecard {
    pub lane: String,
    pub readiness: String,
    pub confidence: u8,
    pub evidence: Vec<String>,
    pub gaps: Vec<String>,
}

pub fn default_autonomy_policy() -> Vec<AutonomyPolicyRule> {
    vec![
        AutonomyPolicyRule {
            id: "credentials".to_string(),
            action_class: "credentials, tokens, paid endpoints, durable auth".to_string(),
            decision: "proposal_required".to_string(),
            rationale: "Credential and billing boundaries must be explicit operator decisions.".to_string(),
        },
        AutonomyPolicyRule {
            id: "production-mutation".to_string(),
            action_class: "production changes, destructive commands, broad rewrites".to_string(),
            decision: "proposal_required".to_string(),
            rationale: "High-blast-radius mutations need a recorded decision and rollback evidence.".to_string(),
        },
        AutonomyPolicyRule {
            id: "bounded-offload".to_string(),
            action_class: "typed worker offload with validator and scoped artifact".to_string(),
            decision: "auto_allowed".to_string(),
            rationale: "Narrow, testable worker jobs can run when validation evidence is captured.".to_string(),
        },
        AutonomyPolicyRule {
            id: "cross-node-handoff".to_string(),
            action_class: "remote session start, visit, fanout, relay handoff".to_string(),
            decision: "auto_allowed_with_mission_event".to_string(),
            rationale: "Remote work is allowed when the mission ledger records node, namespace, and evidence.".to_string(),
        },
    ]
}

pub fn plan_missions(
    missions: &[MissionRecord],
    proposals: &[ProposalRecord],
    mission_id: Option<&str>,
) -> Vec<MissionAutonomyPlan> {
    missions
        .iter()
        .filter(|mission| {
            mission_id
                .map(|id| mission.id == id || mission.title.eq_ignore_ascii_case(id))
                .unwrap_or(true)
        })
        .map(|mission| plan_one_mission(mission, proposals))
        .collect()
}

fn plan_one_mission(mission: &MissionRecord, proposals: &[ProposalRecord]) -> MissionAutonomyPlan {
    let pending_proposals = proposals
        .iter()
        .filter(|proposal| proposal.mission_id.as_deref() == Some(mission.id.as_str()))
        .filter(|proposal| proposal.status == "pending")
        .count();
    let completed = matches!(
        mission.status.as_str(),
        "completed" | "done" | "closed" | "cancelled" | "canceled" | "failed"
    );
    let mut actions = Vec::new();
    let (next_stage, autonomy_level, risk) = if pending_proposals > 0 {
        actions.push(MissionPlanAction {
            kind: "proposal-review".to_string(),
            stage: "authorize".to_string(),
            summary: format!("Review {pending_proposals} pending proposal(s) before mutation."),
            command: Some(format!(
                "jarvisctl proposal list --output table | rg {}",
                mission.id
            )),
            requires_proposal: false,
        });
        ("authorize", "blocked_on_operator", "operator_decision")
    } else if completed {
        actions.push(MissionPlanAction {
            kind: "archive-learning".to_string(),
            stage: "learn".to_string(),
            summary: "Preserve final evidence and promote reusable workflow notes.".to_string(),
            command: Some(format!(
                "jarvisctl mission show {} --output yaml",
                mission.id
            )),
            requires_proposal: false,
        });
        ("learn", "ready", "low")
    } else if mission.namespaces.is_empty() && mission.tickets.is_empty() {
        actions.push(MissionPlanAction {
            kind: "create-ticket".to_string(),
            stage: "sense".to_string(),
            summary: "Create or attach an execution ticket so the mission has a concrete contract."
                .to_string(),
            command: None,
            requires_proposal: false,
        });
        ("sense", "supervised", "missing_contract")
    } else if mission.namespaces.is_empty() {
        let ticket = mission.tickets.first().cloned().unwrap_or_default();
        actions.push(MissionPlanAction {
            kind: "start-session".to_string(),
            stage: "task".to_string(),
            summary: "Start a scheduled Codex session and bind runtime events to this mission."
                .to_string(),
            command: Some(format!(
                "jarvisctl node start-session --node auto --task-note {} --mission {}",
                ticket, mission.id
            )),
            requires_proposal: false,
        });
        ("task", "auto_allowed_with_event", "medium")
    } else {
        actions.push(MissionPlanAction {
            kind: "verify-runtime".to_string(),
            stage: "verify".to_string(),
            summary: "Inspect live namespaces, capture evidence, and decide whether to close or continue.".to_string(),
            command: Some(format!("jarvisctl mission show {} --output table", mission.id)),
            requires_proposal: false,
        });
        ("verify", "supervised", "medium")
    };

    MissionAutonomyPlan {
        mission_id: mission.id.clone(),
        title: mission.title.clone(),
        status: mission.status.clone(),
        next_stage: next_stage.to_string(),
        autonomy_level: autonomy_level.to_string(),
        pending_proposals,
        risk: risk.to_string(),
        actions,
    }
}

pub fn worker_lane_scorecards(
    missions: &[MissionRecord],
    proposals: &[ProposalRecord],
) -> Vec<WorkerLaneScorecard> {
    let mission_count = missions.len();
    let openclaw_evidence = missions
        .iter()
        .filter(|mission| {
            mission
                .labels
                .get("pattern")
                .map(|value| value == "openclaw")
                .unwrap_or(false)
        })
        .count();
    let pending_proposals = proposals
        .iter()
        .filter(|proposal| proposal.status == "pending")
        .count();
    vec![
        WorkerLaneScorecard {
            lane: "codex-remote-session".to_string(),
            readiness: if mission_count > 0 {
                "usable"
            } else {
                "baseline"
            }
            .to_string(),
            confidence: if mission_count > 0 { 80 } else { 55 },
            evidence: vec![
                "node preflight covers reachability and Codex/Jarvis versions".to_string(),
                "mission events capture namespace and node links".to_string(),
            ],
            gaps: vec!["needs recurring two-node mission smoke in CI or cron".to_string()],
        },
        WorkerLaneScorecard {
            lane: "bounded-worker-offload".to_string(),
            readiness: if openclaw_evidence > 0 {
                "candidate"
            } else {
                "unproven"
            }
            .to_string(),
            confidence: if openclaw_evidence > 0 { 68 } else { 35 },
            evidence: vec![format!(
                "{openclaw_evidence} OpenClaw-pattern mission(s) recorded"
            )],
            gaps: vec![
                "record schema/test pass rates per worker lane".to_string(),
                "promote only lanes with repeatable validation artifacts".to_string(),
            ],
        },
        WorkerLaneScorecard {
            lane: "operator-proposal-gate".to_string(),
            readiness: if pending_proposals == 0 {
                "clear"
            } else {
                "attention"
            }
            .to_string(),
            confidence: 75,
            evidence: vec![format!("{pending_proposals} pending proposal(s)")],
            gaps: vec![
                "add recurring proposal approve/reject smoke coverage through the dashboard"
                    .to_string(),
            ],
        },
    ]
}

pub fn render_autonomy_policy_output(
    rules: &[AutonomyPolicyRule],
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(rules).context("failed to encode autonomy policy")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(rules).context("failed to encode autonomy policy")
        }
        ControlPlaneOutput::Table => {
            let mut lines = vec!["ID\tDECISION\tACTION CLASS\tRATIONALE".to_string()];
            for rule in rules {
                lines.push(format!(
                    "{}\t{}\t{}\t{}",
                    rule.id, rule.decision, rule.action_class, rule.rationale
                ));
            }
            Ok(lines.join("\n"))
        }
    }
}

pub fn render_mission_plans_output(
    plans: &[MissionAutonomyPlan],
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(plans).context("failed to encode mission plans")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(plans).context("failed to encode mission plans")
        }
        ControlPlaneOutput::Table => {
            let mut lines = vec!["MISSION\tSTATUS\tNEXT\tAUTONOMY\tRISK\tACTION".to_string()];
            for plan in plans {
                let action = plan
                    .actions
                    .first()
                    .map(|action| action.summary.as_str())
                    .unwrap_or("-");
                lines.push(format!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    plan.title,
                    plan.status,
                    plan.next_stage,
                    plan.autonomy_level,
                    plan.risk,
                    action
                ));
            }
            Ok(lines.join("\n"))
        }
    }
}

pub fn render_worker_lane_scorecards_output(
    scorecards: &[WorkerLaneScorecard],
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => serde_json::to_string_pretty(scorecards)
            .context("failed to encode worker lane scorecards"),
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(scorecards).context("failed to encode worker lane scorecards")
        }
        ControlPlaneOutput::Table => {
            let mut lines = vec!["LANE\tREADINESS\tCONFIDENCE\tEVIDENCE\tGAPS".to_string()];
            for scorecard in scorecards {
                lines.push(format!(
                    "{}\t{}\t{}\t{}\t{}",
                    scorecard.lane,
                    scorecard.readiness,
                    scorecard.confidence,
                    scorecard.evidence.join("; "),
                    scorecard.gaps.join("; ")
                ));
            }
            Ok(lines.join("\n"))
        }
    }
}
