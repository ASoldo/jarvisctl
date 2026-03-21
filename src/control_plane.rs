use crate::SessionBackend;
use crate::codex::{
    CodexLaunchOptions, CodexRuntimeDriver, enrich_native_sessions, launch_codex_ticket,
};
use crate::codex_app::{collect_codex_app_sessions, delete_codex_app_session};
use crate::native::{
    NativeSessionCompletion, NativeSessionMetadata, RuntimeContextMetadata,
    collect_native_sessions, delete_native_session, native_session_completion,
};
use crate::ticket::slugify;
use anyhow::{Context, anyhow, bail, ensure};
use chrono::Utc;
use clap::ValueEnum;
use cron::Schedule;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_yaml::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::str::FromStr;
use std::time::{Duration, Instant};

const API_VERSION: &str = "jarvisctl.io/v1alpha1";
const JOB_COMPLETION_GRACE_MS: u128 = 5_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ControlPlaneOutput {
    Table,
    Json,
    Yaml,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ControlPlaneResourceKindArg {
    #[value(alias = "namespace", alias = "namespaces")]
    Namespace,
    #[value(alias = "deployment", alias = "deployments")]
    Deployment,
    #[value(
        alias = "replicaset",
        alias = "replicasets",
        alias = "replica-set",
        alias = "replica-sets"
    )]
    ReplicaSet,
    #[value(alias = "job", alias = "jobs")]
    Job,
    #[value(
        alias = "cronjob",
        alias = "cronjobs",
        alias = "cron-job",
        alias = "cron-jobs"
    )]
    CronJob,
    #[value(
        alias = "application",
        alias = "applications",
        alias = "app",
        alias = "apps"
    )]
    Application,
    #[value(alias = "service", alias = "services")]
    Service,
    #[value(
        alias = "networkpolicy",
        alias = "networkpolicies",
        alias = "network-policy",
        alias = "network-policies"
    )]
    NetworkPolicy,
    #[value(alias = "configmap", alias = "configmaps")]
    ConfigMap,
    #[value(alias = "secret", alias = "secrets")]
    Secret,
    #[value(alias = "volume", alias = "volumes")]
    Volume,
    #[value(alias = "worker", alias = "workers")]
    Worker,
    #[value(alias = "resources")]
    All,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResourceKind {
    Namespace,
    Deployment,
    ReplicaSet,
    Job,
    CronJob,
    Application,
    Service,
    NetworkPolicy,
    ConfigMap,
    Secret,
    Volume,
    Worker,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResourceMetadata {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NamespaceSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_working_directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_driver: Option<CodexRuntimeDriver>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_startup_delay_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigMapSpec {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub data: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SecretSpec {
    #[serde(
        default,
        rename = "stringData",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub string_data: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VolumeSpec {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkerProvider {
    #[default]
    Ollama,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOutputMode {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkerSpec {
    #[serde(default)]
    pub provider: WorkerProvider,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(
        default,
        rename = "systemPrompt",
        skip_serializing_if = "Option::is_none"
    )]
    pub system_prompt: Option<String>,
    #[serde(
        default,
        rename = "outputMode",
        skip_serializing_if = "Option::is_none"
    )]
    pub output_mode: Option<WorkerOutputMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(
        default,
        rename = "numPredict",
        skip_serializing_if = "Option::is_none"
    )]
    pub num_predict: Option<u32>,
    #[serde(default, rename = "numCtx", skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ServiceRoutingStrategy {
    #[default]
    FirstReady,
    RoundRobin,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServiceSpec {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub selector: BTreeMap<String, String>,
    #[serde(default)]
    pub strategy: ServiceRoutingStrategy,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LabelSelector {
    #[serde(
        default,
        rename = "matchLabels",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub match_labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkPolicyPeer {
    #[serde(
        default,
        rename = "namespaceSelector",
        skip_serializing_if = "Option::is_none"
    )]
    pub namespace_selector: Option<LabelSelector>,
    #[serde(
        default,
        rename = "podSelector",
        skip_serializing_if = "Option::is_none"
    )]
    pub pod_selector: Option<LabelSelector>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkPolicyIngressRule {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub from: Vec<NetworkPolicyPeer>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkPolicyEgressRule {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<NetworkPolicyPeer>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
pub enum NetworkPolicyType {
    #[serde(rename = "Ingress")]
    Ingress,
    #[serde(rename = "Egress")]
    Egress,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkPolicySpec {
    #[serde(default, rename = "podSelector")]
    pub pod_selector: LabelSelector,
    #[serde(default, rename = "policyTypes", skip_serializing_if = "Vec::is_empty")]
    pub policy_types: Vec<NetworkPolicyType>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ingress: Vec<NetworkPolicyIngressRule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub egress: Vec<NetworkPolicyEgressRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeploymentTemplateSpec {
    pub task_note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default, rename = "configMaps", skip_serializing_if = "Vec::is_empty")]
    pub config_maps: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentSpec {
    #[serde(default = "default_replicas")]
    pub replicas: usize,
    #[serde(default = "default_agents")]
    pub agents: usize,
    #[serde(
        default = "default_revision_history_limit",
        rename = "revisionHistoryLimit"
    )]
    pub revision_history_limit: usize,
    #[serde(default)]
    pub paused: bool,
    #[serde(
        default = "default_progress_deadline_seconds",
        rename = "progressDeadlineSeconds"
    )]
    pub progress_deadline_seconds: u64,
    #[serde(
        default,
        rename = "restartToken",
        skip_serializing_if = "Option::is_none"
    )]
    pub restart_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<DeploymentStrategy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<CodexRuntimeDriver>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_delay_ms: Option<u64>,
    pub template: DeploymentTemplateSpec,
}

impl Default for DeploymentSpec {
    fn default() -> Self {
        Self {
            replicas: default_replicas(),
            agents: default_agents(),
            revision_history_limit: default_revision_history_limit(),
            paused: false,
            progress_deadline_seconds: default_progress_deadline_seconds(),
            restart_token: None,
            strategy: Some(DeploymentStrategy::default()),
            driver: None,
            startup_delay_ms: None,
            template: DeploymentTemplateSpec::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, Eq, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub enum DeploymentStrategyType {
    Recreate,
    #[default]
    RollingUpdate,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
pub enum IntOrPercent {
    Int(usize),
    String(String),
}

impl Default for IntOrPercent {
    fn default() -> Self {
        Self::String("25%".to_string())
    }
}

fn is_default_int_or_percent(value: &IntOrPercent) -> bool {
    value == &IntOrPercent::default()
}

fn resolve_int_or_percent(
    value: &IntOrPercent,
    replicas: usize,
    round_up: bool,
) -> anyhow::Result<usize> {
    match value {
        IntOrPercent::Int(value) => Ok(*value),
        IntOrPercent::String(raw) => {
            let trimmed = raw.trim();
            if let Some(percent) = trimmed.strip_suffix('%') {
                let percent = percent
                    .trim()
                    .parse::<usize>()
                    .with_context(|| format!("invalid percentage '{}'", raw))?;
                let scaled = replicas.saturating_mul(percent);
                Ok(if round_up {
                    scaled.saturating_add(99) / 100
                } else {
                    scaled / 100
                })
            } else {
                trimmed
                    .parse::<usize>()
                    .with_context(|| format!("invalid int-or-percent '{}'", raw))
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RollingUpdateDeployment {
    #[serde(
        default,
        rename = "maxUnavailable",
        skip_serializing_if = "is_default_int_or_percent"
    )]
    pub max_unavailable: IntOrPercent,
    #[serde(
        default,
        rename = "maxSurge",
        skip_serializing_if = "is_default_int_or_percent"
    )]
    pub max_surge: IntOrPercent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentStrategy {
    #[serde(default, rename = "type")]
    pub strategy_type: DeploymentStrategyType,
    #[serde(
        default,
        rename = "rollingUpdate",
        skip_serializing_if = "Option::is_none"
    )]
    pub rolling_update: Option<RollingUpdateDeployment>,
}

impl Default for DeploymentStrategy {
    fn default() -> Self {
        Self {
            strategy_type: DeploymentStrategyType::RollingUpdate,
            rolling_update: Some(RollingUpdateDeployment::default()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicaSetSpec {
    #[serde(rename = "deploymentName")]
    pub deployment_name: String,
    pub revision: u64,
    pub replicas: usize,
    pub agents: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<CodexRuntimeDriver>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_delay_ms: Option<u64>,
    #[serde(
        default,
        rename = "restartToken",
        skip_serializing_if = "Option::is_none"
    )]
    pub restart_token: Option<String>,
    #[serde(rename = "templateHash")]
    pub template_hash: String,
    pub template: DeploymentTemplateSpec,
}

impl Default for ReplicaSetSpec {
    fn default() -> Self {
        Self {
            deployment_name: String::new(),
            revision: 1,
            replicas: default_replicas(),
            agents: default_agents(),
            driver: None,
            startup_delay_ms: None,
            restart_token: None,
            template_hash: String::new(),
            template: DeploymentTemplateSpec::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    #[serde(default = "default_parallelism")]
    pub parallelism: usize,
    #[serde(default = "default_completions")]
    pub completions: usize,
    #[serde(default = "default_agents")]
    pub agents: usize,
    #[serde(default, rename = "backoffLimit")]
    pub backoff_limit: usize,
    #[serde(default)]
    pub suspend: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<CodexRuntimeDriver>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_delay_ms: Option<u64>,
    pub template: DeploymentTemplateSpec,
}

impl Default for JobSpec {
    fn default() -> Self {
        Self {
            parallelism: default_parallelism(),
            completions: default_completions(),
            agents: default_agents(),
            backoff_limit: default_backoff_limit(),
            suspend: false,
            driver: None,
            startup_delay_ms: None,
            template: DeploymentTemplateSpec::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct JobTemplateSpec {
    pub spec: JobSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum CronJobConcurrencyPolicy {
    #[serde(rename = "Allow")]
    #[default]
    Allow,
    #[serde(rename = "Forbid")]
    Forbid,
    #[serde(rename = "Replace")]
    Replace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJobSpec {
    pub schedule: String,
    #[serde(default)]
    pub suspend: bool,
    #[serde(default, rename = "concurrencyPolicy")]
    pub concurrency_policy: CronJobConcurrencyPolicy,
    #[serde(
        default = "default_successful_jobs_history_limit",
        rename = "successfulJobsHistoryLimit"
    )]
    pub successful_jobs_history_limit: usize,
    #[serde(
        default = "default_failed_jobs_history_limit",
        rename = "failedJobsHistoryLimit"
    )]
    pub failed_jobs_history_limit: usize,
    #[serde(rename = "jobTemplate")]
    pub job_template: JobTemplateSpec,
}

impl Default for CronJobSpec {
    fn default() -> Self {
        Self {
            schedule: "* * * * *".to_string(),
            suspend: false,
            concurrency_policy: CronJobConcurrencyPolicy::Allow,
            successful_jobs_history_limit: default_successful_jobs_history_limit(),
            failed_jobs_history_limit: default_failed_jobs_history_limit(),
            job_template: JobTemplateSpec::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApplicationSourceSpec {
    pub path: String,
    #[serde(
        default,
        rename = "repoURL",
        alias = "repoUrl",
        skip_serializing_if = "Option::is_none"
    )]
    pub repo_url: Option<String>,
    #[serde(
        default,
        rename = "targetRevision",
        skip_serializing_if = "Option::is_none"
    )]
    pub target_revision: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApplicationDestinationSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplicationAutomatedSyncPolicy {
    #[serde(default = "default_true")]
    pub enable: bool,
    #[serde(default = "default_true", rename = "prune")]
    pub prune: bool,
    #[serde(default = "default_true", rename = "selfHeal")]
    pub self_heal: bool,
}

impl Default for ApplicationAutomatedSyncPolicy {
    fn default() -> Self {
        Self {
            enable: true,
            prune: true,
            self_heal: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplicationSyncPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automated: Option<ApplicationAutomatedSyncPolicy>,
}

impl Default for ApplicationSyncPolicy {
    fn default() -> Self {
        Self {
            automated: Some(ApplicationAutomatedSyncPolicy::default()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApplicationSpec {
    pub source: ApplicationSourceSpec,
    pub destination: ApplicationDestinationSpec,
    #[serde(default, rename = "syncPolicy")]
    pub sync_policy: ApplicationSyncPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceEnvelope<T> {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: ResourceMetadata,
    pub spec: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum ResourceManifest {
    Namespace(ResourceEnvelope<NamespaceSpec>),
    Deployment(ResourceEnvelope<DeploymentSpec>),
    ReplicaSet(ResourceEnvelope<ReplicaSetSpec>),
    Job(ResourceEnvelope<JobSpec>),
    CronJob(ResourceEnvelope<CronJobSpec>),
    Application(ResourceEnvelope<ApplicationSpec>),
    Service(ResourceEnvelope<ServiceSpec>),
    NetworkPolicy(ResourceEnvelope<NetworkPolicySpec>),
    ConfigMap(ResourceEnvelope<ConfigMapSpec>),
    Secret(ResourceEnvelope<SecretSpec>),
    Volume(ResourceEnvelope<VolumeSpec>),
    Worker(ResourceEnvelope<WorkerSpec>),
}

#[derive(Debug, Clone, Serialize)]
struct ResourceSummary {
    kind: String,
    namespace: Option<String>,
    name: String,
    status: String,
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
struct DeploymentStatus {
    replicas: usize,
    ready_replicas: usize,
    updated_replicas: usize,
    unavailable_replicas: usize,
    paused: bool,
    progressing: bool,
    available: bool,
    failed: bool,
    strategy: String,
    progress_deadline_seconds: u64,
    current_revision: Option<u64>,
    current_replica_set: Option<String>,
    replica_sets: Vec<ReplicaSetStatus>,
    sessions: Vec<String>,
    conditions: Vec<DeploymentCondition>,
}

#[derive(Debug, Clone, Serialize)]
struct ReplicaSetStatus {
    deployment_name: String,
    revision: u64,
    template_hash: String,
    replicas: usize,
    ready_replicas: usize,
    sessions: Vec<String>,
    active: bool,
}

#[derive(Debug, Clone, Serialize)]
struct JobStatus {
    completions: usize,
    active: usize,
    succeeded: usize,
    failed: usize,
    runs: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CronJobStatus {
    schedule: String,
    active_jobs: Vec<String>,
    last_schedule_epoch_ms: Option<u128>,
}

#[derive(Debug, Clone, Serialize)]
struct ApplicationStatus {
    source_path: String,
    repo_url: Option<String>,
    source_type: String,
    source_root: Option<String>,
    target_revision: String,
    source_revision: String,
    source_dirty: bool,
    resolved_revision: String,
    last_applied_revision: Option<String>,
    sync_status: String,
    health_status: String,
    destination_namespace: Option<String>,
    rendered_resources: usize,
    last_sync_epoch_ms: Option<u128>,
    history: Vec<ApplicationSyncHistoryEntry>,
}

#[derive(Debug, Clone, Serialize)]
struct DeploymentCondition {
    #[serde(rename = "type")]
    condition_type: String,
    status: String,
    reason: String,
    message: String,
    last_transition_epoch_ms: u128,
}

#[derive(Debug, Clone)]
struct ApplicationSourceResolution {
    repo_url: Option<String>,
    source_type: String,
    source_root: Option<String>,
    source_revision: String,
    source_dirty: bool,
    render_path: PathBuf,
}

#[derive(Debug, Clone)]
struct ApplicationDesiredState {
    source: ApplicationSourceResolution,
    rendered: Vec<ResourceManifest>,
    rendered_resources: BTreeSet<RenderedResourceRef>,
    resolved_revision: String,
}

#[derive(Debug, Clone, Serialize)]
struct ApplicationDiffEntry {
    action: String,
    kind: String,
    namespace: Option<String>,
    name: String,
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
struct ApplicationDiffResult {
    application: String,
    namespace: String,
    repo_url: Option<String>,
    source_type: String,
    source_revision: String,
    source_dirty: bool,
    target_revision: String,
    resolved_revision: String,
    creates: usize,
    updates: usize,
    deletes: usize,
    changes: Vec<ApplicationDiffEntry>,
}

#[derive(Debug, Clone, Serialize)]
struct ServiceStatus {
    endpoints: Vec<String>,
    strategy: ServiceRoutingStrategy,
}

#[derive(Debug, Clone, Serialize)]
struct NetworkPolicyStatus {
    selected_sessions: Vec<String>,
    policy_types: Vec<NetworkPolicyType>,
}

#[derive(Debug, Clone, Serialize)]
struct WorkerStatus {
    provider: String,
    model: String,
    endpoint: String,
    role: Option<String>,
    output_mode: String,
    loaded: bool,
}

#[derive(Debug, Clone, Serialize)]
struct NamespaceStatus {
    resources: usize,
    sessions: usize,
}

#[derive(Debug, Clone, Serialize)]
struct DescribeEnvelope {
    manifest: serde_json::Value,
    status: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ServiceRouteState {
    next_index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobRunState {
    name: String,
    runtime_namespace: String,
    created_at_epoch_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_active_epoch_ms: Option<u128>,
    status: JobRunPhase,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum JobRunPhase {
    #[default]
    Active,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct JobControllerState {
    runs: Vec<JobRunState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CronJobControllerState {
    last_schedule_epoch_ms: Option<u128>,
    jobs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ApplicationControllerState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_sync_epoch_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rendered_resources: Vec<RenderedResourceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_attempted_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_applied_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    history: Vec<ApplicationSyncHistoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
struct RenderedResourceRef {
    kind: String,
    name: String,
    namespace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApplicationSyncHistoryEntry {
    revision: String,
    synced_at_epoch_ms: u128,
    rendered_resources: usize,
    source_path: String,
}

#[derive(Debug, Clone, Serialize)]
struct DeploymentRolloutHistoryEntry {
    revision: u64,
    replica_set: String,
    template_hash: String,
    replicas: usize,
    ready_replicas: usize,
    created_at_epoch_ms: Option<u128>,
    active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct KustomizationFile {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    resources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
    #[serde(
        default,
        rename = "commonLabels",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    common_labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    patches: Vec<KustomizePatchSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct KustomizePatchSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    patch: Option<String>,
    #[serde(default)]
    target: KustomizePatchTarget,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct KustomizePatchTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ServiceResolution {
    pub runtime_namespace: String,
}

impl<T> ResourceEnvelope<T> {
    fn namespace_key(&self) -> &str {
        self.metadata.namespace.as_deref().unwrap_or("default")
    }
}

impl RenderedResourceRef {
    fn from_manifest(manifest: &ResourceManifest) -> Self {
        Self {
            kind: manifest.kind().display_name().to_string(),
            name: manifest.name().to_string(),
            namespace: manifest.namespace().map(ToOwned::to_owned),
        }
    }
}

pub fn apply_manifests(paths: &[PathBuf]) -> anyhow::Result<Vec<String>> {
    ensure!(
        !paths.is_empty(),
        "provide at least one manifest file to apply"
    );
    let mut manifests = Vec::new();
    for path in paths {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read manifest '{}'", path.display()))?;
        let mut parsed = parse_manifest_documents(&raw)?;
        resolve_manifest_relative_paths(
            &mut parsed,
            path.parent().unwrap_or_else(|| Path::new(".")),
        );
        manifests.extend(parsed);
    }

    let mut messages = Vec::new();
    for manifest in &manifests {
        save_manifest(manifest)?;
        if !matches!(
            manifest,
            ResourceManifest::Deployment(_)
                | ResourceManifest::Job(_)
                | ResourceManifest::CronJob(_)
                | ResourceManifest::Application(_)
        ) {
            messages.push(format!("applied {}", manifest_ref(manifest)));
        }
    }
    messages.extend(reconcile_control_plane()?);
    Ok(messages)
}

pub fn apply_kustomization(path: &Path) -> anyhow::Result<Vec<String>> {
    let manifests = render_source_path(path, None, &BTreeMap::new())?;
    ensure!(
        !manifests.is_empty(),
        "kustomization '{}' did not render any resources",
        path.display()
    );

    let mut messages = Vec::new();
    for manifest in &manifests {
        save_manifest(manifest)?;
        if !matches!(
            manifest,
            ResourceManifest::Deployment(_)
                | ResourceManifest::Job(_)
                | ResourceManifest::CronJob(_)
                | ResourceManifest::Application(_)
        ) {
            messages.push(format!("applied {}", manifest_ref(manifest)));
        }
    }
    messages.extend(reconcile_control_plane()?);
    Ok(messages)
}

pub fn render_get_output(
    kind_arg: ControlPlaneResourceKindArg,
    namespace: Option<&str>,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    let _ = reconcile_control_plane()?;
    let summaries = list_resource_summaries(kind_arg, namespace)?;
    match output {
        ControlPlaneOutput::Json => serde_json::to_string_pretty(&summaries)
            .context("failed to encode control-plane summaries"),
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(&summaries).context("failed to encode control-plane summaries")
        }
        ControlPlaneOutput::Table => Ok(render_summary_table(&summaries)),
    }
}

pub fn render_describe_output(
    kind_arg: ControlPlaneResourceKindArg,
    name: &str,
    namespace: Option<&str>,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    let _ = reconcile_control_plane()?;
    let kind = parse_specific_kind(kind_arg)?;
    let manifest = load_manifest(kind, name, namespace)?;
    let status = describe_status(&manifest)?;
    let envelope = DescribeEnvelope {
        manifest: serde_json::to_value(&manifest).context("failed to encode manifest")?,
        status,
    };
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(&envelope).context("failed to encode describe payload")
        }
        ControlPlaneOutput::Yaml | ControlPlaneOutput::Table => {
            serde_yaml::to_string(&envelope).context("failed to encode describe payload")
        }
    }
}

pub fn sync_application_resource(
    application_name: &str,
    namespace: Option<&str>,
) -> anyhow::Result<String> {
    let control_namespace = normalize_namespaced_resource_namespace(namespace);
    let manifest = load_manifest(
        ResourceKind::Application,
        application_name,
        Some(&control_namespace),
    )?;
    let ResourceManifest::Application(application) = manifest else {
        bail!(
            "resource '{}/{}' is not an Application",
            control_namespace,
            application_name
        );
    };
    sync_application(&application, true)
}

pub fn render_application_diff_output(
    application_name: &str,
    namespace: Option<&str>,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    let diff = application_diff(application_name, namespace)?;
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(&diff).context("failed to encode application diff")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(&diff).context("failed to encode application diff")
        }
        ControlPlaneOutput::Table => Ok(render_application_diff_table(&diff)),
    }
}

pub fn render_rollout_status_output(
    deployment_name: &str,
    namespace: Option<&str>,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    let (_, deployment, status) =
        load_rollout_status(deployment_name, namespace).with_context(|| {
            format!(
                "failed to load rollout status for Deployment '{}/{}'",
                normalize_namespaced_resource_namespace(namespace),
                deployment_name
            )
        })?;
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(&status).context("failed to encode rollout status")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(&status).context("failed to encode rollout status")
        }
        ControlPlaneOutput::Table => Ok(render_rollout_status_table(
            deployment.namespace_key(),
            &deployment.metadata.name,
            &status,
        )),
    }
}

pub fn wait_for_rollout_status_output(
    deployment_name: &str,
    namespace: Option<&str>,
    output: ControlPlaneOutput,
    timeout: Duration,
) -> anyhow::Result<String> {
    let started_at = Instant::now();
    loop {
        let (resolved_namespace, deployment, status) =
            load_rollout_status(deployment_name, namespace)?;
        if deployment_rollout_complete(&status) {
            return match output {
                ControlPlaneOutput::Json => {
                    serde_json::to_string_pretty(&status).context("failed to encode rollout status")
                }
                ControlPlaneOutput::Yaml => {
                    serde_yaml::to_string(&status).context("failed to encode rollout status")
                }
                ControlPlaneOutput::Table => Ok(render_rollout_status_table(
                    deployment.namespace_key(),
                    &deployment.metadata.name,
                    &status,
                )),
            };
        }
        if status.failed {
            let failure_message = deployment_rollout_failure_message(&status)
                .unwrap_or_else(|| "rollout failed".to_string());
            bail!(
                "deployment '{}/{}' rollout failed: {}",
                resolved_namespace,
                deployment_name,
                failure_message
            );
        }
        if started_at.elapsed() >= timeout {
            bail!(
                "timed out waiting for deployment '{}/{}' rollout after {}s",
                resolved_namespace,
                deployment_name,
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn load_rollout_status(
    deployment_name: &str,
    namespace: Option<&str>,
) -> anyhow::Result<(String, ResourceEnvelope<DeploymentSpec>, DeploymentStatus)> {
    let _ = reconcile_control_plane()?;
    let resolved_namespace = normalize_namespaced_resource_namespace(namespace);
    let manifest = load_manifest(
        ResourceKind::Deployment,
        deployment_name,
        Some(&resolved_namespace),
    )?;
    let ResourceManifest::Deployment(deployment) = manifest else {
        bail!("resource '{}' is not a Deployment", deployment_name);
    };
    let status = deployment_status(&deployment)?;
    Ok((resolved_namespace, deployment, status))
}

pub fn render_rollout_history_output(
    deployment_name: &str,
    namespace: Option<&str>,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    let _ = reconcile_control_plane()?;
    let manifest = load_manifest(
        ResourceKind::Deployment,
        deployment_name,
        Some(&normalize_namespaced_resource_namespace(namespace)),
    )?;
    let ResourceManifest::Deployment(deployment) = manifest else {
        bail!("resource '{}' is not a Deployment", deployment_name);
    };
    let history = deployment_rollout_history(&deployment)?;
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(&history).context("failed to encode rollout history")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(&history).context("failed to encode rollout history")
        }
        ControlPlaneOutput::Table => Ok(render_rollout_history_table(&history)),
    }
}

pub fn restart_deployment_rollout(
    deployment_name: &str,
    namespace: Option<&str>,
) -> anyhow::Result<String> {
    let control_namespace = normalize_namespaced_resource_namespace(namespace);
    let manifest = load_manifest(
        ResourceKind::Deployment,
        deployment_name,
        Some(&control_namespace),
    )?;
    let ResourceManifest::Deployment(mut deployment) = manifest else {
        bail!(
            "resource '{}/{}' is not a Deployment",
            control_namespace,
            deployment_name
        );
    };
    deployment.spec.restart_token = Some(now_epoch_ms().to_string());
    save_manifest(&ResourceManifest::Deployment(deployment.clone()))?;
    if deployment.spec.paused {
        Ok(format!(
            "queued rollout restart for paused deployment {}/{}",
            control_namespace, deployment_name
        ))
    } else {
        let message = reconcile_deployment(&deployment)?;
        Ok(format!(
            "triggered rollout restart for deployment {}/{}: {}",
            control_namespace, deployment_name, message
        ))
    }
}

pub fn pause_deployment_rollout(
    deployment_name: &str,
    namespace: Option<&str>,
) -> anyhow::Result<String> {
    let control_namespace = normalize_namespaced_resource_namespace(namespace);
    let manifest = load_manifest(
        ResourceKind::Deployment,
        deployment_name,
        Some(&control_namespace),
    )?;
    let ResourceManifest::Deployment(mut deployment) = manifest else {
        bail!(
            "resource '{}/{}' is not a Deployment",
            control_namespace,
            deployment_name
        );
    };
    if deployment.spec.paused {
        return Ok(format!(
            "deployment {}/{} is already paused",
            control_namespace, deployment_name
        ));
    }
    deployment.spec.paused = true;
    save_manifest(&ResourceManifest::Deployment(deployment))?;
    Ok(format!(
        "paused deployment {}/{}",
        control_namespace, deployment_name
    ))
}

pub fn resume_deployment_rollout(
    deployment_name: &str,
    namespace: Option<&str>,
) -> anyhow::Result<String> {
    let control_namespace = normalize_namespaced_resource_namespace(namespace);
    let manifest = load_manifest(
        ResourceKind::Deployment,
        deployment_name,
        Some(&control_namespace),
    )?;
    let ResourceManifest::Deployment(mut deployment) = manifest else {
        bail!(
            "resource '{}/{}' is not a Deployment",
            control_namespace,
            deployment_name
        );
    };
    if !deployment.spec.paused {
        return Ok(format!(
            "deployment {}/{} is not paused",
            control_namespace, deployment_name
        ));
    }
    deployment.spec.paused = false;
    save_manifest(&ResourceManifest::Deployment(deployment.clone()))?;
    let message = reconcile_deployment(&deployment)?;
    Ok(format!(
        "resumed deployment {}/{}: {}",
        control_namespace, deployment_name, message
    ))
}

pub fn undo_deployment_rollout(
    deployment_name: &str,
    namespace: Option<&str>,
    to_revision: Option<u64>,
) -> anyhow::Result<String> {
    let control_namespace = normalize_namespaced_resource_namespace(namespace);
    let manifest = load_manifest(
        ResourceKind::Deployment,
        deployment_name,
        Some(&control_namespace),
    )?;
    let ResourceManifest::Deployment(mut deployment) = manifest else {
        bail!(
            "resource '{}/{}' is not a Deployment",
            control_namespace,
            deployment_name
        );
    };
    let history = deployment_rollout_history(&deployment)?;
    let target_history = if let Some(revision) = to_revision {
        history.into_iter().find(|entry| entry.revision == revision)
    } else {
        history.into_iter().find(|entry| {
            Some(entry.revision)
                != deployment_status(&deployment)
                    .ok()
                    .and_then(|status| status.current_revision)
        })
    }
    .ok_or_else(|| {
        anyhow!(
            "no rollout revision available to undo for {}/{}",
            control_namespace,
            deployment_name
        )
    })?;
    let target_replica_set = load_manifest(
        ResourceKind::ReplicaSet,
        &target_history.replica_set,
        Some(&control_namespace),
    )?;
    let ResourceManifest::ReplicaSet(target_replica_set) = target_replica_set else {
        bail!(
            "resource '{}/{}' is not a ReplicaSet",
            control_namespace,
            target_history.replica_set
        );
    };

    deployment.spec.agents = target_replica_set.spec.agents;
    deployment.spec.driver = target_replica_set.spec.driver;
    deployment.spec.startup_delay_ms = target_replica_set.spec.startup_delay_ms;
    deployment.spec.restart_token = target_replica_set.spec.restart_token.clone();
    deployment.spec.template = target_replica_set.spec.template.clone();
    save_manifest(&ResourceManifest::Deployment(deployment.clone()))?;
    if deployment.spec.paused {
        Ok(format!(
            "updated paused deployment {}/{} to target revision {}",
            control_namespace, deployment_name, target_history.revision
        ))
    } else {
        let message = reconcile_deployment(&deployment)?;
        Ok(format!(
            "rolled back deployment {}/{} to revision {}: {}",
            control_namespace, deployment_name, target_history.revision, message
        ))
    }
}

pub fn resolve_service_target(
    service_name: &str,
    control_namespace: Option<&str>,
) -> anyhow::Result<ServiceResolution> {
    resolve_service_target_for_source(service_name, control_namespace, None)
}

pub fn resolve_service_target_for_message(
    service_name: &str,
    control_namespace: Option<&str>,
    source_runtime_namespace: Option<&str>,
) -> anyhow::Result<ServiceResolution> {
    resolve_service_target_for_source(service_name, control_namespace, source_runtime_namespace)
}

pub fn authorize_runtime_message(
    source_runtime_namespace: Option<&str>,
    target_runtime_namespace: &str,
) -> anyhow::Result<()> {
    let Some(source_runtime_namespace) = source_runtime_namespace else {
        return Ok(());
    };
    let source = load_runtime_session_by_namespace(source_runtime_namespace)?;
    let target = load_runtime_session_by_namespace(target_runtime_namespace)?;
    ensure_message_flow_allowed(Some(&source), &target)
}

fn resolve_service_target_for_source(
    service_name: &str,
    control_namespace: Option<&str>,
    source_runtime_namespace: Option<&str>,
) -> anyhow::Result<ServiceResolution> {
    let namespace = normalize_namespaced_resource_namespace(control_namespace);
    let _ = reconcile_control_plane()?;
    let manifest = load_manifest(ResourceKind::Service, service_name, Some(&namespace))?;
    let ResourceManifest::Service(service) = manifest else {
        bail!("resource '{}' is not a Service", service_name);
    };

    let source = match source_runtime_namespace {
        Some(namespace) => Some(load_runtime_session_by_namespace(namespace)?),
        None => None,
    };
    let mut sessions = collect_runtime_sessions()?;
    sessions.retain(|session| service_matches_session(&service, session));
    let reachable_before_policy = !sessions.is_empty();
    if let Some(source) = source.as_ref() {
        sessions.retain(|session| ensure_message_flow_allowed(Some(source), session).is_ok());
        ensure!(
            !sessions.is_empty(),
            "service '{}/{}' has no reachable endpoints under current network policy",
            namespace,
            service_name
        );
    } else {
        ensure!(
            reachable_before_policy,
            "service '{}/{}' has no ready endpoints",
            namespace,
            service_name
        );
    }
    sessions.sort_by(|left, right| left.namespace.cmp(&right.namespace));

    let index = match service.spec.strategy {
        ServiceRoutingStrategy::FirstReady => 0,
        ServiceRoutingStrategy::RoundRobin => {
            let mut state = load_service_route_state(&namespace, service_name)?;
            let current = state.next_index % sessions.len();
            state.next_index = current + 1;
            save_service_route_state(&namespace, service_name, &state)?;
            current
        }
    };

    Ok(ServiceResolution {
        runtime_namespace: sessions[index].namespace.clone(),
    })
}

fn parse_manifest_documents(raw: &str) -> anyhow::Result<Vec<ResourceManifest>> {
    let mut manifests = Vec::new();
    for document in serde_yaml::Deserializer::from_str(raw) {
        let value = Value::deserialize(document).context("failed to parse YAML manifest")?;
        if matches!(value, Value::Null) {
            continue;
        }
        manifests.push(parse_manifest_value(value)?);
    }
    ensure!(
        !manifests.is_empty(),
        "manifest file did not contain any resources"
    );
    Ok(manifests)
}

fn resolve_manifest_relative_paths(manifests: &mut [ResourceManifest], base_dir: &Path) {
    for manifest in manifests {
        if let ResourceManifest::Application(application) = manifest {
            let source_path = PathBuf::from(&application.spec.source.path);
            if application.spec.source.repo_url.is_none() && source_path.is_relative() {
                application.spec.source.path =
                    base_dir.join(source_path).to_string_lossy().into_owned();
            }
        }
    }
}

fn parse_manifest_value(value: Value) -> anyhow::Result<ResourceManifest> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("manifest is missing kind"))?;
    match kind {
        "Namespace" => {
            let mut manifest: ResourceEnvelope<NamespaceSpec> =
                serde_yaml::from_value(value).context("failed to decode Namespace manifest")?;
            normalize_metadata(&mut manifest.metadata, true)?;
            manifest.api_version = API_VERSION.to_string();
            manifest.kind = "Namespace".to_string();
            Ok(ResourceManifest::Namespace(manifest))
        }
        "Deployment" => {
            let mut manifest: ResourceEnvelope<DeploymentSpec> =
                serde_yaml::from_value(value).context("failed to decode Deployment manifest")?;
            normalize_metadata(&mut manifest.metadata, false)?;
            validate_deployment(&manifest)?;
            manifest.api_version = API_VERSION.to_string();
            manifest.kind = "Deployment".to_string();
            Ok(ResourceManifest::Deployment(manifest))
        }
        "ReplicaSet" => {
            let mut manifest: ResourceEnvelope<ReplicaSetSpec> =
                serde_yaml::from_value(value).context("failed to decode ReplicaSet manifest")?;
            normalize_metadata(&mut manifest.metadata, false)?;
            validate_replica_set(&manifest)?;
            manifest.api_version = API_VERSION.to_string();
            manifest.kind = "ReplicaSet".to_string();
            Ok(ResourceManifest::ReplicaSet(manifest))
        }
        "Job" => {
            let mut manifest: ResourceEnvelope<JobSpec> =
                serde_yaml::from_value(value).context("failed to decode Job manifest")?;
            normalize_metadata(&mut manifest.metadata, false)?;
            validate_job(&manifest)?;
            manifest.api_version = API_VERSION.to_string();
            manifest.kind = "Job".to_string();
            Ok(ResourceManifest::Job(manifest))
        }
        "CronJob" => {
            let mut manifest: ResourceEnvelope<CronJobSpec> =
                serde_yaml::from_value(value).context("failed to decode CronJob manifest")?;
            normalize_metadata(&mut manifest.metadata, false)?;
            validate_cron_job(&manifest)?;
            manifest.api_version = API_VERSION.to_string();
            manifest.kind = "CronJob".to_string();
            Ok(ResourceManifest::CronJob(manifest))
        }
        "Application" => {
            let mut manifest: ResourceEnvelope<ApplicationSpec> =
                serde_yaml::from_value(value).context("failed to decode Application manifest")?;
            normalize_metadata(&mut manifest.metadata, false)?;
            validate_application(&manifest)?;
            manifest.api_version = API_VERSION.to_string();
            manifest.kind = "Application".to_string();
            Ok(ResourceManifest::Application(manifest))
        }
        "Service" => {
            let mut manifest: ResourceEnvelope<ServiceSpec> =
                serde_yaml::from_value(value).context("failed to decode Service manifest")?;
            normalize_metadata(&mut manifest.metadata, false)?;
            validate_service(&manifest)?;
            manifest.api_version = API_VERSION.to_string();
            manifest.kind = "Service".to_string();
            Ok(ResourceManifest::Service(manifest))
        }
        "NetworkPolicy" => {
            let mut manifest: ResourceEnvelope<NetworkPolicySpec> =
                serde_yaml::from_value(value).context("failed to decode NetworkPolicy manifest")?;
            normalize_metadata(&mut manifest.metadata, false)?;
            validate_network_policy(&manifest)?;
            manifest.api_version = API_VERSION.to_string();
            manifest.kind = "NetworkPolicy".to_string();
            Ok(ResourceManifest::NetworkPolicy(manifest))
        }
        "ConfigMap" => {
            let mut manifest: ResourceEnvelope<ConfigMapSpec> =
                serde_yaml::from_value(value).context("failed to decode ConfigMap manifest")?;
            normalize_metadata(&mut manifest.metadata, false)?;
            manifest.api_version = API_VERSION.to_string();
            manifest.kind = "ConfigMap".to_string();
            Ok(ResourceManifest::ConfigMap(manifest))
        }
        "Secret" => {
            let mut manifest: ResourceEnvelope<SecretSpec> =
                serde_yaml::from_value(value).context("failed to decode Secret manifest")?;
            normalize_metadata(&mut manifest.metadata, false)?;
            manifest.api_version = API_VERSION.to_string();
            manifest.kind = "Secret".to_string();
            Ok(ResourceManifest::Secret(manifest))
        }
        "Volume" => {
            let mut manifest: ResourceEnvelope<VolumeSpec> =
                serde_yaml::from_value(value).context("failed to decode Volume manifest")?;
            normalize_metadata(&mut manifest.metadata, false)?;
            validate_volume(&manifest)?;
            manifest.api_version = API_VERSION.to_string();
            manifest.kind = "Volume".to_string();
            Ok(ResourceManifest::Volume(manifest))
        }
        "Worker" => {
            let mut manifest: ResourceEnvelope<WorkerSpec> =
                serde_yaml::from_value(value).context("failed to decode Worker manifest")?;
            normalize_metadata(&mut manifest.metadata, false)?;
            validate_worker(&manifest)?;
            manifest.api_version = API_VERSION.to_string();
            manifest.kind = "Worker".to_string();
            Ok(ResourceManifest::Worker(manifest))
        }
        other => bail!("unsupported control-plane resource kind '{}'", other),
    }
}

fn normalize_metadata(metadata: &mut ResourceMetadata, cluster_scoped: bool) -> anyhow::Result<()> {
    metadata.name = metadata.name.trim().to_string();
    ensure!(
        !metadata.name.is_empty(),
        "resource metadata.name must not be empty"
    );
    if cluster_scoped {
        metadata.namespace = None;
    } else {
        metadata.namespace = Some(normalize_namespaced_resource_namespace(
            metadata.namespace.as_deref(),
        ));
    }
    Ok(())
}

fn validate_deployment(manifest: &ResourceEnvelope<DeploymentSpec>) -> anyhow::Result<()> {
    ensure!(
        manifest.spec.replicas > 0,
        "Deployment '{}' must set spec.replicas > 0",
        manifest.metadata.name
    );
    ensure!(
        manifest.spec.agents > 0,
        "Deployment '{}' must set spec.agents > 0",
        manifest.metadata.name
    );
    ensure!(
        !manifest
            .spec
            .restart_token
            .as_deref()
            .unwrap_or("")
            .contains('\n'),
        "Deployment '{}' has an invalid spec.restartToken",
        manifest.metadata.name
    );
    ensure!(
        !manifest.spec.template.task_note.trim().is_empty(),
        "Deployment '{}' must set spec.template.task_note",
        manifest.metadata.name
    );
    ensure!(
        manifest.spec.progress_deadline_seconds > 0,
        "Deployment '{}' must set spec.progressDeadlineSeconds > 0",
        manifest.metadata.name
    );
    if let Some(strategy) = manifest.spec.strategy.as_ref() {
        match strategy.strategy_type {
            DeploymentStrategyType::Recreate => {}
            DeploymentStrategyType::RollingUpdate => {
                let rolling_update = strategy
                    .rolling_update
                    .clone()
                    .unwrap_or_else(RollingUpdateDeployment::default);
                let max_unavailable = resolve_int_or_percent(
                    &rolling_update.max_unavailable,
                    manifest.spec.replicas,
                    false,
                )?;
                let max_surge = resolve_int_or_percent(
                    &rolling_update.max_surge,
                    manifest.spec.replicas,
                    true,
                )?;
                ensure!(
                    max_unavailable > 0 || max_surge > 0,
                    "Deployment '{}' rollingUpdate cannot set both maxUnavailable and maxSurge to 0",
                    manifest.metadata.name
                );
            }
        }
    }
    Ok(())
}

fn validate_replica_set(manifest: &ResourceEnvelope<ReplicaSetSpec>) -> anyhow::Result<()> {
    ensure!(
        manifest.spec.revision > 0,
        "ReplicaSet '{}' must set spec.revision > 0",
        manifest.metadata.name
    );
    ensure!(
        manifest.spec.agents > 0,
        "ReplicaSet '{}' must set spec.agents > 0",
        manifest.metadata.name
    );
    ensure!(
        !manifest.spec.deployment_name.trim().is_empty(),
        "ReplicaSet '{}' must set spec.deploymentName",
        manifest.metadata.name
    );
    ensure!(
        !manifest.spec.template_hash.trim().is_empty(),
        "ReplicaSet '{}' must set spec.templateHash",
        manifest.metadata.name
    );
    ensure!(
        !manifest.spec.template.task_note.trim().is_empty(),
        "ReplicaSet '{}' must set spec.template.task_note",
        manifest.metadata.name
    );
    Ok(())
}

fn validate_job(manifest: &ResourceEnvelope<JobSpec>) -> anyhow::Result<()> {
    ensure!(
        manifest.spec.parallelism > 0,
        "Job '{}' must set spec.parallelism > 0",
        manifest.metadata.name
    );
    ensure!(
        manifest.spec.completions > 0,
        "Job '{}' must set spec.completions > 0",
        manifest.metadata.name
    );
    ensure!(
        manifest.spec.agents > 0,
        "Job '{}' must set spec.agents > 0",
        manifest.metadata.name
    );
    ensure!(
        !manifest.spec.template.task_note.trim().is_empty(),
        "Job '{}' must set spec.template.task_note",
        manifest.metadata.name
    );
    Ok(())
}

fn validate_cron_job(manifest: &ResourceEnvelope<CronJobSpec>) -> anyhow::Result<()> {
    parse_kubernetes_cron_schedule(&manifest.spec.schedule).with_context(|| {
        format!(
            "CronJob '{}' has an invalid spec.schedule '{}'",
            manifest.metadata.name, manifest.spec.schedule
        )
    })?;
    let job_manifest = ResourceEnvelope {
        api_version: API_VERSION.to_string(),
        kind: "Job".to_string(),
        metadata: manifest.metadata.clone(),
        spec: manifest.spec.job_template.spec.clone(),
    };
    validate_job(&job_manifest)?;
    Ok(())
}

fn validate_application(manifest: &ResourceEnvelope<ApplicationSpec>) -> anyhow::Result<()> {
    ensure!(
        !manifest.spec.source.path.trim().is_empty(),
        "Application '{}' must set spec.source.path",
        manifest.metadata.name
    );
    if let Some(repo_url) = manifest.spec.source.repo_url.as_deref() {
        ensure!(
            !repo_url.trim().is_empty(),
            "Application '{}' has an empty spec.source.repoURL",
            manifest.metadata.name
        );
        ensure!(
            !Path::new(&manifest.spec.source.path).is_absolute(),
            "Application '{}' must use a repo-relative spec.source.path when spec.source.repoURL is set",
            manifest.metadata.name
        );
    }
    Ok(())
}

fn validate_service(manifest: &ResourceEnvelope<ServiceSpec>) -> anyhow::Result<()> {
    ensure!(
        !manifest.spec.selector.is_empty(),
        "Service '{}' must set spec.selector",
        manifest.metadata.name
    );
    Ok(())
}

fn validate_network_policy(manifest: &ResourceEnvelope<NetworkPolicySpec>) -> anyhow::Result<()> {
    let policy_types = effective_network_policy_types(&manifest.spec);
    ensure!(
        !policy_types.is_empty(),
        "NetworkPolicy '{}' must define an ingress or egress policy",
        manifest.metadata.name
    );
    Ok(())
}

fn validate_volume(manifest: &ResourceEnvelope<VolumeSpec>) -> anyhow::Result<()> {
    ensure!(
        !manifest.spec.paths.is_empty(),
        "Volume '{}' must define at least one path",
        manifest.metadata.name
    );
    Ok(())
}

fn validate_worker(manifest: &ResourceEnvelope<WorkerSpec>) -> anyhow::Result<()> {
    ensure!(
        !manifest.spec.model.trim().is_empty(),
        "Worker '{}' must set spec.model",
        manifest.metadata.name
    );
    if let Some(endpoint) = manifest.spec.endpoint.as_deref() {
        ensure!(
            endpoint.starts_with("http://") || endpoint.starts_with("https://"),
            "Worker '{}' has invalid spec.endpoint '{}'",
            manifest.metadata.name,
            endpoint
        );
    }
    Ok(())
}

fn save_manifest(manifest: &ResourceManifest) -> anyhow::Result<()> {
    let path = manifest_path(manifest.kind(), manifest.name(), manifest.namespace())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    let raw = serde_yaml::to_string(manifest).context("failed to encode manifest")?;
    fs::write(&path, raw).with_context(|| format!("failed to write '{}'", path.display()))
}

fn reconcile_control_plane() -> anyhow::Result<Vec<String>> {
    let mut messages = Vec::new();

    for manifest in load_manifests_by_kind(ResourceKind::Application, None)? {
        if let Some(message) = reconcile_manifest(&manifest)? {
            messages.push(message);
        }
    }
    for manifest in load_manifests_by_kind(ResourceKind::CronJob, None)? {
        if let Some(message) = reconcile_manifest(&manifest)? {
            messages.push(message);
        }
    }
    for manifest in load_manifests_by_kind(ResourceKind::Job, None)? {
        if let Some(message) = reconcile_manifest(&manifest)? {
            messages.push(message);
        }
    }
    for manifest in load_manifests_by_kind(ResourceKind::Deployment, None)? {
        if let Some(message) = reconcile_manifest(&manifest)? {
            messages.push(message);
        }
    }

    Ok(messages)
}

fn reconcile_manifest(manifest: &ResourceManifest) -> anyhow::Result<Option<String>> {
    match manifest {
        ResourceManifest::Deployment(deployment) => reconcile_deployment(deployment).map(Some),
        ResourceManifest::Job(job) => reconcile_job(job).map(Some),
        ResourceManifest::CronJob(cron_job) => reconcile_cron_job(cron_job).map(Some),
        ResourceManifest::Application(application) => reconcile_application(application).map(Some),
        ResourceManifest::Worker(_) => Ok(None),
        _ => Ok(None),
    }
}

fn reconcile_deployment(manifest: &ResourceEnvelope<DeploymentSpec>) -> anyhow::Result<String> {
    let control_namespace = manifest.namespace_key().to_string();
    let deployment_name = manifest.metadata.name.clone();
    let namespace_defaults = load_namespace_defaults(&control_namespace)?;
    let desired_hash = deployment_template_hash(manifest, &namespace_defaults)?;
    let mut replica_sets = load_replica_sets_for_deployment(&control_namespace, &deployment_name)?;
    replica_sets.sort_by_key(|replica_set| replica_set.spec.revision);
    let target_replica_set_name = if let Some(replica_set) = replica_sets
        .iter()
        .find(|replica_set| replica_set.spec.template_hash == desired_hash)
    {
        replica_set.metadata.name.clone()
    } else if replica_sets.is_empty() {
        let revision = next_deployment_revision(&replica_sets);
        let replica_set = create_replica_set_manifest(
            manifest,
            &namespace_defaults,
            revision,
            manifest.spec.replicas,
            &desired_hash,
        );
        save_manifest(&ResourceManifest::ReplicaSet(replica_set.clone()))?;
        let name = replica_set.metadata.name.clone();
        replica_sets.push(replica_set);
        replica_sets.sort_by_key(|replica_set| replica_set.spec.revision);
        name
    } else if manifest.spec.paused {
        current_deployment_replica_set(&replica_sets)
            .map(|replica_set| replica_set.metadata.name.clone())
            .ok_or_else(|| {
                anyhow!(
                    "deployment '{}/{}' has no active ReplicaSet while paused",
                    control_namespace,
                    deployment_name
                )
            })?
    } else {
        let revision = next_deployment_revision(&replica_sets);
        let replica_set =
            create_replica_set_manifest(manifest, &namespace_defaults, revision, 0, &desired_hash);
        save_manifest(&ResourceManifest::ReplicaSet(replica_set.clone()))?;
        let name = replica_set.metadata.name.clone();
        replica_sets.push(replica_set);
        replica_sets.sort_by_key(|replica_set| replica_set.spec.revision);
        name
    };

    let current_sessions = collect_runtime_sessions()?;
    if !manifest.spec.paused {
        apply_deployment_strategy(
            manifest,
            &target_replica_set_name,
            &mut replica_sets,
            &current_sessions,
        )?;
    }

    prune_deployment_replica_sets(
        &control_namespace,
        &deployment_name,
        &target_replica_set_name,
        manifest.spec.revision_history_limit,
        &mut replica_sets,
    )?;

    for replica_set in &replica_sets {
        reconcile_replica_set_runtime(replica_set)?;
    }

    let status = deployment_status(manifest)?;
    Ok(format!(
        "{} deployment {}/{} (revision {}, ready {}/{})",
        if manifest.spec.paused {
            "paused"
        } else {
            "applied"
        },
        control_namespace,
        deployment_name,
        status.current_revision.unwrap_or(0),
        status.ready_replicas,
        manifest.spec.replicas
    ))
}

fn load_replica_sets_for_deployment(
    control_namespace: &str,
    deployment_name: &str,
) -> anyhow::Result<Vec<ResourceEnvelope<ReplicaSetSpec>>> {
    Ok(
        load_manifests_by_kind(ResourceKind::ReplicaSet, Some(control_namespace))?
            .into_iter()
            .filter_map(|manifest| match manifest {
                ResourceManifest::ReplicaSet(replica_set)
                    if replica_set.spec.deployment_name == deployment_name =>
                {
                    Some(replica_set)
                }
                _ => None,
            })
            .collect(),
    )
}

fn current_deployment_replica_set<'a>(
    replica_sets: &'a [ResourceEnvelope<ReplicaSetSpec>],
) -> Option<&'a ResourceEnvelope<ReplicaSetSpec>> {
    replica_sets
        .iter()
        .filter(|replica_set| replica_set.spec.replicas > 0)
        .max_by_key(|replica_set| replica_set.spec.revision)
        .or_else(|| {
            replica_sets
                .iter()
                .max_by_key(|replica_set| replica_set.spec.revision)
        })
}

fn next_deployment_revision(replica_sets: &[ResourceEnvelope<ReplicaSetSpec>]) -> u64 {
    replica_sets
        .iter()
        .map(|replica_set| replica_set.spec.revision)
        .max()
        .unwrap_or(0)
        + 1
}

fn create_replica_set_manifest(
    manifest: &ResourceEnvelope<DeploymentSpec>,
    namespace_defaults: &NamespaceSpec,
    revision: u64,
    replicas: usize,
    template_hash: &str,
) -> ResourceEnvelope<ReplicaSetSpec> {
    let control_namespace = manifest.namespace_key().to_string();
    let deployment_name = manifest.metadata.name.clone();
    let mut labels = manifest.metadata.labels.clone();
    labels.insert(
        "jarvisctl.io/control-namespace".to_string(),
        control_namespace.clone(),
    );
    labels.insert(
        "jarvisctl.io/deployment".to_string(),
        deployment_name.clone(),
    );
    labels.insert("jarvisctl.io/revision".to_string(), revision.to_string());
    labels.insert(
        "jarvisctl.io/template-hash".to_string(),
        template_hash.to_string(),
    );

    let mut annotations = manifest.metadata.annotations.clone();
    annotations.insert(
        "jarvisctl.io/created-at-epoch-ms".to_string(),
        now_epoch_ms().to_string(),
    );

    ResourceEnvelope {
        api_version: API_VERSION.to_string(),
        kind: "ReplicaSet".to_string(),
        metadata: ResourceMetadata {
            name: deployment_replica_set_name(&deployment_name, revision),
            namespace: Some(control_namespace),
            labels,
            annotations,
        },
        spec: ReplicaSetSpec {
            deployment_name,
            revision,
            replicas,
            agents: manifest.spec.agents,
            driver: Some(
                manifest
                    .spec
                    .driver
                    .or(namespace_defaults.default_driver)
                    .unwrap_or(CodexRuntimeDriver::AppServer),
            ),
            startup_delay_ms: Some(
                manifest
                    .spec
                    .startup_delay_ms
                    .or(namespace_defaults.default_startup_delay_ms)
                    .unwrap_or(1500),
            ),
            restart_token: manifest.spec.restart_token.clone(),
            template_hash: template_hash.to_string(),
            template: resolved_deployment_template(manifest, namespace_defaults),
        },
    }
}

fn prune_deployment_replica_sets(
    control_namespace: &str,
    deployment_name: &str,
    active_replica_set_name: &str,
    revision_history_limit: usize,
    replica_sets: &mut Vec<ResourceEnvelope<ReplicaSetSpec>>,
) -> anyhow::Result<()> {
    replica_sets.sort_by_key(|replica_set| replica_set.spec.revision);
    let mut inactive_names = replica_sets
        .iter()
        .filter(|replica_set| {
            replica_set.metadata.name != active_replica_set_name && replica_set.spec.replicas == 0
        })
        .map(|replica_set| replica_set.metadata.name.clone())
        .collect::<Vec<_>>();
    while inactive_names.len() > revision_history_limit {
        let removed_name = inactive_names.remove(0);
        delete_replica_set_resources(control_namespace, deployment_name, &removed_name)?;
        replica_sets.retain(|replica_set| replica_set.metadata.name != removed_name);
    }
    Ok(())
}

fn delete_replica_set_resources(
    control_namespace: &str,
    deployment_name: &str,
    replica_set_name: &str,
) -> anyhow::Result<()> {
    for session in collect_runtime_sessions()?.into_iter().filter(|session| {
        let Some(context) = session.context.as_ref() else {
            return false;
        };
        context.control_namespace.as_deref() == Some(control_namespace)
            && context.deployment.as_deref() == Some(deployment_name)
            && context.labels.get("jarvisctl.io/replicaset") == Some(&replica_set_name.to_string())
    }) {
        let _ = delete_runtime_session(&session);
    }
    delete_manifest_only(
        ResourceKind::ReplicaSet,
        replica_set_name,
        Some(control_namespace),
    )
}

fn apply_deployment_strategy(
    manifest: &ResourceEnvelope<DeploymentSpec>,
    target_replica_set_name: &str,
    replica_sets: &mut [ResourceEnvelope<ReplicaSetSpec>],
    sessions: &[NativeSessionMetadata],
) -> anyhow::Result<()> {
    let strategy = effective_deployment_strategy(manifest);
    match strategy.strategy_type {
        DeploymentStrategyType::Recreate => {
            scale_recreate_deployment(manifest, target_replica_set_name, replica_sets, sessions)
        }
        DeploymentStrategyType::RollingUpdate => scale_rolling_update_deployment(
            manifest,
            target_replica_set_name,
            replica_sets,
            sessions,
            strategy
                .rolling_update
                .unwrap_or_else(RollingUpdateDeployment::default),
        ),
    }
}

fn scale_recreate_deployment(
    manifest: &ResourceEnvelope<DeploymentSpec>,
    target_replica_set_name: &str,
    replica_sets: &mut [ResourceEnvelope<ReplicaSetSpec>],
    sessions: &[NativeSessionMetadata],
) -> anyhow::Result<()> {
    let mut old_live_sessions = false;
    for replica_set in replica_sets.iter_mut() {
        if replica_set.metadata.name == target_replica_set_name {
            continue;
        }
        if replica_set.spec.replicas != 0 {
            replica_set.spec.replicas = 0;
            save_manifest(&ResourceManifest::ReplicaSet(replica_set.clone()))?;
        }
        if !replica_set_live_session_names(replica_set, sessions).is_empty() {
            old_live_sessions = true;
        }
    }

    let target_replica_set = replica_sets
        .iter_mut()
        .find(|replica_set| replica_set.metadata.name == target_replica_set_name)
        .ok_or_else(|| anyhow!("missing target ReplicaSet '{}'", target_replica_set_name))?;
    let desired_replicas = if old_live_sessions {
        0
    } else {
        manifest.spec.replicas
    };
    if target_replica_set.spec.replicas != desired_replicas {
        target_replica_set.spec.replicas = desired_replicas;
        save_manifest(&ResourceManifest::ReplicaSet(target_replica_set.clone()))?;
    }
    Ok(())
}

fn scale_rolling_update_deployment(
    manifest: &ResourceEnvelope<DeploymentSpec>,
    target_replica_set_name: &str,
    replica_sets: &mut [ResourceEnvelope<ReplicaSetSpec>],
    sessions: &[NativeSessionMetadata],
    rolling_update: RollingUpdateDeployment,
) -> anyhow::Result<()> {
    let desired = manifest.spec.replicas;
    let max_unavailable = resolve_int_or_percent(&rolling_update.max_unavailable, desired, false)?;
    let max_surge = resolve_int_or_percent(&rolling_update.max_surge, desired, true)?;
    ensure!(
        max_unavailable > 0 || max_surge > 0,
        "rollingUpdate cannot set both maxUnavailable and maxSurge to 0"
    );

    let statuses = replica_sets
        .iter()
        .map(|replica_set| {
            Ok((
                replica_set.metadata.name.clone(),
                replica_set_status_with_sessions(replica_set, sessions)?,
            ))
        })
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;

    let mut total_spec: usize = replica_sets
        .iter()
        .map(|replica_set| replica_set.spec.replicas)
        .sum();
    let mut total_ready: usize = statuses.values().map(|status| status.ready_replicas).sum();
    let max_total = desired + max_surge;

    if let Some(target_replica_set) = replica_sets
        .iter_mut()
        .find(|replica_set| replica_set.metadata.name == target_replica_set_name)
    {
        let current_target = target_replica_set.spec.replicas;
        if current_target < desired {
            let can_add = max_total.saturating_sub(total_spec);
            let add = can_add.min(desired.saturating_sub(current_target));
            if add > 0 {
                target_replica_set.spec.replicas += add;
                total_spec += add;
                save_manifest(&ResourceManifest::ReplicaSet(target_replica_set.clone()))?;
            }
        }
    }

    let min_ready = desired.saturating_sub(max_unavailable);
    replica_sets.sort_by_key(|replica_set| replica_set.spec.revision);
    for replica_set in replica_sets.iter_mut() {
        if replica_set.metadata.name == target_replica_set_name || replica_set.spec.replicas == 0 {
            continue;
        }

        let Some(status) = statuses.get(&replica_set.metadata.name) else {
            continue;
        };
        let unready = replica_set
            .spec
            .replicas
            .saturating_sub(status.ready_replicas);
        let excess_ready = total_ready.saturating_sub(min_ready);
        let remove_ready = excess_ready.min(status.ready_replicas);
        let remove = (unready + remove_ready).min(replica_set.spec.replicas);
        if remove == 0 {
            continue;
        }

        let removed_unready = unready.min(remove);
        let removed_ready = remove.saturating_sub(removed_unready);
        replica_set.spec.replicas -= remove;
        total_spec = total_spec.saturating_sub(remove);
        total_ready = total_ready.saturating_sub(removed_ready);
        save_manifest(&ResourceManifest::ReplicaSet(replica_set.clone()))?;
    }

    if let Some(target_replica_set) = replica_sets
        .iter_mut()
        .find(|replica_set| replica_set.metadata.name == target_replica_set_name)
    {
        let current_target = target_replica_set.spec.replicas;
        if current_target < desired {
            let can_add = max_total.saturating_sub(total_spec);
            let add = can_add.min(desired.saturating_sub(current_target));
            if add > 0 {
                target_replica_set.spec.replicas += add;
                save_manifest(&ResourceManifest::ReplicaSet(target_replica_set.clone()))?;
            }
        }
    }

    Ok(())
}

fn reconcile_replica_set_runtime(
    replica_set: &ResourceEnvelope<ReplicaSetSpec>,
) -> anyhow::Result<()> {
    let desired_namespaces = replica_set_runtime_namespaces(
        replica_set.namespace_key(),
        &replica_set.spec.deployment_name,
        replica_set.spec.revision,
        replica_set.spec.replicas,
    );
    let desired_set: HashSet<String> = desired_namespaces.iter().cloned().collect();
    let current_sessions = collect_runtime_sessions()?;
    let managed_sessions = current_sessions
        .iter()
        .filter(|session| {
            session
                .context
                .as_ref()
                .and_then(|context| context.control_namespace.as_deref())
                == Some(replica_set.namespace_key())
                && session
                    .context
                    .as_ref()
                    .and_then(|context| context.deployment.as_deref())
                    == Some(replica_set.spec.deployment_name.as_str())
                && session
                    .context
                    .as_ref()
                    .and_then(|context| context.labels.get("jarvisctl.io/replicaset"))
                    == Some(&replica_set.metadata.name)
        })
        .cloned()
        .collect::<Vec<_>>();

    for session in &managed_sessions {
        if !desired_set.contains(&session.namespace) {
            delete_runtime_session(session)?;
        }
    }

    let mut existing_names = HashSet::new();
    for session in &managed_sessions {
        if !desired_set.contains(&session.namespace) {
            continue;
        }
        if session.agents.iter().any(|agent| agent.running) {
            existing_names.insert(session.namespace.clone());
        } else {
            delete_runtime_session(session)?;
        }
    }

    for (ordinal, runtime_namespace) in desired_namespaces.iter().enumerate() {
        if existing_names.contains(runtime_namespace) {
            continue;
        }
        launch_replica_set_replica(replica_set, ordinal, runtime_namespace)?;
    }

    Ok(())
}

fn replica_set_live_session_names(
    replica_set: &ResourceEnvelope<ReplicaSetSpec>,
    sessions: &[NativeSessionMetadata],
) -> Vec<String> {
    sessions
        .iter()
        .filter(|session| {
            session
                .context
                .as_ref()
                .and_then(|context| context.control_namespace.as_deref())
                == Some(replica_set.namespace_key())
                && session
                    .context
                    .as_ref()
                    .and_then(|context| context.deployment.as_deref())
                    == Some(replica_set.spec.deployment_name.as_str())
                && session
                    .context
                    .as_ref()
                    .and_then(|context| context.labels.get("jarvisctl.io/replicaset"))
                    == Some(&replica_set.metadata.name)
        })
        .map(|session| session.namespace.clone())
        .collect()
}

fn launch_replica_set_replica(
    replica_set: &ResourceEnvelope<ReplicaSetSpec>,
    ordinal: usize,
    runtime_namespace: &str,
) -> anyhow::Result<()> {
    let control_namespace = replica_set.namespace_key().to_string();
    let driver = replica_set
        .spec
        .driver
        .unwrap_or(CodexRuntimeDriver::AppServer);
    let startup_delay_ms = replica_set.spec.startup_delay_ms.unwrap_or(1500);
    let working_directory = replica_set
        .spec
        .template
        .working_directory
        .clone()
        .map(PathBuf::from);

    let config_maps =
        load_config_map_values(&control_namespace, &replica_set.spec.template.config_maps)?;
    let secrets = load_secret_values(&control_namespace, &replica_set.spec.template.secrets)?;
    let volumes = load_volume_paths(&control_namespace, &replica_set.spec.template.volumes)?;
    let service_environment = service_discovery_environment(&control_namespace)?;
    let environment = merged_environment(
        &config_maps,
        &secrets,
        &service_environment,
        &deployment_runtime_environment(
            &control_namespace,
            &replica_set.spec.deployment_name,
            &replica_set.metadata.name,
            replica_set.spec.revision,
            runtime_namespace,
            ordinal,
        ),
    );
    let context_overlay = RuntimeContextMetadata {
        control_namespace: Some(control_namespace.clone()),
        deployment: Some(replica_set.spec.deployment_name.clone()),
        labels: replica_set_runtime_labels(replica_set, ordinal),
        config_maps: replica_set.spec.template.config_maps.clone(),
        secrets: replica_set.spec.template.secrets.clone(),
        volumes: replica_set.spec.template.volumes.clone(),
        ..RuntimeContextMetadata::default()
    };
    let operator_message = build_control_plane_operator_message(
        replica_set,
        ordinal,
        &config_maps,
        &secrets,
        &volumes,
        &service_environment,
    );
    let images = replica_set
        .spec
        .template
        .images
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();

    launch_codex_ticket(CodexLaunchOptions {
        backend: SessionBackend::Native,
        driver,
        task_note: PathBuf::from(&replica_set.spec.template.task_note),
        namespace: Some(runtime_namespace.to_string()),
        agents: replica_set.spec.agents,
        agent: "agent0".to_string(),
        fresh_session: true,
        resume_session_id: None,
        working_directory,
        prompt_file: None,
        operator_message: operator_message
            .or_else(|| replica_set.spec.template.operator_message.clone()),
        images,
        environment,
        context_overlay,
        extra_runtime_args: volume_runtime_args(&volumes),
        startup_delay_ms,
        command: replica_set.spec.template.command.clone(),
    })?;
    Ok(())
}

fn build_control_plane_operator_message(
    manifest: &ResourceEnvelope<ReplicaSetSpec>,
    ordinal: usize,
    config_maps: &BTreeMap<String, BTreeMap<String, String>>,
    secrets: &BTreeMap<String, BTreeMap<String, String>>,
    volumes: &[String],
    service_environment: &BTreeMap<String, String>,
) -> Option<String> {
    let mut blocks = Vec::new();
    if let Some(message) = manifest.spec.template.operator_message.as_deref() {
        let trimmed = message.trim();
        if !trimmed.is_empty() {
            blocks.push(trimmed.to_string());
        }
    }

    let mut lines = vec![
        "Control plane contract:".to_string(),
        format!("- Control namespace: {}", manifest.namespace_key()),
        format!("- Deployment: {}", manifest.spec.deployment_name),
        format!("- ReplicaSet: {}", manifest.metadata.name),
        format!("- Revision: {}", manifest.spec.revision),
        format!("- Replica ordinal: {}", ordinal),
    ];
    if !config_maps.is_empty() {
        lines.push("- ConfigMaps:".to_string());
        for (name, data) in config_maps {
            lines.push(format!("  - {}:", name));
            for (key, value) in data {
                lines.push(format!("    - {}={}", key, value));
            }
        }
    }
    if !secrets.is_empty() {
        lines.push("- Secrets available as environment variables:".to_string());
        for (name, data) in secrets {
            let keys = data.keys().cloned().collect::<Vec<_>>().join(", ");
            lines.push(format!("  - {} -> {}", name, keys));
        }
    }
    if !volumes.is_empty() {
        lines.push("- Accessible volumes:".to_string());
        for volume in volumes {
            lines.push(format!("  - {}", volume));
        }
    }
    let service_targets = service_environment
        .iter()
        .filter_map(|(key, value)| {
            key.strip_suffix("_TARGET")
                .filter(|_| key.starts_with("JARVIS_SERVICE_"))
                .map(|_| value.as_str())
        })
        .collect::<Vec<_>>();
    if !service_targets.is_empty() {
        lines.push("- Discoverable services:".to_string());
        for target in service_targets {
            lines.push(format!("  - {}", target));
        }
    }
    blocks.push(lines.join("\n"));
    Some(blocks.join("\n\n"))
}

fn volume_runtime_args(volumes: &[String]) -> Vec<String> {
    let mut args = Vec::new();
    for path in volumes {
        args.push("--add-dir".to_string());
        args.push(path.clone());
    }
    args
}

fn resolved_deployment_template(
    manifest: &ResourceEnvelope<DeploymentSpec>,
    namespace_defaults: &NamespaceSpec,
) -> DeploymentTemplateSpec {
    let mut template = manifest.spec.template.clone();
    if template.working_directory.is_none() {
        template.working_directory = namespace_defaults.default_working_directory.clone();
    }
    template
}

fn deployment_template_hash(
    manifest: &ResourceEnvelope<DeploymentSpec>,
    namespace_defaults: &NamespaceSpec,
) -> anyhow::Result<String> {
    let identity = json!({
        "agents": manifest.spec.agents,
        "driver": manifest
            .spec
            .driver
            .or(namespace_defaults.default_driver)
            .unwrap_or(CodexRuntimeDriver::AppServer),
        "startupDelayMs": manifest
            .spec
            .startup_delay_ms
            .or(namespace_defaults.default_startup_delay_ms)
            .unwrap_or(1500),
        "template": resolved_deployment_template(manifest, namespace_defaults),
        "restartToken": manifest.spec.restart_token,
    });
    hash_json_value(&identity)
}

fn hash_json_value(value: &serde_json::Value) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(value).context("failed to encode revision identity")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn deployment_strategy_summary(
    manifest: &ResourceEnvelope<DeploymentSpec>,
) -> anyhow::Result<String> {
    let strategy = effective_deployment_strategy(manifest);
    Ok(match strategy.strategy_type {
        DeploymentStrategyType::Recreate => "Recreate".to_string(),
        DeploymentStrategyType::RollingUpdate => {
            let rolling_update = strategy
                .rolling_update
                .unwrap_or_else(RollingUpdateDeployment::default);
            format!(
                "RollingUpdate(maxUnavailable={}, maxSurge={})",
                resolve_int_or_percent(
                    &rolling_update.max_unavailable,
                    manifest.spec.replicas,
                    false
                )?,
                resolve_int_or_percent(&rolling_update.max_surge, manifest.spec.replicas, true)?
            )
        }
    })
}

fn effective_deployment_strategy(
    manifest: &ResourceEnvelope<DeploymentSpec>,
) -> DeploymentStrategy {
    manifest.spec.strategy.clone().unwrap_or_default()
}

fn deployment_replica_set_name(deployment_name: &str, revision: u64) -> String {
    format!("{}-rs-{:04}", slugify(deployment_name), revision)
}

fn replica_set_runtime_namespaces(
    control_namespace: &str,
    deployment_name: &str,
    revision: u64,
    replicas: usize,
) -> Vec<String> {
    (0..replicas)
        .map(|ordinal| {
            format!(
                "{}--{}--rev{}--r{}",
                slugify(control_namespace),
                slugify(deployment_name),
                revision,
                ordinal
            )
        })
        .collect()
}

fn replica_set_runtime_labels(
    replica_set: &ResourceEnvelope<ReplicaSetSpec>,
    ordinal: usize,
) -> BTreeMap<String, String> {
    let mut labels = replica_set.metadata.labels.clone();
    labels.extend(replica_set.spec.template.labels.clone());
    labels.insert(
        "jarvisctl.io/control-namespace".to_string(),
        replica_set.namespace_key().to_string(),
    );
    labels.insert(
        "jarvisctl.io/deployment".to_string(),
        replica_set.spec.deployment_name.clone(),
    );
    labels.insert(
        "jarvisctl.io/replicaset".to_string(),
        replica_set.metadata.name.clone(),
    );
    labels.insert(
        "jarvisctl.io/revision".to_string(),
        replica_set.spec.revision.to_string(),
    );
    labels.insert(
        "jarvisctl.io/replica-ordinal".to_string(),
        ordinal.to_string(),
    );
    labels
}

fn deployment_runtime_environment(
    control_namespace: &str,
    deployment_name: &str,
    replica_set_name: &str,
    revision: u64,
    runtime_namespace: &str,
    ordinal: usize,
) -> BTreeMap<String, String> {
    let mut environment = runtime_identity_environment(
        control_namespace,
        deployment_name,
        runtime_namespace,
        ordinal,
    );
    environment.insert(
        "JARVIS_REPLICA_SET".to_string(),
        replica_set_name.to_string(),
    );
    environment.insert(
        "JARVIS_DEPLOYMENT_REVISION".to_string(),
        revision.to_string(),
    );
    environment
}

fn reconcile_job(manifest: &ResourceEnvelope<JobSpec>) -> anyhow::Result<String> {
    let mut state = refreshed_job_state(manifest)?;
    let mut status = job_status_from_state(manifest, &state);

    if !manifest.spec.suspend
        && status.succeeded < manifest.spec.completions
        && status.failed <= manifest.spec.backoff_limit
    {
        let desired_active = manifest
            .spec
            .parallelism
            .min(manifest.spec.completions.saturating_sub(status.succeeded));
        while status.active < desired_active && status.failed <= manifest.spec.backoff_limit {
            let run_index = state.runs.len();
            let runtime_namespace =
                job_runtime_namespace(manifest.namespace_key(), &manifest.metadata.name, run_index);
            launch_job_run(manifest, run_index, &runtime_namespace)?;
            state.runs.push(JobRunState {
                name: format!("run-{}", run_index),
                runtime_namespace,
                created_at_epoch_ms: now_epoch_ms(),
                last_active_epoch_ms: Some(now_epoch_ms()),
                status: JobRunPhase::Active,
            });
            status = job_status_from_state(manifest, &state);
        }
    }

    save_job_controller_state(manifest.namespace_key(), &manifest.metadata.name, &state)?;
    let status = job_status_from_state(manifest, &state);
    Ok(format!(
        "applied job {}/{} (active {}, succeeded {}/{}, failed {})",
        manifest.namespace_key(),
        manifest.metadata.name,
        status.active,
        status.succeeded,
        status.completions,
        status.failed
    ))
}

fn launch_job_run(
    manifest: &ResourceEnvelope<JobSpec>,
    run_index: usize,
    runtime_namespace: &str,
) -> anyhow::Result<()> {
    let control_namespace = manifest.namespace_key().to_string();
    let namespace_defaults = load_namespace_defaults(&control_namespace)?;
    let driver = manifest
        .spec
        .driver
        .or(namespace_defaults.default_driver)
        .unwrap_or(CodexRuntimeDriver::AppServer);
    let startup_delay_ms = manifest
        .spec
        .startup_delay_ms
        .or(namespace_defaults.default_startup_delay_ms)
        .unwrap_or(1500);
    let working_directory = manifest
        .spec
        .template
        .working_directory
        .clone()
        .or(namespace_defaults.default_working_directory)
        .map(PathBuf::from);

    let config_maps =
        load_config_map_values(&control_namespace, &manifest.spec.template.config_maps)?;
    let secrets = load_secret_values(&control_namespace, &manifest.spec.template.secrets)?;
    let volumes = load_volume_paths(&control_namespace, &manifest.spec.template.volumes)?;
    let service_environment = service_discovery_environment(&control_namespace)?;
    let environment = merged_environment(
        &config_maps,
        &secrets,
        &service_environment,
        &job_runtime_identity_environment(
            &control_namespace,
            &manifest.metadata.name,
            runtime_namespace,
            run_index,
        ),
    );
    let context_overlay = RuntimeContextMetadata {
        control_namespace: Some(control_namespace.clone()),
        labels: job_labels(manifest, run_index),
        config_maps: manifest.spec.template.config_maps.clone(),
        secrets: manifest.spec.template.secrets.clone(),
        volumes: manifest.spec.template.volumes.clone(),
        ..RuntimeContextMetadata::default()
    };
    let operator_message = build_job_operator_message(
        manifest,
        run_index,
        &config_maps,
        &secrets,
        &volumes,
        &service_environment,
    );
    let images = manifest
        .spec
        .template
        .images
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();

    if let Err(error) = launch_codex_ticket(CodexLaunchOptions {
        backend: SessionBackend::Native,
        driver,
        task_note: PathBuf::from(&manifest.spec.template.task_note),
        namespace: Some(runtime_namespace.to_string()),
        agents: manifest.spec.agents,
        agent: "agent0".to_string(),
        fresh_session: true,
        resume_session_id: None,
        working_directory,
        prompt_file: None,
        operator_message: operator_message
            .or_else(|| manifest.spec.template.operator_message.clone()),
        images,
        environment,
        context_overlay,
        extra_runtime_args: volume_runtime_args(&volumes),
        startup_delay_ms,
        command: manifest.spec.template.command.clone(),
    }) {
        if native_session_completion(runtime_namespace)?.is_some() {
            return Ok(());
        }
        return Err(error);
    }
    Ok(())
}

fn build_job_operator_message(
    manifest: &ResourceEnvelope<JobSpec>,
    run_index: usize,
    config_maps: &BTreeMap<String, BTreeMap<String, String>>,
    secrets: &BTreeMap<String, BTreeMap<String, String>>,
    volumes: &[String],
    service_environment: &BTreeMap<String, String>,
) -> Option<String> {
    let mut blocks = Vec::new();
    if let Some(message) = manifest.spec.template.operator_message.as_deref() {
        let trimmed = message.trim();
        if !trimmed.is_empty() {
            blocks.push(trimmed.to_string());
        }
    }

    let mut lines = vec![
        "Control plane contract:".to_string(),
        format!("- Control namespace: {}", manifest.namespace_key()),
        format!("- Job: {}", manifest.metadata.name),
        format!("- Job run: {}", run_index),
    ];
    if !config_maps.is_empty() {
        lines.push("- ConfigMaps:".to_string());
        for (name, data) in config_maps {
            lines.push(format!("  - {}:", name));
            for (key, value) in data {
                lines.push(format!("    - {}={}", key, value));
            }
        }
    }
    if !secrets.is_empty() {
        lines.push("- Secrets available as environment variables:".to_string());
        for (name, data) in secrets {
            let keys = data.keys().cloned().collect::<Vec<_>>().join(", ");
            lines.push(format!("  - {} -> {}", name, keys));
        }
    }
    if !volumes.is_empty() {
        lines.push("- Accessible volumes:".to_string());
        for volume in volumes {
            lines.push(format!("  - {}", volume));
        }
    }
    let service_targets = service_environment
        .iter()
        .filter_map(|(key, value)| {
            key.strip_suffix("_TARGET")
                .filter(|_| key.starts_with("JARVIS_SERVICE_"))
                .map(|_| value.as_str())
        })
        .collect::<Vec<_>>();
    if !service_targets.is_empty() {
        lines.push("- Discoverable services:".to_string());
        for target in service_targets {
            lines.push(format!("  - {}", target));
        }
    }
    blocks.push(lines.join("\n"));
    Some(blocks.join("\n\n"))
}

fn job_runtime_identity_environment(
    control_namespace: &str,
    job_name: &str,
    runtime_namespace: &str,
    run_index: usize,
) -> BTreeMap<String, String> {
    let mut environment =
        runtime_identity_environment(control_namespace, job_name, runtime_namespace, run_index);
    environment.insert("JARVIS_JOB".to_string(), job_name.to_string());
    environment.insert("JARVIS_JOB_RUN".to_string(), run_index.to_string());
    environment
}

fn job_labels(manifest: &ResourceEnvelope<JobSpec>, run_index: usize) -> BTreeMap<String, String> {
    let mut labels = manifest.metadata.labels.clone();
    labels.extend(manifest.spec.template.labels.clone());
    labels.insert(
        "jarvisctl.io/control-namespace".to_string(),
        manifest.namespace_key().to_string(),
    );
    labels.insert(
        "jarvisctl.io/job".to_string(),
        manifest.metadata.name.clone(),
    );
    labels.insert("jarvisctl.io/job-run".to_string(), run_index.to_string());
    labels
}

fn job_runtime_namespace(control_namespace: &str, job_name: &str, run_index: usize) -> String {
    format!(
        "{}--{}--j{}",
        slugify(control_namespace),
        slugify(job_name),
        run_index
    )
}

fn job_controller_state_path(control_namespace: &str, job_name: &str) -> anyhow::Result<PathBuf> {
    Ok(control_plane_root()?
        .join("state")
        .join("jobs")
        .join(control_namespace)
        .join(format!("{}.json", slugify(job_name))))
}

fn load_job_controller_state(
    control_namespace: &str,
    job_name: &str,
) -> anyhow::Result<JobControllerState> {
    let path = job_controller_state_path(control_namespace, job_name)?;
    if !path.exists() {
        return Ok(JobControllerState::default());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse '{}'", path.display()))
}

fn save_job_controller_state(
    control_namespace: &str,
    job_name: &str,
    state: &JobControllerState,
) -> anyhow::Result<()> {
    let path = job_controller_state_path(control_namespace, job_name)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    let raw =
        serde_json::to_string_pretty(state).context("failed to encode job controller state")?;
    fs::write(&path, raw).with_context(|| format!("failed to write '{}'", path.display()))
}

fn reconcile_cron_job(manifest: &ResourceEnvelope<CronJobSpec>) -> anyhow::Result<String> {
    let control_namespace = manifest.namespace_key().to_string();
    let mut state = load_cron_job_controller_state(&control_namespace, &manifest.metadata.name)?;
    let schedule = parse_kubernetes_cron_schedule(&manifest.spec.schedule).with_context(|| {
        format!(
            "failed to parse schedule '{}' for CronJob '{}/{}'",
            manifest.spec.schedule, control_namespace, manifest.metadata.name
        )
    })?;

    if !manifest.spec.suspend {
        let now = Utc::now();
        let last_schedule = state
            .last_schedule_epoch_ms
            .and_then(|last_epoch_ms| {
                chrono::DateTime::<Utc>::from_timestamp_millis(last_epoch_ms as i64)
            })
            .unwrap_or_else(|| now - chrono::TimeDelta::minutes(1));
        let mut due_times = schedule.after(&last_schedule).take(16).collect::<Vec<_>>();
        due_times.retain(|scheduled_at| *scheduled_at <= now);
        due_times.sort();
        for scheduled_at in due_times {
            if !cron_job_allows_new_run(manifest, &state)? {
                break;
            }
            let job_name = cron_job_run_name(&manifest.metadata.name, scheduled_at.timestamp());
            if load_manifest(
                ResourceKind::Job,
                &job_name,
                manifest.metadata.namespace.as_deref(),
            )
            .is_ok()
            {
                state.last_schedule_epoch_ms = Some(scheduled_at.timestamp_millis() as u128);
                continue;
            }
            let job_manifest = cron_job_to_job(manifest, &job_name);
            save_manifest(&ResourceManifest::Job(job_manifest.clone()))?;
            state.jobs.push(job_name.clone());
            state.last_schedule_epoch_ms = Some(scheduled_at.timestamp_millis() as u128);
        }
    }

    prune_cron_job_history(manifest, &mut state)?;
    save_cron_job_controller_state(&control_namespace, &manifest.metadata.name, &state)?;
    let status = cron_job_status(manifest)?;
    Ok(format!(
        "applied cronjob {}/{} ({} active jobs)",
        control_namespace,
        manifest.metadata.name,
        status.active_jobs.len()
    ))
}

fn cron_job_allows_new_run(
    manifest: &ResourceEnvelope<CronJobSpec>,
    state: &CronJobControllerState,
) -> anyhow::Result<bool> {
    let active_jobs = state
        .jobs
        .iter()
        .filter(|job_name| {
            load_manifest(
                ResourceKind::Job,
                job_name,
                manifest.metadata.namespace.as_deref(),
            )
            .ok()
            .and_then(|resource| match resource {
                ResourceManifest::Job(job) => job_status(&job).ok(),
                _ => None,
            })
            .map(|status| status.active > 0)
            .unwrap_or(false)
        })
        .cloned()
        .collect::<Vec<_>>();

    match manifest.spec.concurrency_policy {
        CronJobConcurrencyPolicy::Allow => Ok(true),
        CronJobConcurrencyPolicy::Forbid => Ok(active_jobs.is_empty()),
        CronJobConcurrencyPolicy::Replace => {
            for job_name in active_jobs {
                delete_job_resources(manifest.namespace_key(), &job_name)?;
            }
            Ok(true)
        }
    }
}

fn cron_job_to_job(
    manifest: &ResourceEnvelope<CronJobSpec>,
    job_name: &str,
) -> ResourceEnvelope<JobSpec> {
    let mut metadata = manifest.metadata.clone();
    metadata.name = job_name.to_string();
    metadata.labels.insert(
        "jarvisctl.io/cronjob".to_string(),
        manifest.metadata.name.clone(),
    );
    ResourceEnvelope {
        api_version: API_VERSION.to_string(),
        kind: "Job".to_string(),
        metadata,
        spec: manifest.spec.job_template.spec.clone(),
    }
}

fn cron_job_run_name(cron_job_name: &str, timestamp_secs: i64) -> String {
    format!("{}-{}", slugify(cron_job_name), timestamp_secs)
}

fn prune_cron_job_history(
    manifest: &ResourceEnvelope<CronJobSpec>,
    state: &mut CronJobControllerState,
) -> anyhow::Result<()> {
    let mut succeeded = Vec::new();
    let mut failed = Vec::new();
    let mut retained = Vec::new();

    for job_name in &state.jobs {
        let Ok(ResourceManifest::Job(job)) = load_manifest(
            ResourceKind::Job,
            job_name,
            manifest.metadata.namespace.as_deref(),
        ) else {
            continue;
        };
        let status = job_status(&job)?;
        if status.active > 0 {
            retained.push(job_name.clone());
            continue;
        }
        if status.succeeded >= status.completions {
            succeeded.push(job_name.clone());
        } else {
            failed.push(job_name.clone());
        }
    }

    succeeded.sort();
    failed.sort();

    while succeeded.len() > manifest.spec.successful_jobs_history_limit {
        if let Some(job_name) = succeeded.first().cloned() {
            delete_job_resources(manifest.namespace_key(), &job_name)?;
            succeeded.remove(0);
        }
    }
    while failed.len() > manifest.spec.failed_jobs_history_limit {
        if let Some(job_name) = failed.first().cloned() {
            delete_job_resources(manifest.namespace_key(), &job_name)?;
            failed.remove(0);
        }
    }

    retained.extend(succeeded);
    retained.extend(failed);
    retained.sort();
    retained.dedup();
    state.jobs = retained;
    Ok(())
}

fn cron_job_controller_state_path(
    control_namespace: &str,
    cron_job_name: &str,
) -> anyhow::Result<PathBuf> {
    Ok(control_plane_root()?
        .join("state")
        .join("cronjobs")
        .join(control_namespace)
        .join(format!("{}.json", slugify(cron_job_name))))
}

fn load_cron_job_controller_state(
    control_namespace: &str,
    cron_job_name: &str,
) -> anyhow::Result<CronJobControllerState> {
    let path = cron_job_controller_state_path(control_namespace, cron_job_name)?;
    if !path.exists() {
        return Ok(CronJobControllerState::default());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse '{}'", path.display()))
}

fn save_cron_job_controller_state(
    control_namespace: &str,
    cron_job_name: &str,
    state: &CronJobControllerState,
) -> anyhow::Result<()> {
    let path = cron_job_controller_state_path(control_namespace, cron_job_name)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    let raw =
        serde_json::to_string_pretty(state).context("failed to encode cronjob controller state")?;
    fs::write(&path, raw).with_context(|| format!("failed to write '{}'", path.display()))
}

fn parse_kubernetes_cron_schedule(raw: &str) -> anyhow::Result<Schedule> {
    let trimmed = raw.trim();
    let normalized = match trimmed {
        "@yearly" | "@annually" => "0 0 0 1 1 *",
        "@monthly" => "0 0 0 1 * *",
        "@weekly" => "0 0 0 * * 0",
        "@daily" | "@midnight" => "0 0 0 * * *",
        "@hourly" => "0 0 * * * *",
        _ => {
            let fields = trimmed.split_whitespace().collect::<Vec<_>>();
            match fields.len() {
                5 => {
                    return Schedule::from_str(&format!("0 {}", trimmed))
                        .context("invalid 5-field cron schedule");
                }
                6 | 7 => {
                    return Schedule::from_str(trimmed).context("invalid cron schedule");
                }
                _ => bail!("expected a 5-field Kubernetes cron schedule or supported macro"),
            }
        }
    };
    Schedule::from_str(normalized).context("invalid cron macro")
}

fn reconcile_application(manifest: &ResourceEnvelope<ApplicationSpec>) -> anyhow::Result<String> {
    sync_application(manifest, false)
}

fn sync_application(
    manifest: &ResourceEnvelope<ApplicationSpec>,
    force: bool,
) -> anyhow::Result<String> {
    let control_namespace = manifest.namespace_key().to_string();
    let mut state = load_application_controller_state(&control_namespace, &manifest.metadata.name)?;
    if !force && !application_sync_enabled(manifest) {
        return Ok(format!(
            "application {}/{} sync disabled",
            control_namespace, manifest.metadata.name
        ));
    }

    let desired = build_application_desired_state(manifest)?;

    if application_prune_enabled(manifest) {
        for old_ref in &state.rendered_resources {
            if !desired.rendered_resources.contains(old_ref) {
                delete_rendered_resource(old_ref)?;
            }
        }
    }

    for rendered_manifest in &desired.rendered {
        save_manifest(rendered_manifest)?;
    }

    let synced_at_epoch_ms = now_epoch_ms();
    state.last_sync_epoch_ms = Some(synced_at_epoch_ms);
    state.rendered_resources = desired.rendered_resources.into_iter().collect();
    state.last_attempted_revision = Some(desired.resolved_revision.clone());
    state.last_applied_revision = Some(desired.resolved_revision.clone());
    if state.history.last().map(|entry| entry.revision.as_str())
        != Some(desired.resolved_revision.as_str())
    {
        state.history.push(ApplicationSyncHistoryEntry {
            revision: desired.resolved_revision.clone(),
            synced_at_epoch_ms,
            rendered_resources: state.rendered_resources.len(),
            source_path: manifest.spec.source.path.clone(),
        });
        if state.history.len() > default_application_history_limit() {
            let remove_count = state.history.len() - default_application_history_limit();
            state.history.drain(0..remove_count);
        }
    }
    save_application_controller_state(&control_namespace, &manifest.metadata.name, &state)?;

    Ok(format!(
        "synced application {}/{} to {} from {}{} ({} resources)",
        control_namespace,
        manifest.metadata.name,
        short_revision(&desired.resolved_revision),
        short_revision(&desired.source.source_revision),
        if desired.source.source_dirty {
            " dirty"
        } else {
            ""
        },
        state.rendered_resources.len()
    ))
}

fn application_resolved_revision(
    manifest: &ResourceEnvelope<ApplicationSpec>,
    source: &ApplicationSourceResolution,
    rendered: &[ResourceManifest],
) -> anyhow::Result<String> {
    let manifests = rendered
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .context("failed to encode rendered manifests for application revision")?;
    let value = json!({
        "sourcePath": manifest.spec.source.path,
        "repoURL": manifest.spec.source.repo_url,
        "sourceType": source.source_type,
        "sourceRoot": source.source_root,
        "sourceRevision": source.source_revision,
        "sourceDirty": source.source_dirty,
        "targetRevision": manifest
            .spec
            .source
            .target_revision
            .clone()
            .unwrap_or_else(|| "HEAD".to_string()),
        "destinationNamespace": effective_application_destination_namespace(manifest),
        "manifests": manifests,
    });
    hash_json_value(&value)
}

fn build_application_desired_state(
    manifest: &ResourceEnvelope<ApplicationSpec>,
) -> anyhow::Result<ApplicationDesiredState> {
    let initial_source = application_source_resolution(manifest, &[])?;
    let rendered = render_source_path(
        &initial_source.render_path,
        effective_application_destination_namespace(manifest).as_deref(),
        &application_management_labels(manifest),
    )?;
    let source = application_source_resolution(manifest, &rendered)?;
    let rendered_resources = rendered
        .iter()
        .map(RenderedResourceRef::from_manifest)
        .collect::<BTreeSet<_>>();
    let resolved_revision = application_resolved_revision(manifest, &source, &rendered)?;
    Ok(ApplicationDesiredState {
        source,
        rendered,
        rendered_resources,
        resolved_revision,
    })
}

fn application_target_revision(manifest: &ResourceEnvelope<ApplicationSpec>) -> String {
    manifest
        .spec
        .source
        .target_revision
        .clone()
        .unwrap_or_else(|| "HEAD".to_string())
}

fn application_source_resolution(
    manifest: &ResourceEnvelope<ApplicationSpec>,
    rendered: &[ResourceManifest],
) -> anyhow::Result<ApplicationSourceResolution> {
    if let Some(repo_url) = manifest.spec.source.repo_url.as_deref() {
        let source_root = prepare_remote_application_source(manifest)?;
        let render_path = source_root.join(&manifest.spec.source.path);
        ensure!(
            render_path.exists(),
            "Application '{}/{}' path '{}' does not exist in repository '{}'",
            manifest.namespace_key(),
            manifest.metadata.name,
            manifest.spec.source.path,
            repo_url
        );
        let source_revision = git_resolve_revision(&source_root, "HEAD")?;
        return Ok(ApplicationSourceResolution {
            repo_url: Some(repo_url.to_string()),
            source_type: "git_remote".to_string(),
            source_root: Some(source_root.display().to_string()),
            source_revision,
            source_dirty: false,
            render_path,
        });
    }

    let render_path = resolve_application_source_path(&manifest.spec.source.path)?;
    if let Some(source_root) = git_repo_root_containing(&render_path)? {
        return Ok(ApplicationSourceResolution {
            repo_url: None,
            source_type: "git".to_string(),
            source_root: Some(source_root.display().to_string()),
            source_revision: git_resolve_revision(
                &source_root,
                &application_target_revision(manifest),
            )?,
            source_dirty: git_worktree_dirty(&source_root)?,
            render_path,
        });
    }

    let manifests = rendered
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .context("failed to encode rendered manifests for application source revision")?;
    Ok(ApplicationSourceResolution {
        repo_url: None,
        source_type: "path".to_string(),
        source_root: None,
        source_revision: hash_json_value(&json!({
            "sourcePath": manifest.spec.source.path,
            "repoURL": manifest.spec.source.repo_url,
            "targetRevision": application_target_revision(manifest),
            "manifests": manifests,
        }))?,
        source_dirty: false,
        render_path,
    })
}

fn resolve_application_source_path(raw: &str) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()
            .context("failed to resolve current directory for application source path")?
            .join(path))
    }
}

fn prepare_remote_application_source(
    manifest: &ResourceEnvelope<ApplicationSpec>,
) -> anyhow::Result<PathBuf> {
    let repo_url = manifest.spec.source.repo_url.as_deref().ok_or_else(|| {
        anyhow!(
            "Application '{}/{}' is missing spec.source.repoURL",
            manifest.namespace_key(),
            manifest.metadata.name
        )
    })?;
    let checkout_root = application_repo_checkout_path(manifest)?;
    if !checkout_root.join(".git").exists() {
        if checkout_root.exists() {
            let _ = fs::remove_dir_all(&checkout_root);
        }
        let parent = checkout_root.parent().ok_or_else(|| {
            anyhow!(
                "failed to resolve cache parent for Application '{}/{}'",
                manifest.namespace_key(),
                manifest.metadata.name
            )
        })?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
        git_run(
            None,
            &[
                "clone",
                repo_url,
                checkout_root
                    .to_str()
                    .ok_or_else(|| anyhow!("non-utf8 cache path"))?,
            ],
        )?;
    } else if let Ok(origin_url) =
        git_capture_stdout(&checkout_root, &["remote", "get-url", "origin"])
    {
        if origin_url.trim() != repo_url.trim() {
            let _ = fs::remove_dir_all(&checkout_root);
            let parent = checkout_root
                .parent()
                .ok_or_else(|| anyhow!("missing cache parent"))?;
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create '{}'", parent.display()))?;
            git_run(
                None,
                &[
                    "clone",
                    repo_url,
                    checkout_root
                        .to_str()
                        .ok_or_else(|| anyhow!("non-utf8 cache path"))?,
                ],
            )?;
        }
    }

    git_run(
        Some(&checkout_root),
        &["fetch", "--all", "--tags", "--prune"],
    )?;
    let checkout_target = if application_target_revision(manifest).eq_ignore_ascii_case("HEAD") {
        "origin/HEAD".to_string()
    } else {
        application_target_revision(manifest)
    };
    git_run(
        Some(&checkout_root),
        &["checkout", "--detach", "--force", &checkout_target],
    )?;
    Ok(checkout_root)
}

fn application_repo_checkout_path(
    manifest: &ResourceEnvelope<ApplicationSpec>,
) -> anyhow::Result<PathBuf> {
    Ok(control_plane_root()?
        .join("cache")
        .join("applications")
        .join(manifest.namespace_key())
        .join(slugify(&manifest.metadata.name))
        .join("repo"))
}

fn git_repo_root_containing(path: &Path) -> anyhow::Result<Option<PathBuf>> {
    let workdir = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .with_context(|| format!("failed to run git for '{}'", workdir.display()))?;
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8(output.stdout).context("git returned non-utf8 repo root")?;
    let root = stdout.trim();
    if root.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(root)))
}

fn git_run(repo_root: Option<&Path>, args: &[&str]) -> anyhow::Result<()> {
    let mut command = ProcessCommand::new("git");
    if let Some(repo_root) = repo_root {
        command.arg("-C").arg(repo_root);
    }
    let output = command.args(args).output().with_context(|| {
        let repo_display = repo_root
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<cwd>".to_string());
        format!("failed to run git {:?} in '{}'", args, repo_display)
    })?;
    ensure!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn git_capture_stdout(repo_root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {:?} in '{}'", args, repo_root.display()))?;
    ensure!(
        output.status.success(),
        "git {:?} failed in '{}': {}",
        args,
        repo_root.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)
        .context("git returned non-utf8 stdout")?
        .trim()
        .to_string())
}

fn git_resolve_revision(repo_root: &Path, target_revision: &str) -> anyhow::Result<String> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", &format!("{}^{{commit}}", target_revision)])
        .output()
        .with_context(|| {
            format!(
                "failed to resolve git revision in '{}'",
                repo_root.display()
            )
        })?;
    ensure!(
        output.status.success(),
        "failed to resolve git revision '{}' in '{}': {}",
        target_revision,
        repo_root.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)
        .context("git returned non-utf8 revision")?
        .trim()
        .to_string())
}

fn git_worktree_dirty(repo_root: &Path) -> anyhow::Result<bool> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["status", "--porcelain"])
        .output()
        .with_context(|| format!("failed to inspect git status in '{}'", repo_root.display()))?;
    ensure!(
        output.status.success(),
        "failed to inspect git worktree in '{}': {}",
        repo_root.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(!String::from_utf8(output.stdout)
        .context("git returned non-utf8 status")?
        .trim()
        .is_empty())
}

fn short_revision(revision: &str) -> String {
    revision.chars().take(12).collect()
}

fn application_sync_enabled(manifest: &ResourceEnvelope<ApplicationSpec>) -> bool {
    manifest
        .spec
        .sync_policy
        .automated
        .as_ref()
        .map(|automated| automated.enable)
        .unwrap_or(true)
}

fn application_prune_enabled(manifest: &ResourceEnvelope<ApplicationSpec>) -> bool {
    manifest
        .spec
        .sync_policy
        .automated
        .as_ref()
        .map(|automated| automated.prune)
        .unwrap_or(true)
}

fn effective_application_destination_namespace(
    manifest: &ResourceEnvelope<ApplicationSpec>,
) -> Option<String> {
    manifest
        .spec
        .destination
        .namespace
        .clone()
        .or_else(|| manifest.metadata.namespace.clone())
}

fn application_management_labels(
    manifest: &ResourceEnvelope<ApplicationSpec>,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "jarvisctl.io/application".to_string(),
            manifest.metadata.name.clone(),
        ),
        (
            "jarvisctl.io/application-namespace".to_string(),
            manifest.namespace_key().to_string(),
        ),
    ])
}

fn render_source_path(
    path: &Path,
    namespace_override: Option<&str>,
    inherited_labels: &BTreeMap<String, String>,
) -> anyhow::Result<Vec<ResourceManifest>> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .context("failed to resolve current directory for source path")?
            .join(path)
    };

    if path.is_dir() {
        if let Some(kustomization_file) = find_kustomization_file(&path) {
            render_kustomization_dir(
                &path,
                &kustomization_file,
                namespace_override,
                inherited_labels,
            )
        } else {
            render_manifest_directory(&path, namespace_override, inherited_labels)
        }
    } else {
        render_manifest_file(&path, namespace_override, inherited_labels)
    }
}

fn find_kustomization_file(dir: &Path) -> Option<PathBuf> {
    ["kustomization.yaml", "kustomization.yml", "Kustomization"]
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.exists())
}

fn render_kustomization_dir(
    dir: &Path,
    kustomization_file: &Path,
    namespace_override: Option<&str>,
    inherited_labels: &BTreeMap<String, String>,
) -> anyhow::Result<Vec<ResourceManifest>> {
    let raw = fs::read_to_string(kustomization_file)
        .with_context(|| format!("failed to read '{}'", kustomization_file.display()))?;
    let kustomization: KustomizationFile = serde_yaml::from_str(&raw).with_context(|| {
        format!(
            "failed to parse kustomization '{}'",
            kustomization_file.display()
        )
    })?;

    let effective_namespace = namespace_override
        .map(ToOwned::to_owned)
        .or(kustomization.namespace.clone());
    let mut effective_labels = inherited_labels.clone();
    effective_labels.extend(kustomization.common_labels.clone());

    let mut manifests = Vec::new();
    for resource in &kustomization.resources {
        let resource_path = dir.join(resource);
        manifests.extend(render_source_path(
            &resource_path,
            effective_namespace.as_deref(),
            &effective_labels,
        )?);
    }
    if manifests.is_empty() && kustomization.resources.is_empty() {
        manifests.extend(render_manifest_directory(
            dir,
            effective_namespace.as_deref(),
            &effective_labels,
        )?);
    }

    apply_kustomize_patches(&mut manifests, &kustomization.patches, dir)?;
    Ok(manifests)
}

fn render_manifest_directory(
    dir: &Path,
    namespace_override: Option<&str>,
    inherited_labels: &BTreeMap<String, String>,
) -> anyhow::Result<Vec<ResourceManifest>> {
    let mut manifests = Vec::new();
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("failed to read '{}'", dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("failed to read '{}'", dir.display()))?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            continue;
        }
        let Some(filename) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if matches!(
            filename,
            "kustomization.yaml" | "kustomization.yml" | "Kustomization"
        ) {
            continue;
        }
        if !matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("yaml" | "yml")
        ) {
            continue;
        }
        manifests.extend(render_manifest_file(
            &path,
            namespace_override,
            inherited_labels,
        )?);
    }
    Ok(manifests)
}

fn render_manifest_file(
    path: &Path,
    namespace_override: Option<&str>,
    inherited_labels: &BTreeMap<String, String>,
) -> anyhow::Result<Vec<ResourceManifest>> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    let mut manifests = parse_manifest_documents(&raw)?;
    for manifest in &mut manifests {
        apply_namespace_and_labels(manifest, namespace_override, inherited_labels);
    }
    Ok(manifests)
}

fn apply_namespace_and_labels(
    manifest: &mut ResourceManifest,
    namespace_override: Option<&str>,
    inherited_labels: &BTreeMap<String, String>,
) {
    let is_namespace = matches!(manifest, ResourceManifest::Namespace(_));
    let metadata = manifest.metadata_mut();
    metadata.labels.extend(inherited_labels.clone());
    if let Some(namespace_override) = namespace_override
        && !is_namespace
    {
        metadata.namespace = Some(normalize_namespaced_resource_namespace(Some(
            namespace_override,
        )));
    }
}

fn apply_kustomize_patches(
    manifests: &mut Vec<ResourceManifest>,
    patches: &[KustomizePatchSpec],
    base_dir: &Path,
) -> anyhow::Result<()> {
    for patch_spec in patches {
        let patch_values = load_kustomize_patch_values(patch_spec, base_dir)?;
        for patch_value in patch_values {
            let target = if patch_spec.target.kind.is_some()
                || patch_spec.target.name.is_some()
                || patch_spec.target.namespace.is_some()
            {
                patch_spec.target.clone()
            } else {
                infer_patch_target(&patch_value)?
            };
            let mut matched = false;
            for manifest in manifests.iter_mut() {
                if !patch_target_matches(manifest, &target) {
                    continue;
                }
                let mut manifest_value = serde_yaml::to_value(&*manifest)
                    .context("failed to encode manifest patch target")?;
                merge_yaml(&mut manifest_value, &patch_value);
                *manifest = parse_manifest_value(manifest_value)?;
                matched = true;
            }
            ensure!(
                matched,
                "kustomize patch did not match any rendered resources"
            );
        }
    }
    Ok(())
}

fn load_kustomize_patch_values(
    patch_spec: &KustomizePatchSpec,
    base_dir: &Path,
) -> anyhow::Result<Vec<Value>> {
    let raw = match (&patch_spec.path, &patch_spec.patch) {
        (Some(path), None) => fs::read_to_string(base_dir.join(path))
            .with_context(|| format!("failed to read patch '{}'", base_dir.join(path).display()))?,
        (None, Some(patch)) => patch.clone(),
        (Some(_), Some(_)) => bail!("kustomize patch may specify either path or patch, not both"),
        (None, None) => bail!("kustomize patch must specify path or patch"),
    };
    let mut values = Vec::new();
    for document in serde_yaml::Deserializer::from_str(&raw) {
        let value = Value::deserialize(document).context("failed to parse kustomize patch")?;
        if !matches!(value, Value::Null) {
            values.push(value);
        }
    }
    ensure!(!values.is_empty(), "kustomize patch was empty");
    Ok(values)
}

fn infer_patch_target(value: &Value) -> anyhow::Result<KustomizePatchTarget> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let metadata = value
        .get("metadata")
        .and_then(Value::as_mapping)
        .ok_or_else(|| anyhow!("kustomize patch target is missing metadata"))?;
    let name = metadata
        .get(Value::from("name"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let namespace = metadata
        .get(Value::from("namespace"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    Ok(KustomizePatchTarget {
        kind,
        name,
        namespace,
    })
}

fn patch_target_matches(manifest: &ResourceManifest, target: &KustomizePatchTarget) -> bool {
    if let Some(kind) = target.kind.as_deref()
        && manifest.kind().display_name() != kind
    {
        return false;
    }
    if let Some(name) = target.name.as_deref()
        && manifest.name() != name
    {
        return false;
    }
    if let Some(namespace) = target.namespace.as_deref()
        && manifest.namespace() != Some(namespace)
    {
        return false;
    }
    true
}

fn merge_yaml(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Mapping(target_map), Value::Mapping(patch_map)) => {
            for (key, patch_value) in patch_map {
                if let Some(target_value) = target_map.get_mut(key) {
                    merge_yaml(target_value, patch_value);
                } else {
                    target_map.insert(key.clone(), patch_value.clone());
                }
            }
        }
        (target_value, patch_value) => *target_value = patch_value.clone(),
    }
}

fn delete_rendered_resource(resource: &RenderedResourceRef) -> anyhow::Result<()> {
    let kind = ResourceKind::from_manifest_kind(&resource.kind)?;
    match kind {
        ResourceKind::Deployment => delete_deployment_resources(
            resource.namespace.as_deref().unwrap_or("default"),
            &resource.name,
        ),
        ResourceKind::ReplicaSet => {
            if let Ok(ResourceManifest::ReplicaSet(replica_set)) = load_manifest(
                ResourceKind::ReplicaSet,
                &resource.name,
                resource.namespace.as_deref(),
            ) {
                delete_replica_set_resources(
                    resource.namespace.as_deref().unwrap_or("default"),
                    &replica_set.spec.deployment_name,
                    &resource.name,
                )
            } else {
                delete_manifest_only(kind, &resource.name, resource.namespace.as_deref())
            }
        }
        ResourceKind::Job => delete_job_resources(
            resource.namespace.as_deref().unwrap_or("default"),
            &resource.name,
        ),
        ResourceKind::CronJob => delete_cron_job_resources(
            resource.namespace.as_deref().unwrap_or("default"),
            &resource.name,
        ),
        ResourceKind::Application => delete_application_resources(
            resource.namespace.as_deref().unwrap_or("default"),
            &resource.name,
        ),
        _ => delete_manifest_only(kind, &resource.name, resource.namespace.as_deref()),
    }
}

fn delete_deployment_resources(
    control_namespace: &str,
    deployment_name: &str,
) -> anyhow::Result<()> {
    for session in collect_runtime_sessions()?.into_iter().filter(|session| {
        let Some(context) = session.context.as_ref() else {
            return false;
        };
        context.control_namespace.as_deref() == Some(control_namespace)
            && context.deployment.as_deref() == Some(deployment_name)
    }) {
        let _ = delete_runtime_session(&session);
    }
    for replica_set in load_replica_sets_for_deployment(control_namespace, deployment_name)? {
        let _ = delete_manifest_only(
            ResourceKind::ReplicaSet,
            &replica_set.metadata.name,
            Some(control_namespace),
        );
    }
    delete_manifest_only(
        ResourceKind::Deployment,
        deployment_name,
        Some(control_namespace),
    )
}

fn delete_job_resources(control_namespace: &str, job_name: &str) -> anyhow::Result<()> {
    let state = load_job_controller_state(control_namespace, job_name).unwrap_or_default();
    for run in &state.runs {
        let _ = delete_runtime_session_by_namespace(&run.runtime_namespace);
    }
    let state_path = job_controller_state_path(control_namespace, job_name)?;
    if state_path.exists() {
        let _ = fs::remove_file(&state_path);
    }
    delete_manifest_only(ResourceKind::Job, job_name, Some(control_namespace))
}

fn delete_cron_job_resources(control_namespace: &str, cron_job_name: &str) -> anyhow::Result<()> {
    let state =
        load_cron_job_controller_state(control_namespace, cron_job_name).unwrap_or_default();
    for job_name in state.jobs {
        let _ = delete_job_resources(control_namespace, &job_name);
    }
    let state_path = cron_job_controller_state_path(control_namespace, cron_job_name)?;
    if state_path.exists() {
        let _ = fs::remove_file(&state_path);
    }
    delete_manifest_only(
        ResourceKind::CronJob,
        cron_job_name,
        Some(control_namespace),
    )
}

fn delete_application_resources(
    control_namespace: &str,
    application_name: &str,
) -> anyhow::Result<()> {
    let state =
        load_application_controller_state(control_namespace, application_name).unwrap_or_default();
    for rendered_resource in &state.rendered_resources {
        let _ = delete_rendered_resource(rendered_resource);
    }
    let state_path = application_controller_state_path(control_namespace, application_name)?;
    if state_path.exists() {
        let _ = fs::remove_file(&state_path);
    }
    let cache_path = control_plane_root()?
        .join("cache")
        .join("applications")
        .join(control_namespace)
        .join(slugify(application_name));
    if cache_path.exists() {
        let _ = fs::remove_dir_all(&cache_path);
    }
    delete_manifest_only(
        ResourceKind::Application,
        application_name,
        Some(control_namespace),
    )
}

fn delete_manifest_only(
    kind: ResourceKind,
    name: &str,
    namespace: Option<&str>,
) -> anyhow::Result<()> {
    let path = manifest_path(kind, name, namespace)?;
    if path.exists() {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to remove '{}'", path.display()));
            }
        }
    }
    Ok(())
}

fn delete_runtime_session_by_namespace(namespace: &str) -> anyhow::Result<()> {
    if let Ok(session) = load_runtime_session_by_namespace(namespace) {
        let _ = delete_runtime_session(&session);
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    let codex_app_dir = PathBuf::from(&home)
        .join(".jarvis")
        .join("codex-app")
        .join("sessions")
        .join(namespace);
    if codex_app_dir.exists() {
        let _ = fs::remove_dir_all(&codex_app_dir);
    }
    Ok(())
}

fn application_controller_state_path(
    control_namespace: &str,
    application_name: &str,
) -> anyhow::Result<PathBuf> {
    Ok(control_plane_root()?
        .join("state")
        .join("applications")
        .join(control_namespace)
        .join(format!("{}.json", slugify(application_name))))
}

fn load_application_controller_state(
    control_namespace: &str,
    application_name: &str,
) -> anyhow::Result<ApplicationControllerState> {
    let path = application_controller_state_path(control_namespace, application_name)?;
    if !path.exists() {
        return Ok(ApplicationControllerState::default());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse '{}'", path.display()))
}

fn save_application_controller_state(
    control_namespace: &str,
    application_name: &str,
    state: &ApplicationControllerState,
) -> anyhow::Result<()> {
    let path = application_controller_state_path(control_namespace, application_name)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    let raw = serde_json::to_string_pretty(state)
        .context("failed to encode application controller state")?;
    fs::write(&path, raw).with_context(|| format!("failed to write '{}'", path.display()))
}

fn load_namespace_defaults(control_namespace: &str) -> anyhow::Result<NamespaceSpec> {
    match load_manifest(ResourceKind::Namespace, control_namespace, None) {
        Ok(ResourceManifest::Namespace(namespace)) => Ok(namespace.spec),
        Ok(_) => bail!("resource '{}' is not a Namespace", control_namespace),
        Err(_) => Ok(NamespaceSpec::default()),
    }
}

fn load_config_map_values(
    control_namespace: &str,
    names: &[String],
) -> anyhow::Result<BTreeMap<String, BTreeMap<String, String>>> {
    let mut values = BTreeMap::new();
    for name in names {
        let manifest = load_manifest(ResourceKind::ConfigMap, name, Some(control_namespace))?;
        let ResourceManifest::ConfigMap(config_map) = manifest else {
            bail!(
                "resource '{}/{}' is not a ConfigMap",
                control_namespace,
                name
            );
        };
        values.insert(name.clone(), config_map.spec.data);
    }
    Ok(values)
}

fn load_secret_values(
    control_namespace: &str,
    names: &[String],
) -> anyhow::Result<BTreeMap<String, BTreeMap<String, String>>> {
    let mut values = BTreeMap::new();
    for name in names {
        let manifest = load_manifest(ResourceKind::Secret, name, Some(control_namespace))?;
        let ResourceManifest::Secret(secret) = manifest else {
            bail!("resource '{}/{}' is not a Secret", control_namespace, name);
        };
        values.insert(name.clone(), secret.spec.string_data);
    }
    Ok(values)
}

fn load_volume_paths(control_namespace: &str, names: &[String]) -> anyhow::Result<Vec<String>> {
    let mut paths = Vec::new();
    for name in names {
        let manifest = load_manifest(ResourceKind::Volume, name, Some(control_namespace))?;
        let ResourceManifest::Volume(volume) = manifest else {
            bail!("resource '{}/{}' is not a Volume", control_namespace, name);
        };
        for path in volume.spec.paths {
            ensure!(
                Path::new(&path).exists(),
                "volume path '{}' from {}/{} does not exist",
                path,
                control_namespace,
                name
            );
            paths.push(path);
        }
    }
    Ok(paths)
}

fn merged_environment(
    config_maps: &BTreeMap<String, BTreeMap<String, String>>,
    secrets: &BTreeMap<String, BTreeMap<String, String>>,
    service_environment: &BTreeMap<String, String>,
    runtime_identity: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut environment = BTreeMap::new();
    for data in config_maps.values() {
        environment.extend(data.clone());
    }
    for data in secrets.values() {
        environment.extend(data.clone());
    }
    environment.extend(service_environment.clone());
    environment.extend(runtime_identity.clone());
    environment
}

fn runtime_identity_environment(
    control_namespace: &str,
    deployment_name: &str,
    runtime_namespace: &str,
    ordinal: usize,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "JARVIS_RUNTIME_NAMESPACE".to_string(),
            runtime_namespace.to_string(),
        ),
        (
            "JARVIS_CONTROL_NAMESPACE".to_string(),
            control_namespace.to_string(),
        ),
        ("JARVIS_DEPLOYMENT".to_string(), deployment_name.to_string()),
        ("JARVIS_REPLICA_ORDINAL".to_string(), ordinal.to_string()),
    ])
}

fn service_discovery_environment(
    control_namespace: &str,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut environment = BTreeMap::new();
    for manifest in load_manifests_by_kind(ResourceKind::Service, Some(control_namespace))? {
        let ResourceManifest::Service(service) = manifest else {
            continue;
        };
        let env_key = service_env_key(&service.metadata.name);
        environment.insert(
            format!("JARVIS_SERVICE_{}_TARGET", env_key),
            format!("service://{}/{}", control_namespace, service.metadata.name),
        );
        environment.insert(
            format!("JARVIS_SERVICE_{}_NAME", env_key),
            service.metadata.name.clone(),
        );
        environment.insert(
            format!("JARVIS_SERVICE_{}_NAMESPACE", env_key),
            control_namespace.to_string(),
        );
        environment.insert(
            format!("JARVIS_SERVICE_{}_DNS", env_key),
            format!(
                "{}.{}.jarvis",
                slugify(&service.metadata.name),
                slugify(control_namespace)
            ),
        );
    }
    Ok(environment)
}

fn service_env_key(name: &str) -> String {
    let mut value = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            value.push(ch.to_ascii_uppercase());
        } else {
            value.push('_');
        }
    }
    while value.contains("__") {
        value = value.replace("__", "_");
    }
    value.trim_matches('_').to_string()
}

fn list_resource_summaries(
    kind_arg: ControlPlaneResourceKindArg,
    namespace: Option<&str>,
) -> anyhow::Result<Vec<ResourceSummary>> {
    let manifests = match kind_arg {
        ControlPlaneResourceKindArg::All => load_all_manifests(namespace)?,
        _ => load_manifests_by_kind(parse_specific_kind(kind_arg)?, namespace)?,
    };

    let mut summaries = manifests
        .iter()
        .map(resource_summary)
        .collect::<anyhow::Result<Vec<_>>>()?;
    summaries.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.namespace.cmp(&right.namespace))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(summaries)
}

fn resource_summary(manifest: &ResourceManifest) -> anyhow::Result<ResourceSummary> {
    match manifest {
        ResourceManifest::Namespace(namespace) => {
            let status = namespace_status(&namespace.metadata.name)?;
            Ok(ResourceSummary {
                kind: "Namespace".to_string(),
                namespace: None,
                name: namespace.metadata.name.clone(),
                status: format!("{} sessions", status.sessions),
                detail: format!("{} resources", status.resources),
            })
        }
        ResourceManifest::Deployment(deployment) => {
            let status = deployment_status(deployment)?;
            Ok(ResourceSummary {
                kind: "Deployment".to_string(),
                namespace: deployment.metadata.namespace.clone(),
                name: deployment.metadata.name.clone(),
                status: format!("{}/{} ready", status.ready_replicas, status.replicas),
                detail: status
                    .current_replica_set
                    .map(|replica_set| {
                        format!(
                            "{} rev {}",
                            replica_set,
                            status.current_revision.unwrap_or(0)
                        )
                    })
                    .unwrap_or_else(|| "no active ReplicaSet".to_string()),
            })
        }
        ResourceManifest::ReplicaSet(replica_set) => {
            let status = replica_set_status(replica_set)?;
            Ok(ResourceSummary {
                kind: "ReplicaSet".to_string(),
                namespace: replica_set.metadata.namespace.clone(),
                name: replica_set.metadata.name.clone(),
                status: format!("{}/{} ready", status.ready_replicas, status.replicas),
                detail: format!(
                    "{} rev {}{}",
                    status.deployment_name,
                    status.revision,
                    if status.active { " active" } else { "" }
                ),
            })
        }
        ResourceManifest::Job(job) => {
            let status = job_status(job)?;
            Ok(ResourceSummary {
                kind: "Job".to_string(),
                namespace: job.metadata.namespace.clone(),
                name: job.metadata.name.clone(),
                status: format!("{}/{} succeeded", status.succeeded, status.completions),
                detail: format!("active {}, failed {}", status.active, status.failed),
            })
        }
        ResourceManifest::CronJob(cron_job) => {
            let status = cron_job_status(cron_job)?;
            Ok(ResourceSummary {
                kind: "CronJob".to_string(),
                namespace: cron_job.metadata.namespace.clone(),
                name: cron_job.metadata.name.clone(),
                status: format!("{} active jobs", status.active_jobs.len()),
                detail: status.schedule,
            })
        }
        ResourceManifest::Application(application) => {
            let status = application_status(application)?;
            Ok(ResourceSummary {
                kind: "Application".to_string(),
                namespace: application.metadata.namespace.clone(),
                name: application.metadata.name.clone(),
                status: format!("{}/{}", status.sync_status, status.health_status),
                detail: format!(
                    "{} resources @ {} from {}{}",
                    status.rendered_resources,
                    short_revision(&status.resolved_revision),
                    short_revision(&status.source_revision),
                    if status.source_dirty { " dirty" } else { "" }
                ),
            })
        }
        ResourceManifest::Service(service) => {
            let status = service_status(service)?;
            Ok(ResourceSummary {
                kind: "Service".to_string(),
                namespace: service.metadata.namespace.clone(),
                name: service.metadata.name.clone(),
                status: format!("{} endpoints", status.endpoints.len()),
                detail: status.endpoints.join(", "),
            })
        }
        ResourceManifest::NetworkPolicy(network_policy) => {
            let status = network_policy_status(network_policy)?;
            let policy_types = status
                .policy_types
                .iter()
                .map(|value| match value {
                    NetworkPolicyType::Ingress => "Ingress",
                    NetworkPolicyType::Egress => "Egress",
                })
                .collect::<Vec<_>>()
                .join(", ");
            Ok(ResourceSummary {
                kind: "NetworkPolicy".to_string(),
                namespace: network_policy.metadata.namespace.clone(),
                name: network_policy.metadata.name.clone(),
                status: format!("{} selected", status.selected_sessions.len()),
                detail: policy_types,
            })
        }
        ResourceManifest::ConfigMap(config_map) => Ok(ResourceSummary {
            kind: "ConfigMap".to_string(),
            namespace: config_map.metadata.namespace.clone(),
            name: config_map.metadata.name.clone(),
            status: format!("{} entries", config_map.spec.data.len()),
            detail: config_map
                .spec
                .data
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        }),
        ResourceManifest::Secret(secret) => Ok(ResourceSummary {
            kind: "Secret".to_string(),
            namespace: secret.metadata.namespace.clone(),
            name: secret.metadata.name.clone(),
            status: format!("{} keys", secret.spec.string_data.len()),
            detail: secret
                .spec
                .string_data
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        }),
        ResourceManifest::Volume(volume) => Ok(ResourceSummary {
            kind: "Volume".to_string(),
            namespace: volume.metadata.namespace.clone(),
            name: volume.metadata.name.clone(),
            status: format!("{} paths", volume.spec.paths.len()),
            detail: volume.spec.paths.join(", "),
        }),
        ResourceManifest::Worker(worker) => {
            let status = worker_status(worker)?;
            Ok(ResourceSummary {
                kind: "Worker".to_string(),
                namespace: worker.metadata.namespace.clone(),
                name: worker.metadata.name.clone(),
                status: format!(
                    "{}{}",
                    status.model,
                    status
                        .role
                        .as_deref()
                        .map(|role| format!(" ({})", role))
                        .unwrap_or_default()
                ),
                detail: format!(
                    "{} {}",
                    status.provider,
                    if status.loaded { "loaded" } else { "idle" }
                ),
            })
        }
    }
}

fn namespace_status(control_namespace: &str) -> anyhow::Result<NamespaceStatus> {
    let resources = load_all_manifests(Some(control_namespace))?;
    let sessions = collect_runtime_sessions()?
        .into_iter()
        .filter(|session| {
            session
                .context
                .as_ref()
                .and_then(|context| context.control_namespace.as_deref())
                == Some(control_namespace)
        })
        .count();
    Ok(NamespaceStatus {
        resources: resources.len(),
        sessions,
    })
}

fn deployment_status(
    manifest: &ResourceEnvelope<DeploymentSpec>,
) -> anyhow::Result<DeploymentStatus> {
    let control_namespace = manifest.namespace_key();
    let namespace_defaults = load_namespace_defaults(control_namespace)?;
    let desired_hash = deployment_template_hash(manifest, &namespace_defaults)?;
    let replica_sets =
        load_replica_sets_for_deployment(control_namespace, &manifest.metadata.name)?;
    let sessions = collect_runtime_sessions()?;
    let target_replica_set = replica_sets
        .iter()
        .find(|replica_set| replica_set.spec.template_hash == desired_hash)
        .or_else(|| current_deployment_replica_set(&replica_sets));
    let replica_sets = replica_sets
        .iter()
        .map(|replica_set| replica_set_status_with_sessions(replica_set, &sessions))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let updated_replicas = target_replica_set
        .and_then(|replica_set| {
            replica_sets
                .iter()
                .find(|status| status.revision == replica_set.spec.revision)
        })
        .map(|status| status.replicas)
        .unwrap_or(0);
    let ready_replicas = replica_sets
        .iter()
        .filter(|status| status.active)
        .map(|status| status.ready_replicas)
        .sum();
    let unavailable_replicas = manifest.spec.replicas.saturating_sub(ready_replicas);
    let active_sessions = replica_sets
        .iter()
        .filter(|status| status.active)
        .flat_map(|status| status.sessions.clone())
        .collect::<Vec<_>>();
    let available = ready_replicas >= manifest.spec.replicas
        && updated_replicas >= manifest.spec.replicas
        && replica_sets.iter().filter(|status| status.active).count() <= 1;
    let failed = deployment_progress_deadline_exceeded(manifest, target_replica_set, available);
    let progressing = manifest.spec.paused || (!available && !failed) || updated_replicas > 0;
    let conditions = deployment_conditions(
        manifest,
        target_replica_set,
        &replica_sets,
        updated_replicas,
        ready_replicas,
        available,
        failed,
    );
    Ok(DeploymentStatus {
        replicas: manifest.spec.replicas,
        ready_replicas,
        updated_replicas,
        unavailable_replicas,
        paused: manifest.spec.paused,
        progressing,
        available,
        failed,
        strategy: deployment_strategy_summary(manifest)?,
        progress_deadline_seconds: manifest.spec.progress_deadline_seconds,
        current_revision: target_replica_set.map(|replica_set| replica_set.spec.revision),
        current_replica_set: target_replica_set
            .map(|replica_set| replica_set.metadata.name.clone()),
        replica_sets,
        sessions: active_sessions,
        conditions,
    })
}

fn deployment_rollout_history(
    manifest: &ResourceEnvelope<DeploymentSpec>,
) -> anyhow::Result<Vec<DeploymentRolloutHistoryEntry>> {
    let mut history =
        load_replica_sets_for_deployment(manifest.namespace_key(), &manifest.metadata.name)?
            .into_iter()
            .map(|replica_set| {
                let status = replica_set_status(&replica_set)?;
                Ok(DeploymentRolloutHistoryEntry {
                    revision: replica_set.spec.revision,
                    replica_set: replica_set.metadata.name,
                    template_hash: replica_set.spec.template_hash,
                    replicas: status.replicas,
                    ready_replicas: status.ready_replicas,
                    created_at_epoch_ms: replica_set
                        .metadata
                        .annotations
                        .get("jarvisctl.io/created-at-epoch-ms")
                        .and_then(|value| value.parse::<u128>().ok()),
                    active: status.active,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
    history.sort_by(|left, right| right.revision.cmp(&left.revision));
    Ok(history)
}

fn deployment_progress_deadline_exceeded(
    manifest: &ResourceEnvelope<DeploymentSpec>,
    target_replica_set: Option<&ResourceEnvelope<ReplicaSetSpec>>,
    available: bool,
) -> bool {
    if manifest.spec.paused || available {
        return false;
    }
    let Some(replica_set) = target_replica_set else {
        return false;
    };
    let Some(created_at_epoch_ms) = replica_set
        .metadata
        .annotations
        .get("jarvisctl.io/created-at-epoch-ms")
        .and_then(|value| value.parse::<u128>().ok())
    else {
        return false;
    };
    now_epoch_ms().saturating_sub(created_at_epoch_ms)
        > (manifest.spec.progress_deadline_seconds as u128).saturating_mul(1000)
}

fn deployment_conditions(
    manifest: &ResourceEnvelope<DeploymentSpec>,
    target_replica_set: Option<&ResourceEnvelope<ReplicaSetSpec>>,
    replica_sets: &[ReplicaSetStatus],
    updated_replicas: usize,
    ready_replicas: usize,
    available: bool,
    failed: bool,
) -> Vec<DeploymentCondition> {
    let now = now_epoch_ms();
    let current_replica_set_name = target_replica_set
        .map(|replica_set| replica_set.metadata.name.as_str())
        .unwrap_or("unknown");
    let mut conditions = Vec::new();
    conditions.push(DeploymentCondition {
        condition_type: "Available".to_string(),
        status: if available {
            "True".to_string()
        } else {
            "False".to_string()
        },
        reason: if available {
            "MinimumReplicasAvailable".to_string()
        } else {
            "MinimumReplicasUnavailable".to_string()
        },
        message: if available {
            format!(
                "Deployment has {} ready replica(s) and is fully available",
                ready_replicas
            )
        } else {
            format!(
                "Deployment has {}/{} ready replica(s)",
                ready_replicas, manifest.spec.replicas
            )
        },
        last_transition_epoch_ms: now,
    });

    let progressing_condition = if manifest.spec.paused {
        DeploymentCondition {
            condition_type: "Progressing".to_string(),
            status: "Unknown".to_string(),
            reason: "DeploymentPaused".to_string(),
            message: format!(
                "Deployment rollout is paused at revision {}",
                target_replica_set
                    .map(|replica_set| replica_set.spec.revision.to_string())
                    .unwrap_or_else(|| "0".to_string())
            ),
            last_transition_epoch_ms: now,
        }
    } else if failed {
        DeploymentCondition {
            condition_type: "Progressing".to_string(),
            status: "False".to_string(),
            reason: "ProgressDeadlineExceeded".to_string(),
            message: format!(
                "ReplicaSet '{}' has not reached {} updated/ready replica(s) within {}s",
                current_replica_set_name,
                manifest.spec.replicas,
                manifest.spec.progress_deadline_seconds
            ),
            last_transition_epoch_ms: now,
        }
    } else if available {
        DeploymentCondition {
            condition_type: "Progressing".to_string(),
            status: "True".to_string(),
            reason: "NewReplicaSetAvailable".to_string(),
            message: format!(
                "ReplicaSet '{}' is serving revision {}",
                current_replica_set_name,
                target_replica_set
                    .map(|replica_set| replica_set.spec.revision)
                    .unwrap_or(0)
            ),
            last_transition_epoch_ms: now,
        }
    } else {
        let active_replica_sets = replica_sets
            .iter()
            .filter(|replica_set| replica_set.active)
            .count();
        DeploymentCondition {
            condition_type: "Progressing".to_string(),
            status: "True".to_string(),
            reason: "ReplicaSetUpdating".to_string(),
            message: format!(
                "Waiting for ReplicaSet '{}' to reach {}/{} updated replica(s); {} active ReplicaSet(s)",
                current_replica_set_name,
                updated_replicas,
                manifest.spec.replicas,
                active_replica_sets
            ),
            last_transition_epoch_ms: now,
        }
    };
    conditions.push(progressing_condition);
    conditions
}

fn replica_set_status(
    manifest: &ResourceEnvelope<ReplicaSetSpec>,
) -> anyhow::Result<ReplicaSetStatus> {
    let sessions = collect_runtime_sessions()?;
    replica_set_status_with_sessions(manifest, &sessions)
}

fn replica_set_status_with_sessions(
    manifest: &ResourceEnvelope<ReplicaSetSpec>,
    sessions: &[NativeSessionMetadata],
) -> anyhow::Result<ReplicaSetStatus> {
    let desired_namespaces = replica_set_runtime_namespaces(
        manifest.namespace_key(),
        &manifest.spec.deployment_name,
        manifest.spec.revision,
        manifest.spec.replicas,
    );
    let desired_set: HashSet<String> = desired_namespaces.iter().cloned().collect();
    let ready_replicas = sessions
        .iter()
        .filter(|session| desired_set.contains(&session.namespace))
        .filter(|session| {
            session
                .context
                .as_ref()
                .and_then(|context| context.labels.get("jarvisctl.io/replicaset"))
                == Some(&manifest.metadata.name)
        })
        .filter(|session| session.agents.iter().any(|agent| agent.running))
        .count();
    Ok(ReplicaSetStatus {
        deployment_name: manifest.spec.deployment_name.clone(),
        revision: manifest.spec.revision,
        template_hash: manifest.spec.template_hash.clone(),
        replicas: manifest.spec.replicas,
        ready_replicas,
        sessions: desired_namespaces,
        active: manifest.spec.replicas > 0,
    })
}

fn render_rollout_status_table(
    control_namespace: &str,
    deployment_name: &str,
    status: &DeploymentStatus,
) -> String {
    let sessions = if status.sessions.is_empty() {
        "-".to_string()
    } else {
        status.sessions.join(", ")
    };
    let rollout_state = if status.failed {
        "failed"
    } else if deployment_rollout_complete(status) {
        "complete"
    } else if status.paused {
        "paused"
    } else {
        "progressing"
    };
    format!(
        "DEPLOYMENT\tNAMESPACE\tSTATE\tPAUSED\tSTRATEGY\tREVISION\tUPDATED\tREADY\tACTIVE_REPLICASET\tSESSIONS\n{}\t{}\t{}\t{}\t{}\t{}\t{}/{}\t{}/{}\t{}\t{}",
        deployment_name,
        control_namespace,
        rollout_state,
        if status.paused { "yes" } else { "no" },
        status.strategy,
        status
            .current_revision
            .map(|revision| revision.to_string())
            .unwrap_or_else(|| "-".to_string()),
        status.updated_replicas,
        status.replicas,
        status.ready_replicas,
        status.replicas,
        status
            .current_replica_set
            .clone()
            .unwrap_or_else(|| "-".to_string()),
        sessions
    )
}

fn deployment_rollout_complete(status: &DeploymentStatus) -> bool {
    !status.failed
        && status.available
        && status.ready_replicas >= status.replicas
        && status.updated_replicas >= status.replicas
        && status
            .replica_sets
            .iter()
            .filter(|replica_set| replica_set.active)
            .count()
            <= 1
}

fn deployment_rollout_failure_message(status: &DeploymentStatus) -> Option<String> {
    status
        .conditions
        .iter()
        .find(|condition| {
            condition.condition_type == "Progressing"
                && condition.reason == "ProgressDeadlineExceeded"
        })
        .map(|condition| condition.message.clone())
}

fn render_rollout_history_table(history: &[DeploymentRolloutHistoryEntry]) -> String {
    if history.is_empty() {
        return "No rollout history found.".to_string();
    }
    let mut lines =
        vec!["REVISION\tACTIVE\tREPLICASET\tREADY\tCREATED_AT_EPOCH_MS\tTEMPLATE".to_string()];
    for entry in history {
        lines.push(format!(
            "{}\t{}\t{}\t{}/{}\t{}\t{}",
            entry.revision,
            if entry.active { "yes" } else { "no" },
            entry.replica_set,
            entry.ready_replicas,
            entry.replicas,
            entry
                .created_at_epoch_ms
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string()),
            short_revision(&entry.template_hash)
        ));
    }
    lines.join("\n")
}

fn job_status(manifest: &ResourceEnvelope<JobSpec>) -> anyhow::Result<JobStatus> {
    let state = refreshed_job_state(manifest)?;
    Ok(job_status_from_state(manifest, &state))
}

fn cron_job_status(manifest: &ResourceEnvelope<CronJobSpec>) -> anyhow::Result<CronJobStatus> {
    let state = load_cron_job_controller_state(manifest.namespace_key(), &manifest.metadata.name)?;
    let mut active_jobs = Vec::new();
    for job_name in &state.jobs {
        if let Ok(ResourceManifest::Job(job)) = load_manifest(
            ResourceKind::Job,
            job_name,
            manifest.metadata.namespace.as_deref(),
        ) {
            let status = job_status(&job)?;
            if status.active > 0 {
                active_jobs.push(job_name.clone());
            }
        }
    }
    active_jobs.sort();
    Ok(CronJobStatus {
        schedule: manifest.spec.schedule.clone(),
        active_jobs,
        last_schedule_epoch_ms: state.last_schedule_epoch_ms,
    })
}

fn application_status(
    manifest: &ResourceEnvelope<ApplicationSpec>,
) -> anyhow::Result<ApplicationStatus> {
    let state =
        load_application_controller_state(manifest.namespace_key(), &manifest.metadata.name)?;
    let desired = build_application_desired_state(manifest)?;
    let sync_status = if state.last_applied_revision.as_deref()
        == Some(desired.resolved_revision.as_str())
        && application_resources_present(&state.rendered_resources)?
    {
        "Synced".to_string()
    } else {
        "OutOfSync".to_string()
    };
    Ok(ApplicationStatus {
        source_path: manifest.spec.source.path.clone(),
        repo_url: desired.source.repo_url,
        source_type: desired.source.source_type,
        source_root: desired.source.source_root,
        target_revision: application_target_revision(manifest),
        source_revision: desired.source.source_revision,
        source_dirty: desired.source.source_dirty,
        resolved_revision: desired.resolved_revision,
        last_applied_revision: state.last_applied_revision.clone(),
        sync_status,
        health_status: application_health_status(&state.rendered_resources)?,
        destination_namespace: effective_application_destination_namespace(manifest),
        rendered_resources: state.rendered_resources.len(),
        last_sync_epoch_ms: state.last_sync_epoch_ms,
        history: state.history.clone(),
    })
}

fn application_diff(
    application_name: &str,
    namespace: Option<&str>,
) -> anyhow::Result<ApplicationDiffResult> {
    let control_namespace = normalize_namespaced_resource_namespace(namespace);
    let manifest = load_manifest(
        ResourceKind::Application,
        application_name,
        Some(&control_namespace),
    )?;
    let ResourceManifest::Application(application) = manifest else {
        bail!(
            "resource '{}/{}' is not an Application",
            control_namespace,
            application_name
        );
    };
    let state =
        load_application_controller_state(application.namespace_key(), &application.metadata.name)?;
    let desired = build_application_desired_state(&application)?;
    let changes = application_diff_entries(&desired.rendered, &state.rendered_resources)?;
    let creates = changes
        .iter()
        .filter(|entry| entry.action == "create")
        .count();
    let updates = changes
        .iter()
        .filter(|entry| entry.action == "update")
        .count();
    let deletes = changes
        .iter()
        .filter(|entry| entry.action == "delete")
        .count();
    Ok(ApplicationDiffResult {
        application: application.metadata.name.clone(),
        namespace: control_namespace,
        repo_url: desired.source.repo_url,
        source_type: desired.source.source_type,
        source_revision: desired.source.source_revision,
        source_dirty: desired.source.source_dirty,
        target_revision: application_target_revision(&application),
        resolved_revision: desired.resolved_revision,
        creates,
        updates,
        deletes,
        changes,
    })
}

fn application_diff_entries(
    desired_manifests: &[ResourceManifest],
    previous_resources: &[RenderedResourceRef],
) -> anyhow::Result<Vec<ApplicationDiffEntry>> {
    let mut changes = Vec::new();
    let mut desired_refs = BTreeSet::new();

    for desired_manifest in desired_manifests {
        let resource_ref = RenderedResourceRef::from_manifest(desired_manifest);
        desired_refs.insert(resource_ref.clone());
        let kind = ResourceKind::from_manifest_kind(&resource_ref.kind)?;
        match load_manifest(kind, &resource_ref.name, resource_ref.namespace.as_deref()) {
            Ok(current_manifest) => {
                if manifests_equivalent(desired_manifest, &current_manifest)? {
                    continue;
                }
                changes.push(ApplicationDiffEntry {
                    action: "update".to_string(),
                    kind: resource_ref.kind,
                    namespace: resource_ref.namespace,
                    name: resource_ref.name,
                    detail: "live manifest differs from desired source".to_string(),
                });
            }
            Err(_) => changes.push(ApplicationDiffEntry {
                action: "create".to_string(),
                kind: resource_ref.kind,
                namespace: resource_ref.namespace,
                name: resource_ref.name,
                detail: "resource is missing from the control plane".to_string(),
            }),
        }
    }

    for previous_resource in previous_resources {
        if desired_refs.contains(previous_resource) {
            continue;
        }
        let kind = ResourceKind::from_manifest_kind(&previous_resource.kind)?;
        if load_manifest(
            kind,
            &previous_resource.name,
            previous_resource.namespace.as_deref(),
        )
        .is_ok()
        {
            changes.push(ApplicationDiffEntry {
                action: "delete".to_string(),
                kind: previous_resource.kind.clone(),
                namespace: previous_resource.namespace.clone(),
                name: previous_resource.name.clone(),
                detail: "resource is owned by the application but absent from desired source"
                    .to_string(),
            });
        }
    }

    changes.sort_by(|left, right| {
        left.action
            .cmp(&right.action)
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.namespace.cmp(&right.namespace))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(changes)
}

fn manifests_equivalent(left: &ResourceManifest, right: &ResourceManifest) -> anyhow::Result<bool> {
    Ok(
        serde_json::to_value(left).context("failed to encode desired manifest")?
            == serde_json::to_value(right).context("failed to encode live manifest")?,
    )
}

fn render_application_diff_table(diff: &ApplicationDiffResult) -> String {
    if diff.changes.is_empty() {
        return format!(
            "APPLICATION\tNAMESPACE\tSOURCE\tREVISION\tDIRTY\tCHANGES\n{}\t{}\t{}\t{}\t{}\t0",
            diff.application,
            diff.namespace,
            diff.source_type,
            short_revision(&diff.source_revision),
            if diff.source_dirty { "yes" } else { "no" },
        );
    }

    let mut lines =
        vec!["APPLICATION\tNAMESPACE\tACTION\tKIND\tRESOURCE_NAMESPACE\tNAME\tDETAIL".to_string()];
    for change in &diff.changes {
        lines.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            diff.application,
            diff.namespace,
            change.action,
            change.kind,
            change.namespace.clone().unwrap_or_else(|| "-".to_string()),
            change.name,
            change.detail
        ));
    }
    lines.join("\n")
}

fn application_resources_present(resources: &[RenderedResourceRef]) -> anyhow::Result<bool> {
    for resource in resources {
        let kind = ResourceKind::from_manifest_kind(&resource.kind)?;
        if load_manifest(kind, &resource.name, resource.namespace.as_deref()).is_err() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn application_health_status(resources: &[RenderedResourceRef]) -> anyhow::Result<String> {
    if resources.is_empty() {
        return Ok("Missing".to_string());
    }

    let mut missing = false;
    let mut degraded = false;
    let mut progressing = false;

    for resource in resources {
        let kind = ResourceKind::from_manifest_kind(&resource.kind)?;
        let manifest = match load_manifest(kind, &resource.name, resource.namespace.as_deref()) {
            Ok(manifest) => manifest,
            Err(_) => {
                missing = true;
                continue;
            }
        };
        match manifest {
            ResourceManifest::Deployment(deployment) => {
                let status = deployment_status(&deployment)?;
                if status.ready_replicas < status.replicas {
                    progressing = true;
                }
            }
            ResourceManifest::ReplicaSet(replica_set) => {
                let status = replica_set_status(&replica_set)?;
                if status.ready_replicas < status.replicas {
                    progressing = true;
                }
            }
            ResourceManifest::Job(job) => {
                let status = job_status(&job)?;
                if status.succeeded < status.completions {
                    if status.failed > 0 && status.active == 0 {
                        degraded = true;
                    } else {
                        progressing = true;
                    }
                }
            }
            _ => {}
        }
    }

    if missing {
        Ok("Missing".to_string())
    } else if degraded {
        Ok("Degraded".to_string())
    } else if progressing {
        Ok("Progressing".to_string())
    } else {
        Ok("Healthy".to_string())
    }
}

fn job_status_from_state(
    manifest: &ResourceEnvelope<JobSpec>,
    state: &JobControllerState,
) -> JobStatus {
    let mut runs = state
        .runs
        .iter()
        .map(|run| format!("{} ({})", run.runtime_namespace, job_phase_name(run.status)))
        .collect::<Vec<_>>();
    runs.sort();
    JobStatus {
        completions: manifest.spec.completions,
        active: state
            .runs
            .iter()
            .filter(|run| run.status == JobRunPhase::Active)
            .count(),
        succeeded: state
            .runs
            .iter()
            .filter(|run| run.status == JobRunPhase::Succeeded)
            .count(),
        failed: state
            .runs
            .iter()
            .filter(|run| run.status == JobRunPhase::Failed)
            .count(),
        runs,
    }
}

fn job_phase_name(phase: JobRunPhase) -> &'static str {
    match phase {
        JobRunPhase::Active => "active",
        JobRunPhase::Succeeded => "succeeded",
        JobRunPhase::Failed => "failed",
    }
}

fn refreshed_job_state(manifest: &ResourceEnvelope<JobSpec>) -> anyhow::Result<JobControllerState> {
    let mut state = load_job_controller_state(manifest.namespace_key(), &manifest.metadata.name)?;
    let sessions = collect_runtime_sessions()?;
    let now = now_epoch_ms();
    for run in &mut state.runs {
        let session = sessions
            .iter()
            .find(|session| session.namespace == run.runtime_namespace);
        let completion = if session.is_none() {
            native_session_completion(&run.runtime_namespace)?
        } else {
            None
        };
        if job_session_is_active(session) {
            run.last_active_epoch_ms = Some(now);
        }
        run.status = job_run_phase(
            session,
            completion.as_ref(),
            run.status,
            run.last_active_epoch_ms.unwrap_or(run.created_at_epoch_ms),
            now,
        );
    }
    save_job_controller_state(manifest.namespace_key(), &manifest.metadata.name, &state)?;
    Ok(state)
}

fn job_session_is_active(session: Option<&NativeSessionMetadata>) -> bool {
    session
        .map(|session| session.agents.iter().any(|agent| agent.running))
        .unwrap_or(false)
}

fn job_run_phase(
    session: Option<&NativeSessionMetadata>,
    completion: Option<&NativeSessionCompletion>,
    previous: JobRunPhase,
    last_active_epoch_ms: u128,
    now_epoch_ms: u128,
) -> JobRunPhase {
    let Some(session) = session else {
        if let Some(completion) = completion {
            let succeeded = completion
                .agents
                .iter()
                .all(|agent| agent.exit_code.unwrap_or(1) == 0);
            return if succeeded {
                JobRunPhase::Succeeded
            } else {
                JobRunPhase::Failed
            };
        }
        return match previous {
            JobRunPhase::Succeeded => JobRunPhase::Succeeded,
            JobRunPhase::Active
                if now_epoch_ms.saturating_sub(last_active_epoch_ms) < JOB_COMPLETION_GRACE_MS =>
            {
                JobRunPhase::Active
            }
            _ => JobRunPhase::Failed,
        };
    };
    let Some(context) = session.context.as_ref() else {
        return if session.agents.iter().any(|agent| agent.running) {
            JobRunPhase::Active
        } else if session
            .agents
            .iter()
            .all(|agent| agent.exit_code.unwrap_or(1) == 0)
        {
            JobRunPhase::Succeeded
        } else if session.agents.iter().all(|agent| !agent.running) {
            JobRunPhase::Failed
        } else {
            previous
        };
    };

    let turn_completed = context.turn_status.as_deref() == Some("completed");
    let thread_idle = context.thread_status.as_deref() == Some("idle");
    if turn_completed && context.last_error.is_some() {
        return JobRunPhase::Failed;
    }
    if turn_completed && (thread_idle || context.last_error.is_none()) {
        return JobRunPhase::Succeeded;
    }
    if session.agents.iter().any(|agent| agent.running) {
        JobRunPhase::Active
    } else if context.last_error.is_some() {
        JobRunPhase::Failed
    } else {
        JobRunPhase::Succeeded
    }
}

fn service_status(manifest: &ResourceEnvelope<ServiceSpec>) -> anyhow::Result<ServiceStatus> {
    let mut sessions = collect_runtime_sessions()?;
    sessions.retain(|session| service_matches_session(manifest, session));
    sessions.sort_by(|left, right| left.namespace.cmp(&right.namespace));
    Ok(ServiceStatus {
        endpoints: sessions
            .into_iter()
            .map(|session| session.namespace)
            .collect(),
        strategy: manifest.spec.strategy.clone(),
    })
}

fn network_policy_status(
    manifest: &ResourceEnvelope<NetworkPolicySpec>,
) -> anyhow::Result<NetworkPolicyStatus> {
    let mut sessions = collect_runtime_sessions()?
        .into_iter()
        .filter(|session| network_policy_selects_session(manifest, session))
        .map(|session| session.namespace)
        .collect::<Vec<_>>();
    sessions.sort();
    Ok(NetworkPolicyStatus {
        selected_sessions: sessions,
        policy_types: effective_network_policy_types(&manifest.spec),
    })
}

fn worker_status(manifest: &ResourceEnvelope<WorkerSpec>) -> anyhow::Result<WorkerStatus> {
    let loaded = match manifest.spec.provider {
        WorkerProvider::Ollama => ollama_model_loaded(&manifest.spec.model)?,
    };
    Ok(WorkerStatus {
        provider: "ollama".to_string(),
        model: manifest.spec.model.clone(),
        endpoint: worker_endpoint(manifest),
        role: manifest.spec.role.clone(),
        output_mode: match effective_worker_output_mode(manifest) {
            WorkerOutputMode::Text => "text".to_string(),
            WorkerOutputMode::Json => "json".to_string(),
        },
        loaded,
    })
}

pub fn invoke_worker(
    worker_name: &str,
    namespace: Option<&str>,
    prompt: &str,
) -> anyhow::Result<String> {
    let control_namespace = normalize_namespaced_resource_namespace(namespace);
    let manifest = load_manifest(ResourceKind::Worker, worker_name, Some(&control_namespace))?;
    let ResourceManifest::Worker(worker) = manifest else {
        bail!(
            "resource '{}/{}' is not a Worker",
            control_namespace,
            worker_name
        );
    };
    match worker.spec.provider {
        WorkerProvider::Ollama => invoke_ollama_worker(&worker, prompt),
    }
}

fn invoke_ollama_worker(
    manifest: &ResourceEnvelope<WorkerSpec>,
    prompt: &str,
) -> anyhow::Result<String> {
    let endpoint = worker_endpoint(manifest);
    let mut options = serde_json::Map::new();
    if let Some(temperature) = manifest.spec.temperature {
        options.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(num_predict) = manifest.spec.num_predict {
        options.insert("num_predict".to_string(), json!(num_predict));
    }
    if let Some(num_ctx) = manifest.spec.num_ctx {
        options.insert("num_ctx".to_string(), json!(num_ctx));
    }
    let final_prompt = manifest
        .spec
        .system_prompt
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|system_prompt| format!("{}\n\nTask:\n{}", system_prompt.trim(), prompt.trim()))
        .unwrap_or_else(|| prompt.trim().to_string());
    let mut payload = serde_json::Map::from_iter([
        ("model".to_string(), json!(manifest.spec.model)),
        ("prompt".to_string(), json!(final_prompt)),
        ("stream".to_string(), json!(false)),
    ]);
    if !options.is_empty() {
        payload.insert("options".to_string(), serde_json::Value::Object(options));
    }
    if matches!(
        effective_worker_output_mode(manifest),
        WorkerOutputMode::Json
    ) {
        payload.insert("format".to_string(), json!("json"));
    }

    let url = format!("{}/api/generate", endpoint.trim_end_matches('/'));
    let mut response = ureq::post(&url)
        .content_type("application/json")
        .send_json(serde_json::Value::Object(payload))
        .with_context(|| format!("failed to reach Ollama worker endpoint '{}'", url))?;
    let body: serde_json::Value = response
        .body_mut()
        .read_json()
        .context("failed to decode Ollama worker response")?;
    let text = body
        .get("response")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("Ollama worker response was missing 'response' text"))?
        .trim()
        .to_string();
    Ok(match effective_worker_output_mode(manifest) {
        WorkerOutputMode::Text => text,
        WorkerOutputMode::Json => serde_json::to_string_pretty(
            &serde_json::from_str::<serde_json::Value>(&text)
                .context("worker declared json output but returned invalid JSON")?,
        )
        .context("failed to pretty-print worker JSON output")?,
    })
}

fn ollama_model_loaded(model: &str) -> anyhow::Result<bool> {
    let output = ProcessCommand::new("ollama")
        .arg("ps")
        .output()
        .context("failed to execute 'ollama ps'")?;
    ensure!(
        output.status.success(),
        "'ollama ps' failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let stdout = String::from_utf8(output.stdout).context("ollama ps returned non-utf8 output")?;
    Ok(stdout
        .lines()
        .any(|line| line.split_whitespace().next() == Some(model)))
}

fn worker_endpoint(manifest: &ResourceEnvelope<WorkerSpec>) -> String {
    manifest
        .spec
        .endpoint
        .clone()
        .unwrap_or_else(default_ollama_endpoint)
}

fn default_ollama_endpoint() -> String {
    "http://127.0.0.1:11434".to_string()
}

fn effective_worker_output_mode(manifest: &ResourceEnvelope<WorkerSpec>) -> WorkerOutputMode {
    manifest.spec.output_mode.clone().unwrap_or_default()
}

fn describe_status(manifest: &ResourceManifest) -> anyhow::Result<serde_json::Value> {
    match manifest {
        ResourceManifest::Namespace(namespace) => Ok(serde_json::to_value(namespace_status(
            &namespace.metadata.name,
        )?)?),
        ResourceManifest::Deployment(deployment) => {
            Ok(serde_json::to_value(deployment_status(deployment)?)?)
        }
        ResourceManifest::ReplicaSet(replica_set) => {
            Ok(serde_json::to_value(replica_set_status(replica_set)?)?)
        }
        ResourceManifest::Job(job) => Ok(serde_json::to_value(job_status(job)?)?),
        ResourceManifest::CronJob(cron_job) => {
            Ok(serde_json::to_value(cron_job_status(cron_job)?)?)
        }
        ResourceManifest::Application(application) => {
            Ok(serde_json::to_value(application_status(application)?)?)
        }
        ResourceManifest::Service(service) => Ok(serde_json::to_value(service_status(service)?)?),
        ResourceManifest::NetworkPolicy(network_policy) => Ok(serde_json::to_value(
            network_policy_status(network_policy)?,
        )?),
        ResourceManifest::ConfigMap(config_map) => Ok(json!({
            "entries": config_map.spec.data.len(),
            "keys": config_map.spec.data.keys().cloned().collect::<Vec<_>>(),
        })),
        ResourceManifest::Secret(secret) => Ok(json!({
            "keys": secret.spec.string_data.keys().cloned().collect::<Vec<_>>(),
        })),
        ResourceManifest::Volume(volume) => Ok(json!({
            "paths": volume.spec.paths,
        })),
        ResourceManifest::Worker(worker) => Ok(serde_json::to_value(worker_status(worker)?)?),
    }
}

fn service_matches_session(
    manifest: &ResourceEnvelope<ServiceSpec>,
    session: &NativeSessionMetadata,
) -> bool {
    if !session.agents.iter().any(|agent| agent.running) {
        return false;
    }
    let Some(context) = session.context.as_ref() else {
        return false;
    };
    if context.control_namespace.as_deref() != manifest.metadata.namespace.as_deref() {
        return false;
    }
    manifest
        .spec
        .selector
        .iter()
        .all(|(key, value)| context.labels.get(key) == Some(value))
}

fn ensure_message_flow_allowed(
    source: Option<&NativeSessionMetadata>,
    target: &NativeSessionMetadata,
) -> anyhow::Result<()> {
    let Some(source) = source else {
        return Ok(());
    };
    let Some(source_context) = source.context.as_ref() else {
        return Ok(());
    };
    let Some(target_context) = target.context.as_ref() else {
        return Ok(());
    };
    let Some(source_control_namespace) = source_context.control_namespace.as_deref() else {
        return Ok(());
    };
    let Some(target_control_namespace) = target_context.control_namespace.as_deref() else {
        return Ok(());
    };

    let ingress_policies = network_policies_selecting_session(target)?
        .into_iter()
        .filter(|policy| {
            effective_network_policy_types(&policy.spec).contains(&NetworkPolicyType::Ingress)
        })
        .collect::<Vec<_>>();
    if !ingress_policies.is_empty()
        && !ingress_policies
            .iter()
            .any(|policy| network_policy_allows_ingress(policy, source, target_control_namespace))
    {
        bail!(
            "network policy denied message from '{}' to '{}'",
            source.namespace,
            target.namespace
        );
    }

    let egress_policies = network_policies_selecting_session(source)?
        .into_iter()
        .filter(|policy| {
            effective_network_policy_types(&policy.spec).contains(&NetworkPolicyType::Egress)
        })
        .collect::<Vec<_>>();
    if !egress_policies.is_empty()
        && !egress_policies
            .iter()
            .any(|policy| network_policy_allows_egress(policy, target, source_control_namespace))
    {
        bail!(
            "network policy denied message from '{}' to '{}'",
            source.namespace,
            target.namespace
        );
    }

    Ok(())
}

fn network_policies_selecting_session(
    session: &NativeSessionMetadata,
) -> anyhow::Result<Vec<ResourceEnvelope<NetworkPolicySpec>>> {
    let Some(context) = session.context.as_ref() else {
        return Ok(Vec::new());
    };
    let Some(control_namespace) = context.control_namespace.as_deref() else {
        return Ok(Vec::new());
    };
    let policies = load_manifests_by_kind(ResourceKind::NetworkPolicy, Some(control_namespace))?
        .into_iter()
        .filter_map(|manifest| match manifest {
            ResourceManifest::NetworkPolicy(policy) => Some(policy),
            _ => None,
        })
        .filter(|policy| network_policy_selects_session(policy, session))
        .collect::<Vec<_>>();
    Ok(policies)
}

fn network_policy_selects_session(
    policy: &ResourceEnvelope<NetworkPolicySpec>,
    session: &NativeSessionMetadata,
) -> bool {
    if session
        .context
        .as_ref()
        .and_then(|context| context.control_namespace.as_deref())
        != policy.metadata.namespace.as_deref()
    {
        return false;
    }
    let Some(context) = session.context.as_ref() else {
        return false;
    };
    label_selector_matches(&policy.spec.pod_selector, &context.labels)
}

fn network_policy_allows_ingress(
    policy: &ResourceEnvelope<NetworkPolicySpec>,
    source: &NativeSessionMetadata,
    policy_namespace: &str,
) -> bool {
    if !effective_network_policy_types(&policy.spec).contains(&NetworkPolicyType::Ingress) {
        return true;
    }
    policy.spec.ingress.iter().any(|rule| {
        if rule.from.is_empty() {
            return true;
        }
        rule.from
            .iter()
            .any(|peer| network_policy_peer_matches_session(peer, source, policy_namespace))
    })
}

fn network_policy_allows_egress(
    policy: &ResourceEnvelope<NetworkPolicySpec>,
    target: &NativeSessionMetadata,
    policy_namespace: &str,
) -> bool {
    if !effective_network_policy_types(&policy.spec).contains(&NetworkPolicyType::Egress) {
        return true;
    }
    policy.spec.egress.iter().any(|rule| {
        if rule.to.is_empty() {
            return true;
        }
        rule.to
            .iter()
            .any(|peer| network_policy_peer_matches_session(peer, target, policy_namespace))
    })
}

fn network_policy_peer_matches_session(
    peer: &NetworkPolicyPeer,
    session: &NativeSessionMetadata,
    policy_namespace: &str,
) -> bool {
    let Some(context) = session.context.as_ref() else {
        return false;
    };
    let Some(session_namespace) = context.control_namespace.as_deref() else {
        return false;
    };

    match (&peer.namespace_selector, &peer.pod_selector) {
        (None, None) => true,
        (Some(namespace_selector), None) => {
            match_namespace_selector(session_namespace, namespace_selector).unwrap_or(false)
        }
        (None, Some(pod_selector)) => {
            session_namespace == policy_namespace
                && label_selector_matches(pod_selector, &context.labels)
        }
        (Some(namespace_selector), Some(pod_selector)) => {
            match_namespace_selector(session_namespace, namespace_selector).unwrap_or(false)
                && label_selector_matches(pod_selector, &context.labels)
        }
    }
}

fn match_namespace_selector(
    control_namespace: &str,
    selector: &LabelSelector,
) -> anyhow::Result<bool> {
    let labels = namespace_labels(control_namespace)?;
    Ok(label_selector_matches(selector, &labels))
}

fn namespace_labels(control_namespace: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let mut labels = match load_manifest(ResourceKind::Namespace, control_namespace, None) {
        Ok(ResourceManifest::Namespace(namespace)) => namespace.metadata.labels,
        Ok(_) => BTreeMap::new(),
        Err(_) => BTreeMap::new(),
    };
    labels.insert(
        "kubernetes.io/metadata.name".to_string(),
        control_namespace.to_string(),
    );
    labels.insert(
        "jarvisctl.io/name".to_string(),
        control_namespace.to_string(),
    );
    Ok(labels)
}

fn label_selector_matches(selector: &LabelSelector, labels: &BTreeMap<String, String>) -> bool {
    selector
        .match_labels
        .iter()
        .all(|(key, value)| labels.get(key) == Some(value))
}

fn effective_network_policy_types(spec: &NetworkPolicySpec) -> Vec<NetworkPolicyType> {
    let mut policy_types = if spec.policy_types.is_empty() {
        vec![NetworkPolicyType::Ingress]
    } else {
        spec.policy_types.clone()
    };
    if !spec.egress.is_empty() && !policy_types.contains(&NetworkPolicyType::Egress) {
        policy_types.push(NetworkPolicyType::Egress);
    }
    policy_types
}

fn load_all_manifests(namespace: Option<&str>) -> anyhow::Result<Vec<ResourceManifest>> {
    let mut manifests = Vec::new();
    for kind in [
        ResourceKind::Namespace,
        ResourceKind::Application,
        ResourceKind::CronJob,
        ResourceKind::Job,
        ResourceKind::Deployment,
        ResourceKind::ReplicaSet,
        ResourceKind::Service,
        ResourceKind::NetworkPolicy,
        ResourceKind::ConfigMap,
        ResourceKind::Secret,
        ResourceKind::Volume,
        ResourceKind::Worker,
    ] {
        manifests.extend(load_manifests_by_kind(kind, namespace)?);
    }
    Ok(manifests)
}

fn load_manifests_by_kind(
    kind: ResourceKind,
    namespace: Option<&str>,
) -> anyhow::Result<Vec<ResourceManifest>> {
    let root = control_plane_root()?;
    let mut manifests = Vec::new();
    match kind {
        ResourceKind::Namespace => {
            let dir = root.join("namespaces");
            if !dir.exists() {
                return Ok(Vec::new());
            }
            for entry in
                fs::read_dir(&dir).with_context(|| format!("failed to read '{}'", dir.display()))?
            {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    manifests.push(load_manifest_from_path(&entry.path())?);
                }
            }
        }
        _ => {
            let dir = root.join(kind.directory_name());
            if !dir.exists() {
                return Ok(Vec::new());
            }
            if let Some(namespace) = namespace {
                let namespace_dir =
                    dir.join(normalize_namespaced_resource_namespace(Some(namespace)));
                if !namespace_dir.exists() {
                    return Ok(Vec::new());
                }
                for entry in fs::read_dir(&namespace_dir)
                    .with_context(|| format!("failed to read '{}'", namespace_dir.display()))?
                {
                    let entry = entry?;
                    if entry.file_type()?.is_file() {
                        manifests.push(load_manifest_from_path(&entry.path())?);
                    }
                }
            } else {
                for namespace_entry in fs::read_dir(&dir)
                    .with_context(|| format!("failed to read '{}'", dir.display()))?
                {
                    let namespace_entry = namespace_entry?;
                    if !namespace_entry.file_type()?.is_dir() {
                        continue;
                    }
                    for entry in fs::read_dir(namespace_entry.path()).with_context(|| {
                        format!("failed to read '{}'", namespace_entry.path().display())
                    })? {
                        let entry = entry?;
                        if entry.file_type()?.is_file() {
                            manifests.push(load_manifest_from_path(&entry.path())?);
                        }
                    }
                }
            }
        }
    }
    Ok(manifests)
}

fn load_manifest(
    kind: ResourceKind,
    name: &str,
    namespace: Option<&str>,
) -> anyhow::Result<ResourceManifest> {
    let path = manifest_path(kind, name, namespace)?;
    load_manifest_from_path(&path)
}

fn load_manifest_from_path(path: &Path) -> anyhow::Result<ResourceManifest> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    parse_manifest_documents(&raw)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("manifest '{}' is empty", path.display()))
}

fn manifest_path(
    kind: ResourceKind,
    name: &str,
    namespace: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let root = control_plane_root()?;
    let filename = format!("{}.yaml", slugify(name));
    let path = match kind {
        ResourceKind::Namespace => root.join("namespaces").join(filename),
        _ => root
            .join(kind.directory_name())
            .join(normalize_namespaced_resource_namespace(namespace))
            .join(filename),
    };
    Ok(path)
}

fn control_plane_root() -> anyhow::Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".jarvis").join("control-plane"))
}

fn normalize_namespaced_resource_namespace(namespace: Option<&str>) -> String {
    namespace
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("default")
        .to_string()
}

fn render_summary_table(summaries: &[ResourceSummary]) -> String {
    if summaries.is_empty() {
        return "No control-plane resources found.".to_string();
    }
    let mut lines = vec!["KIND\tNAMESPACE\tNAME\tSTATUS\tDETAIL".to_string()];
    for summary in summaries {
        lines.push(format!(
            "{}\t{}\t{}\t{}\t{}",
            summary.kind,
            summary.namespace.clone().unwrap_or("-".to_string()),
            summary.name,
            summary.status,
            if summary.detail.is_empty() {
                "-".to_string()
            } else {
                summary.detail.clone()
            }
        ));
    }
    lines.join("\n")
}

fn manifest_ref(manifest: &ResourceManifest) -> String {
    match manifest.namespace() {
        Some(namespace) => format!(
            "{} {}/{}",
            manifest.kind().display_name(),
            namespace,
            manifest.name()
        ),
        None => format!("{} {}", manifest.kind().display_name(), manifest.name()),
    }
}

fn collect_runtime_sessions() -> anyhow::Result<Vec<NativeSessionMetadata>> {
    let mut sessions = collect_native_sessions()?;
    sessions.extend(collect_codex_app_sessions()?);
    enrich_native_sessions(&mut sessions)?;
    Ok(sessions)
}

fn load_runtime_session_by_namespace(namespace: &str) -> anyhow::Result<NativeSessionMetadata> {
    collect_runtime_sessions()?
        .into_iter()
        .find(|session| session.namespace == namespace)
        .ok_or_else(|| anyhow!("runtime session '{}' does not exist", namespace))
}

fn delete_runtime_session(session: &NativeSessionMetadata) -> anyhow::Result<()> {
    match session.backend.as_str() {
        "codex-app" => delete_codex_app_session(&session.namespace),
        _ => delete_native_session(&session.namespace),
    }
}

fn parse_specific_kind(kind_arg: ControlPlaneResourceKindArg) -> anyhow::Result<ResourceKind> {
    match kind_arg {
        ControlPlaneResourceKindArg::Namespace => Ok(ResourceKind::Namespace),
        ControlPlaneResourceKindArg::Deployment => Ok(ResourceKind::Deployment),
        ControlPlaneResourceKindArg::ReplicaSet => Ok(ResourceKind::ReplicaSet),
        ControlPlaneResourceKindArg::Job => Ok(ResourceKind::Job),
        ControlPlaneResourceKindArg::CronJob => Ok(ResourceKind::CronJob),
        ControlPlaneResourceKindArg::Application => Ok(ResourceKind::Application),
        ControlPlaneResourceKindArg::Service => Ok(ResourceKind::Service),
        ControlPlaneResourceKindArg::NetworkPolicy => Ok(ResourceKind::NetworkPolicy),
        ControlPlaneResourceKindArg::ConfigMap => Ok(ResourceKind::ConfigMap),
        ControlPlaneResourceKindArg::Secret => Ok(ResourceKind::Secret),
        ControlPlaneResourceKindArg::Volume => Ok(ResourceKind::Volume),
        ControlPlaneResourceKindArg::Worker => Ok(ResourceKind::Worker),
        ControlPlaneResourceKindArg::All => bail!("'all' is not valid for this command"),
    }
}

fn service_route_state_path(
    control_namespace: &str,
    service_name: &str,
) -> anyhow::Result<PathBuf> {
    Ok(control_plane_root()?
        .join("state")
        .join("services")
        .join(control_namespace)
        .join(format!("{}.json", slugify(service_name))))
}

fn load_service_route_state(
    control_namespace: &str,
    service_name: &str,
) -> anyhow::Result<ServiceRouteState> {
    let path = service_route_state_path(control_namespace, service_name)?;
    if !path.exists() {
        return Ok(ServiceRouteState::default());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse '{}'", path.display()))
}

fn save_service_route_state(
    control_namespace: &str,
    service_name: &str,
    state: &ServiceRouteState,
) -> anyhow::Result<()> {
    let path = service_route_state_path(control_namespace, service_name)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    let raw =
        serde_json::to_string_pretty(state).context("failed to encode service route state")?;
    fs::write(&path, raw).with_context(|| format!("failed to write '{}'", path.display()))
}

impl ResourceKind {
    fn directory_name(self) -> &'static str {
        match self {
            ResourceKind::Namespace => "namespaces",
            ResourceKind::Deployment => "deployments",
            ResourceKind::ReplicaSet => "replicasets",
            ResourceKind::Job => "jobs",
            ResourceKind::CronJob => "cronjobs",
            ResourceKind::Application => "applications",
            ResourceKind::Service => "services",
            ResourceKind::NetworkPolicy => "networkpolicies",
            ResourceKind::ConfigMap => "configmaps",
            ResourceKind::Secret => "secrets",
            ResourceKind::Volume => "volumes",
            ResourceKind::Worker => "workers",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            ResourceKind::Namespace => "Namespace",
            ResourceKind::Deployment => "Deployment",
            ResourceKind::ReplicaSet => "ReplicaSet",
            ResourceKind::Job => "Job",
            ResourceKind::CronJob => "CronJob",
            ResourceKind::Application => "Application",
            ResourceKind::Service => "Service",
            ResourceKind::NetworkPolicy => "NetworkPolicy",
            ResourceKind::ConfigMap => "ConfigMap",
            ResourceKind::Secret => "Secret",
            ResourceKind::Volume => "Volume",
            ResourceKind::Worker => "Worker",
        }
    }

    fn from_manifest_kind(kind: &str) -> anyhow::Result<Self> {
        match kind {
            "Namespace" => Ok(ResourceKind::Namespace),
            "Deployment" => Ok(ResourceKind::Deployment),
            "ReplicaSet" => Ok(ResourceKind::ReplicaSet),
            "Job" => Ok(ResourceKind::Job),
            "CronJob" => Ok(ResourceKind::CronJob),
            "Application" => Ok(ResourceKind::Application),
            "Service" => Ok(ResourceKind::Service),
            "NetworkPolicy" => Ok(ResourceKind::NetworkPolicy),
            "ConfigMap" => Ok(ResourceKind::ConfigMap),
            "Secret" => Ok(ResourceKind::Secret),
            "Volume" => Ok(ResourceKind::Volume),
            "Worker" => Ok(ResourceKind::Worker),
            other => bail!("unsupported manifest kind '{}'", other),
        }
    }
}

impl ResourceManifest {
    fn kind(&self) -> ResourceKind {
        match self {
            ResourceManifest::Namespace(_) => ResourceKind::Namespace,
            ResourceManifest::Deployment(_) => ResourceKind::Deployment,
            ResourceManifest::ReplicaSet(_) => ResourceKind::ReplicaSet,
            ResourceManifest::Job(_) => ResourceKind::Job,
            ResourceManifest::CronJob(_) => ResourceKind::CronJob,
            ResourceManifest::Application(_) => ResourceKind::Application,
            ResourceManifest::Service(_) => ResourceKind::Service,
            ResourceManifest::NetworkPolicy(_) => ResourceKind::NetworkPolicy,
            ResourceManifest::ConfigMap(_) => ResourceKind::ConfigMap,
            ResourceManifest::Secret(_) => ResourceKind::Secret,
            ResourceManifest::Volume(_) => ResourceKind::Volume,
            ResourceManifest::Worker(_) => ResourceKind::Worker,
        }
    }

    fn name(&self) -> &str {
        match self {
            ResourceManifest::Namespace(manifest) => &manifest.metadata.name,
            ResourceManifest::Deployment(manifest) => &manifest.metadata.name,
            ResourceManifest::ReplicaSet(manifest) => &manifest.metadata.name,
            ResourceManifest::Job(manifest) => &manifest.metadata.name,
            ResourceManifest::CronJob(manifest) => &manifest.metadata.name,
            ResourceManifest::Application(manifest) => &manifest.metadata.name,
            ResourceManifest::Service(manifest) => &manifest.metadata.name,
            ResourceManifest::NetworkPolicy(manifest) => &manifest.metadata.name,
            ResourceManifest::ConfigMap(manifest) => &manifest.metadata.name,
            ResourceManifest::Secret(manifest) => &manifest.metadata.name,
            ResourceManifest::Volume(manifest) => &manifest.metadata.name,
            ResourceManifest::Worker(manifest) => &manifest.metadata.name,
        }
    }

    fn namespace(&self) -> Option<&str> {
        match self {
            ResourceManifest::Namespace(_) => None,
            ResourceManifest::Deployment(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::ReplicaSet(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::Job(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::CronJob(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::Application(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::Service(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::NetworkPolicy(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::ConfigMap(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::Secret(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::Volume(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::Worker(manifest) => manifest.metadata.namespace.as_deref(),
        }
    }

    fn metadata_mut(&mut self) -> &mut ResourceMetadata {
        match self {
            ResourceManifest::Namespace(manifest) => &mut manifest.metadata,
            ResourceManifest::Deployment(manifest) => &mut manifest.metadata,
            ResourceManifest::ReplicaSet(manifest) => &mut manifest.metadata,
            ResourceManifest::Job(manifest) => &mut manifest.metadata,
            ResourceManifest::CronJob(manifest) => &mut manifest.metadata,
            ResourceManifest::Application(manifest) => &mut manifest.metadata,
            ResourceManifest::Service(manifest) => &mut manifest.metadata,
            ResourceManifest::NetworkPolicy(manifest) => &mut manifest.metadata,
            ResourceManifest::ConfigMap(manifest) => &mut manifest.metadata,
            ResourceManifest::Secret(manifest) => &mut manifest.metadata,
            ResourceManifest::Volume(manifest) => &mut manifest.metadata,
            ResourceManifest::Worker(manifest) => &mut manifest.metadata,
        }
    }
}

fn default_replicas() -> usize {
    1
}

fn default_agents() -> usize {
    1
}

fn default_revision_history_limit() -> usize {
    10
}

fn default_progress_deadline_seconds() -> u64 {
    600
}

fn default_parallelism() -> usize {
    1
}

fn default_completions() -> usize {
    1
}

fn default_backoff_limit() -> usize {
    1
}

fn default_successful_jobs_history_limit() -> usize {
    3
}

fn default_failed_jobs_history_limit() -> usize {
    1
}

fn default_application_history_limit() -> usize {
    10
}

fn default_true() -> bool {
    true
}

fn now_epoch_ms() -> u128 {
    Utc::now().timestamp_millis() as u128
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::{Mutex, OnceLock};

    fn home_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct TempHomeGuard {
        original_home: Option<OsString>,
        root: PathBuf,
    }

    impl TempHomeGuard {
        fn new(prefix: &str) -> Self {
            let root = env::temp_dir().join(format!("{}-{}", prefix, now_epoch_ms()));
            fs::create_dir_all(&root).unwrap();
            let original_home = env::var_os("HOME");
            unsafe {
                env::set_var("HOME", &root);
            }
            Self {
                original_home,
                root,
            }
        }
    }

    impl Drop for TempHomeGuard {
        fn drop(&mut self) {
            match &self.original_home {
                Some(home) => unsafe {
                    env::set_var("HOME", home);
                },
                None => unsafe {
                    env::remove_var("HOME");
                },
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn write_text_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn parse_worker_manifest_normalizes_namespace_and_defaults_provider() {
        let manifests = parse_manifest_documents(
            r#"apiVersion: jarvisctl.io/v1alpha1
kind: Worker
metadata:
  name: qwen-junior
spec:
  model: qwen3:8b
"#,
        )
        .unwrap();

        let ResourceManifest::Worker(worker) = &manifests[0] else {
            panic!("expected worker manifest");
        };

        assert_eq!(worker.metadata.namespace.as_deref(), Some("default"));
        assert_eq!(worker.spec.model, "qwen3:8b");
        assert!(matches!(worker.spec.provider, WorkerProvider::Ollama));
        assert!(worker.spec.output_mode.is_none());
    }

    #[test]
    fn kubernetes_cron_parser_accepts_five_field_syntax() {
        assert!(parse_kubernetes_cron_schedule("* * * * *").is_ok());
        assert!(parse_kubernetes_cron_schedule("@hourly").is_ok());
    }

    #[test]
    fn merge_yaml_deep_merges_mappings() {
        let mut target: Value = serde_yaml::from_str(
            r#"
spec:
  template:
    labels:
      app: demo
"#,
        )
        .unwrap();
        let patch: Value = serde_yaml::from_str(
            r#"
spec:
  template:
    labels:
      tier: backend
"#,
        )
        .unwrap();

        merge_yaml(&mut target, &patch);

        let labels = target
            .get("spec")
            .and_then(|value| value.get("template"))
            .and_then(|value| value.get("labels"))
            .and_then(Value::as_mapping)
            .unwrap();
        assert_eq!(
            labels.get(Value::from("app")).and_then(Value::as_str),
            Some("demo")
        );
        assert_eq!(
            labels.get(Value::from("tier")).and_then(Value::as_str),
            Some("backend")
        );
    }

    #[test]
    fn job_run_phase_keeps_recently_active_runs_pending_without_completion_record() {
        let phase = job_run_phase(None, None, JobRunPhase::Active, 1_000, 1_000 + 1_000);
        assert_eq!(phase, JobRunPhase::Active);
    }

    #[test]
    fn job_run_phase_fails_stale_active_runs_without_completion_record() {
        let phase = job_run_phase(
            None,
            None,
            JobRunPhase::Active,
            1_000,
            1_000 + JOB_COMPLETION_GRACE_MS,
        );
        assert_eq!(phase, JobRunPhase::Failed);
    }

    #[test]
    fn job_run_phase_succeeds_from_zero_exit_completion_record() {
        let completion = NativeSessionCompletion {
            namespace: "job-demo--run0".to_string(),
            created_at_epoch_ms: 2_000,
            agents: vec![crate::native::NativeAgentCompletion {
                name: "agent0".to_string(),
                exit_code: Some(0),
            }],
        };

        let phase = job_run_phase(None, Some(&completion), JobRunPhase::Active, 1_000, 2_000);
        assert_eq!(phase, JobRunPhase::Succeeded);
    }

    #[test]
    fn resolve_manifest_relative_paths_updates_application_sources() {
        let mut manifests = vec![ResourceManifest::Application(ResourceEnvelope {
            api_version: API_VERSION.to_string(),
            kind: "Application".to_string(),
            metadata: ResourceMetadata {
                name: "demo".to_string(),
                namespace: Some("gitops".to_string()),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
            },
            spec: ApplicationSpec {
                source: ApplicationSourceSpec {
                    path: "overlays/dev".to_string(),
                    repo_url: None,
                    target_revision: None,
                },
                destination: ApplicationDestinationSpec {
                    namespace: Some("gitops".to_string()),
                },
                sync_policy: ApplicationSyncPolicy::default(),
            },
        })];

        resolve_manifest_relative_paths(&mut manifests, Path::new("/tmp/root"));

        let ResourceManifest::Application(application) = &manifests[0] else {
            panic!("expected application manifest");
        };
        assert_eq!(application.spec.source.path, "/tmp/root/overlays/dev");
    }

    #[test]
    fn deployment_template_hash_changes_on_restart_token() {
        let mut manifest = ResourceEnvelope {
            api_version: API_VERSION.to_string(),
            kind: "Deployment".to_string(),
            metadata: ResourceMetadata {
                name: "demo".to_string(),
                namespace: Some("mesh".to_string()),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
            },
            spec: DeploymentSpec {
                replicas: 1,
                agents: 1,
                revision_history_limit: default_revision_history_limit(),
                paused: false,
                progress_deadline_seconds: default_progress_deadline_seconds(),
                restart_token: None,
                strategy: Some(DeploymentStrategy::default()),
                driver: Some(CodexRuntimeDriver::AppServer),
                startup_delay_ms: Some(1500),
                template: DeploymentTemplateSpec {
                    task_note: "/tmp/demo.md".to_string(),
                    ..DeploymentTemplateSpec::default()
                },
            },
        };

        let before = deployment_template_hash(&manifest, &NamespaceSpec::default()).unwrap();
        manifest.spec.restart_token = Some("restart-1".to_string());
        let after = deployment_template_hash(&manifest, &NamespaceSpec::default()).unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn replica_set_runtime_namespaces_encode_revision() {
        let namespaces = replica_set_runtime_namespaces("mesh", "sender", 7, 2);
        assert_eq!(
            namespaces,
            vec![
                "mesh--sender--rev7--r0".to_string(),
                "mesh--sender--rev7--r1".to_string()
            ]
        );
    }

    #[test]
    fn deployment_status_marks_progress_deadline_exceeded() {
        let _home_guard = home_env_lock().lock().unwrap();
        let _temp_home = TempHomeGuard::new("jarvisctl-progress-deadline-test");

        let deployment = ResourceEnvelope {
            api_version: API_VERSION.to_string(),
            kind: "Deployment".to_string(),
            metadata: ResourceMetadata {
                name: "deadline-demo".to_string(),
                namespace: Some("rollout-lab".to_string()),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
            },
            spec: DeploymentSpec {
                replicas: 1,
                agents: 1,
                revision_history_limit: default_revision_history_limit(),
                paused: false,
                progress_deadline_seconds: 1,
                restart_token: None,
                strategy: Some(DeploymentStrategy::default()),
                driver: Some(CodexRuntimeDriver::AppServer),
                startup_delay_ms: Some(0),
                template: DeploymentTemplateSpec {
                    task_note: "/tmp/deadline-demo.md".to_string(),
                    ..DeploymentTemplateSpec::default()
                },
            },
        };

        let desired_hash =
            deployment_template_hash(&deployment, &NamespaceSpec::default()).unwrap();
        let mut replica_set = create_replica_set_manifest(
            &deployment,
            &NamespaceSpec::default(),
            1,
            1,
            &desired_hash,
        );
        replica_set.metadata.annotations.insert(
            "jarvisctl.io/created-at-epoch-ms".to_string(),
            now_epoch_ms().saturating_sub(5_000).to_string(),
        );

        save_manifest(&ResourceManifest::Deployment(deployment.clone())).unwrap();
        save_manifest(&ResourceManifest::ReplicaSet(replica_set)).unwrap();

        let status = deployment_status(&deployment).unwrap();
        assert!(status.failed);
        assert!(!status.available);
        assert_eq!(status.updated_replicas, 1);
        assert_eq!(
            deployment_rollout_failure_message(&status).as_deref(),
            Some(
                "ReplicaSet 'deadline-demo-rs-0001' has not reached 1 updated/ready replica(s) within 1s"
            )
        );
    }

    #[test]
    fn application_status_uses_git_source_metadata() {
        let _home_guard = home_env_lock().lock().unwrap();
        let temp_home = TempHomeGuard::new("jarvisctl-application-git-source");
        let repo_dir = temp_home.root.join("repo");
        fs::create_dir_all(repo_dir.join("manifests")).unwrap();
        write_text_file(
            &repo_dir.join("manifests/deployment.yaml"),
            r#"apiVersion: jarvisctl.io/v1alpha1
kind: Deployment
metadata:
  name: git-app
spec:
  replicas: 1
  agents: 1
  template:
    task_note: /tmp/git-app.md
"#,
        );
        ProcessCommand::new("git")
            .arg("init")
            .arg(&repo_dir)
            .output()
            .unwrap();
        ProcessCommand::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["config", "user.email", "codex@example.com"])
            .output()
            .unwrap();
        ProcessCommand::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["config", "user.name", "Codex"])
            .output()
            .unwrap();
        ProcessCommand::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["add", "."])
            .output()
            .unwrap();
        ProcessCommand::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["commit", "-m", "initial"])
            .output()
            .unwrap();

        let head = String::from_utf8(
            ProcessCommand::new("git")
                .arg("-C")
                .arg(&repo_dir)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let application = ResourceEnvelope {
            api_version: API_VERSION.to_string(),
            kind: "Application".to_string(),
            metadata: ResourceMetadata {
                name: "git-source-demo".to_string(),
                namespace: Some("gitops".to_string()),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
            },
            spec: ApplicationSpec {
                source: ApplicationSourceSpec {
                    path: repo_dir.join("manifests").display().to_string(),
                    repo_url: None,
                    target_revision: Some("HEAD".to_string()),
                },
                destination: ApplicationDestinationSpec {
                    namespace: Some("gitops".to_string()),
                },
                sync_policy: ApplicationSyncPolicy::default(),
            },
        };

        let clean_status = application_status(&application).unwrap();
        assert_eq!(clean_status.source_type, "git");
        assert_eq!(
            clean_status.source_root.as_deref(),
            Some(repo_dir.to_string_lossy().as_ref())
        );
        assert_eq!(clean_status.source_revision, head);
        assert!(!clean_status.source_dirty);

        write_text_file(
            &repo_dir.join("manifests/deployment.yaml"),
            r#"apiVersion: jarvisctl.io/v1alpha1
kind: Deployment
metadata:
  name: git-app
spec:
  replicas: 2
  agents: 1
  template:
    task_note: /tmp/git-app.md
"#,
        );

        let dirty_status = application_status(&application).unwrap();
        assert_eq!(dirty_status.source_type, "git");
        assert!(dirty_status.source_dirty);
    }

    #[test]
    fn application_status_uses_remote_git_source_metadata() {
        let _home_guard = home_env_lock().lock().unwrap();
        let temp_home = TempHomeGuard::new("jarvisctl-application-remote-git-source");
        let repo_dir = temp_home.root.join("repo");
        let remote_dir = temp_home.root.join("remote.git");
        fs::create_dir_all(repo_dir.join("manifests")).unwrap();
        write_text_file(
            &repo_dir.join("manifests/configmap.yaml"),
            r#"apiVersion: jarvisctl.io/v1alpha1
kind: ConfigMap
metadata:
  name: remote-git-config
spec:
  data:
    MODE: demo
"#,
        );
        ProcessCommand::new("git")
            .arg("init")
            .arg(&repo_dir)
            .output()
            .unwrap();
        ProcessCommand::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["config", "user.email", "codex@example.com"])
            .output()
            .unwrap();
        ProcessCommand::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["config", "user.name", "Codex"])
            .output()
            .unwrap();
        ProcessCommand::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["add", "."])
            .output()
            .unwrap();
        ProcessCommand::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["commit", "-m", "initial"])
            .output()
            .unwrap();
        ProcessCommand::new("git")
            .arg("clone")
            .args([
                "--bare",
                repo_dir.to_string_lossy().as_ref(),
                remote_dir.to_string_lossy().as_ref(),
            ])
            .output()
            .unwrap();

        let head = String::from_utf8(
            ProcessCommand::new("git")
                .arg("-C")
                .arg(&repo_dir)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let application = ResourceEnvelope {
            api_version: API_VERSION.to_string(),
            kind: "Application".to_string(),
            metadata: ResourceMetadata {
                name: "remote-git-source-demo".to_string(),
                namespace: Some("gitops".to_string()),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
            },
            spec: ApplicationSpec {
                source: ApplicationSourceSpec {
                    path: "manifests".to_string(),
                    repo_url: Some(remote_dir.display().to_string()),
                    target_revision: Some("HEAD".to_string()),
                },
                destination: ApplicationDestinationSpec {
                    namespace: Some("gitops".to_string()),
                },
                sync_policy: ApplicationSyncPolicy::default(),
            },
        };

        let status = application_status(&application).unwrap();
        assert_eq!(status.source_type, "git_remote");
        assert_eq!(
            status.repo_url.as_deref(),
            Some(remote_dir.to_string_lossy().as_ref())
        );
        assert_eq!(status.source_revision, head);
        assert!(!status.source_dirty);
    }

    #[test]
    fn application_diff_reports_create_update_and_delete() {
        let _home_guard = home_env_lock().lock().unwrap();
        let temp_home = TempHomeGuard::new("jarvisctl-application-diff");
        let source_dir = temp_home.root.join("app-src");
        fs::create_dir_all(&source_dir).unwrap();
        write_text_file(
            &source_dir.join("configmap.yaml"),
            r#"apiVersion: jarvisctl.io/v1alpha1
kind: ConfigMap
metadata:
  name: diff-config
spec:
  data:
    MODE: desired
"#,
        );

        let application = ResourceEnvelope {
            api_version: API_VERSION.to_string(),
            kind: "Application".to_string(),
            metadata: ResourceMetadata {
                name: "diff-demo".to_string(),
                namespace: Some("gitops".to_string()),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
            },
            spec: ApplicationSpec {
                source: ApplicationSourceSpec {
                    path: source_dir.display().to_string(),
                    repo_url: None,
                    target_revision: None,
                },
                destination: ApplicationDestinationSpec {
                    namespace: Some("gitops".to_string()),
                },
                sync_policy: ApplicationSyncPolicy::default(),
            },
        };

        save_manifest(&ResourceManifest::Application(application.clone())).unwrap();

        let create_diff = application_diff("diff-demo", Some("gitops")).unwrap();
        assert_eq!(create_diff.creates, 1);
        assert_eq!(create_diff.updates, 0);
        assert_eq!(create_diff.deletes, 0);

        sync_application(&application, true).unwrap();
        let synced_diff = application_diff("diff-demo", Some("gitops")).unwrap();
        assert!(synced_diff.changes.is_empty());

        let ResourceManifest::ConfigMap(mut configmap) =
            load_manifest(ResourceKind::ConfigMap, "diff-config", Some("gitops")).unwrap()
        else {
            panic!("expected configmap");
        };
        configmap
            .spec
            .data
            .insert("MODE".to_string(), "drifted".to_string());
        save_manifest(&ResourceManifest::ConfigMap(configmap)).unwrap();

        let update_diff = application_diff("diff-demo", Some("gitops")).unwrap();
        assert_eq!(update_diff.creates, 0);
        assert_eq!(update_diff.updates, 1);
        assert_eq!(update_diff.deletes, 0);

        fs::remove_file(source_dir.join("configmap.yaml")).unwrap();
        let delete_diff = application_diff("diff-demo", Some("gitops")).unwrap();
        assert_eq!(delete_diff.creates, 0);
        assert_eq!(delete_diff.updates, 0);
        assert_eq!(delete_diff.deletes, 1);
    }

    #[test]
    fn application_rollout_revision_two_preserves_replica_set_history() {
        let _home_guard = home_env_lock().lock().unwrap();
        let temp_home = TempHomeGuard::new("jarvisctl-app-rollout-test");
        let source_dir = temp_home.root.join("app-src");
        write_text_file(
            &source_dir.join("kustomization.yaml"),
            "resources:\n  - deployment.yaml\n",
        );
        write_text_file(
            &source_dir.join("deployment.yaml"),
            r#"apiVersion: jarvisctl.io/v1alpha1
kind: Deployment
metadata:
  name: app-rollout
spec:
  replicas: 1
  agents: 1
  driver: cli_pty
  startupDelayMs: 0
  template:
    task_note: /tmp/jarvisctl-rollout-demo-ticket.md
    working_directory: /home/rootster/documents/jarvisctl
    operator_message: app revision one
    command:
      - /bin/sh
      - -lc
      - sleep 30
"#,
        );

        save_manifest(&ResourceManifest::Namespace(ResourceEnvelope {
            api_version: API_VERSION.to_string(),
            kind: "Namespace".to_string(),
            metadata: ResourceMetadata {
                name: "app-revision-lab".to_string(),
                namespace: None,
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
            },
            spec: NamespaceSpec::default(),
        }))
        .unwrap();

        let application_v1 = ResourceEnvelope {
            api_version: API_VERSION.to_string(),
            kind: "Application".to_string(),
            metadata: ResourceMetadata {
                name: "app-revision-two".to_string(),
                namespace: Some("app-revision-lab".to_string()),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
            },
            spec: ApplicationSpec {
                source: ApplicationSourceSpec {
                    path: source_dir.display().to_string(),
                    repo_url: None,
                    target_revision: Some("rev1".to_string()),
                },
                destination: ApplicationDestinationSpec {
                    namespace: Some("app-revision-lab".to_string()),
                },
                sync_policy: ApplicationSyncPolicy::default(),
            },
        };

        save_manifest(&ResourceManifest::Application(application_v1.clone())).unwrap();
        reconcile_application(&application_v1).unwrap();
        let ResourceManifest::Deployment(deployment_v1) = load_manifest(
            ResourceKind::Deployment,
            "app-rollout",
            Some("app-revision-lab"),
        )
        .unwrap() else {
            panic!("expected deployment manifest");
        };
        let namespace_defaults = NamespaceSpec::default();
        let revision_one_hash =
            deployment_template_hash(&deployment_v1, &namespace_defaults).unwrap();
        let mut replica_set_v1 = create_replica_set_manifest(
            &deployment_v1,
            &namespace_defaults,
            1,
            1,
            &revision_one_hash,
        );
        save_manifest(&ResourceManifest::ReplicaSet(replica_set_v1.clone())).unwrap();

        write_text_file(
            &source_dir.join("deployment.yaml"),
            r#"apiVersion: jarvisctl.io/v1alpha1
kind: Deployment
metadata:
  name: app-rollout
spec:
  replicas: 1
  agents: 1
  driver: cli_pty
  startupDelayMs: 0
  template:
    task_note: /tmp/jarvisctl-rollout-demo-ticket.md
    working_directory: /home/rootster/documents/jarvisctl
    operator_message: app revision two
    command:
      - /bin/sh
      - -lc
      - sleep 30
"#,
        );

        let application_v2 = ResourceEnvelope {
            spec: ApplicationSpec {
                source: ApplicationSourceSpec {
                    path: source_dir.display().to_string(),
                    repo_url: None,
                    target_revision: Some("rev2".to_string()),
                },
                destination: ApplicationDestinationSpec {
                    namespace: Some("app-revision-lab".to_string()),
                },
                sync_policy: ApplicationSyncPolicy::default(),
            },
            ..application_v1.clone()
        };

        save_manifest(&ResourceManifest::Application(application_v2.clone())).unwrap();
        reconcile_application(&application_v2).unwrap();
        let ResourceManifest::Deployment(deployment_v2) = load_manifest(
            ResourceKind::Deployment,
            "app-rollout",
            Some("app-revision-lab"),
        )
        .unwrap() else {
            panic!("expected deployment manifest");
        };
        let revision_two_hash =
            deployment_template_hash(&deployment_v2, &namespace_defaults).unwrap();
        replica_set_v1.spec.replicas = 0;
        save_manifest(&ResourceManifest::ReplicaSet(replica_set_v1.clone())).unwrap();
        let replica_set_v2 = create_replica_set_manifest(
            &deployment_v2,
            &namespace_defaults,
            2,
            1,
            &revision_two_hash,
        );
        save_manifest(&ResourceManifest::ReplicaSet(replica_set_v2)).unwrap();

        let status = deployment_status(&deployment_v2).unwrap();
        assert_eq!(status.current_revision, Some(2));
        assert_eq!(
            status.current_replica_set.as_deref(),
            Some("app-rollout-rs-0002")
        );

        let history = deployment_rollout_history(&deployment_v2).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].revision, 2);
        assert_eq!(history[0].replica_set, "app-rollout-rs-0002");
        assert_eq!(history[1].revision, 1);
        assert_eq!(history[1].replica_set, "app-rollout-rs-0001");
        assert_ne!(history[0].template_hash, history[1].template_hash);

        let application_status = application_status(&application_v2).unwrap();
        assert_eq!(application_status.sync_status, "Synced");
        assert_eq!(application_status.history.len(), 2);

        assert_eq!(
            replica_set_runtime_namespaces("app-revision-lab", "app-rollout", 2, 1),
            vec!["app-revision-lab--app-rollout--rev2--r0".to_string()]
        );
    }
}
