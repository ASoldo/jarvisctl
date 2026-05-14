use crate::SessionBackend;
use crate::codex::{
    CodexLaunchOptions, CodexRuntimeDriver, codex_app_manifest_from_prepared,
    enrich_native_sessions, launch_codex_ticket, prepare_codex_ticket_launch,
};
use crate::codex_app::{collect_codex_app_sessions, delete_codex_app_session};
use crate::native::{
    NativeSessionMetadata, RuntimeContextMetadata, collect_native_sessions, delete_native_session,
};
use crate::ticket::slugify;
use anyhow::{Context, anyhow, bail, ensure};
use chrono::Utc;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_yaml::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tracing::error;

mod kubernetes;
mod reporting;
mod storage;

use kubernetes::*;
pub use kubernetes::{apply_kubernetes_resources, render_kubernetes_resources};
use reporting::*;
pub use reporting::{
    render_describe_output, render_get_output, render_rollout_history_output,
    render_rollout_status_output, wait_for_rollout_status_output,
};
use storage::*;

const API_VERSION: &str = "jarvisctl.io/v1alpha1";
const DEFAULT_KUBERNETES_RUNTIME_IMAGE: &str = "node:25-bookworm-slim";
const DEFAULT_KUBERNETES_RUNTIME_CONTROL_PORT: u16 = 47832;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ControlPlaneOutput {
    Table,
    Json,
    Yaml,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum KubernetesRenderOutput {
    Json,
    Yaml,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ControlPlaneResourceKindArg {
    #[value(alias = "node", alias = "nodes")]
    Node,
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
    #[value(alias = "resources")]
    All,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResourceKind {
    Node,
    Namespace,
    Deployment,
    ReplicaSet,
    Service,
    NetworkPolicy,
    ConfigMap,
    Secret,
    Volume,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeSpec {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, rename = "sshHost", skip_serializing_if = "Option::is_none")]
    pub ssh_host: Option<String>,
    #[serde(default, rename = "sshUser", skip_serializing_if = "Option::is_none")]
    pub ssh_user: Option<String>,
    #[serde(
        default,
        rename = "workspaceRoot",
        skip_serializing_if = "Option::is_none"
    )]
    pub workspace_root: Option<String>,
    #[serde(
        default,
        rename = "maxSessions",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_sessions: Option<usize>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub cordoned: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub taints: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub capabilities: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct NodeRegisterOptions {
    pub name: String,
    pub address: Option<String>,
    pub ssh_host: Option<String>,
    pub ssh_user: Option<String>,
    pub roles: Vec<String>,
    pub labels: BTreeMap<String, String>,
    pub workspace_root: Option<String>,
    pub max_sessions: Option<usize>,
    pub local: bool,
}

#[derive(Debug, Clone)]
pub struct NodeVisitOptions {
    pub node: String,
    pub prompt: String,
    pub working_directory: Option<String>,
    pub namespace: Option<String>,
    pub timeout_seconds: u64,
    pub sandbox_mode: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub ephemeral: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeVisitResult {
    pub node: String,
    pub namespace: String,
    pub exit_status: i32,
    pub final_message: String,
    pub stdout: String,
    pub stderr: String,
    pub cleanup_status: String,
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
    #[serde(
        default,
        rename = "accessPolicy",
        skip_serializing_if = "ResourceAccessPolicy::is_empty"
    )]
    pub access_policy: ResourceAccessPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SecretSpec {
    #[serde(
        default,
        rename = "stringData",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub string_data: BTreeMap<String, String>,
    #[serde(
        default,
        rename = "accessPolicy",
        skip_serializing_if = "ResourceAccessPolicy::is_empty"
    )]
    pub access_policy: ResourceAccessPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VolumeSpec {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(
        default,
        rename = "accessPolicy",
        skip_serializing_if = "ResourceAccessPolicy::is_empty"
    )]
    pub access_policy: ResourceAccessPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ServiceRoutingStrategy {
    #[default]
    FirstReady,
    RoundRobin,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTargetKind {
    #[default]
    Runtime,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServiceSpec {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub selector: BTreeMap<String, String>,
    #[serde(default)]
    pub strategy: ServiceRoutingStrategy,
    #[serde(
        default,
        rename = "targetKind",
        skip_serializing_if = "Option::is_none"
    )]
    pub target_kind: Option<ServiceTargetKind>,
    #[serde(
        default,
        rename = "allowedIntents",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub allowed_intents: Vec<String>,
    #[serde(
        default,
        rename = "accessPolicy",
        skip_serializing_if = "ResourceAccessPolicy::is_empty"
    )]
    pub access_policy: ResourceAccessPolicy,
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
pub struct ResourceAccessPolicy {
    #[serde(
        default,
        rename = "allowedNamespaces",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub allowed_namespaces: Vec<String>,
    #[serde(
        default,
        rename = "workloadSelector",
        skip_serializing_if = "Option::is_none"
    )]
    pub workload_selector: Option<LabelSelector>,
}

impl ResourceAccessPolicy {
    fn is_empty(&self) -> bool {
        self.allowed_namespaces.is_empty() && self.workload_selector.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EnvBindingRef {
    Name(String),
    Ref(NamedEnvBindingRef),
}

impl Default for EnvBindingRef {
    fn default() -> Self {
        Self::Name(String::new())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NamedEnvBindingRef {
    pub name: String,
    #[serde(default)]
    pub optional: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum VolumeBindingRef {
    Name(String),
    Ref(NamedVolumeBindingRef),
}

impl Default for VolumeBindingRef {
    fn default() -> Self {
        Self::Name(String::new())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NamedVolumeBindingRef {
    pub name: String,
    #[serde(default)]
    pub optional: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
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
    pub config_maps: Vec<EnvBindingRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<EnvBindingRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<VolumeBindingRef>,
    #[serde(
        default,
        rename = "nodeSelector",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub node_selector: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerations: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubernetes: Option<KubernetesRuntimeSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KubernetesHostPathMount {
    #[serde(rename = "hostPath")]
    pub host_path: String,
    #[serde(rename = "mountPath")]
    pub mount_path: String,
    #[serde(default, rename = "readOnly", skip_serializing_if = "is_false")]
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KubernetesRuntimeSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(
        default,
        rename = "imagePullPolicy",
        skip_serializing_if = "Option::is_none"
    )]
    pub image_pull_policy: Option<String>,
    #[serde(
        default,
        rename = "serviceAccountName",
        skip_serializing_if = "Option::is_none"
    )]
    pub service_account_name: Option<String>,
    #[serde(
        default,
        rename = "controlPort",
        skip_serializing_if = "Option::is_none"
    )]
    pub control_port: Option<u16>,
    #[serde(
        default,
        rename = "workspaceHostPath",
        skip_serializing_if = "Option::is_none"
    )]
    pub workspace_host_path: Option<String>,
    #[serde(
        default,
        rename = "workspaceMountPath",
        skip_serializing_if = "Option::is_none"
    )]
    pub workspace_mount_path: Option<String>,
    #[serde(
        default,
        rename = "hostPathMounts",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub host_path_mounts: Vec<KubernetesHostPathMount>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
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
    #[serde(default)]
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

fn is_false(value: &bool) -> bool {
    !*value
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
    Node(ResourceEnvelope<NodeSpec>),
    Namespace(ResourceEnvelope<NamespaceSpec>),
    Deployment(ResourceEnvelope<DeploymentSpec>),
    ReplicaSet(ResourceEnvelope<ReplicaSetSpec>),
    Service(ResourceEnvelope<ServiceSpec>),
    NetworkPolicy(ResourceEnvelope<NetworkPolicySpec>),
    ConfigMap(ResourceEnvelope<ConfigMapSpec>),
    Secret(ResourceEnvelope<SecretSpec>),
    Volume(ResourceEnvelope<VolumeSpec>),
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
struct NodeStatus {
    available: bool,
    schedulable: bool,
    roles: Vec<String>,
    address: Option<String>,
    ssh_target: Option<String>,
    architecture: Option<String>,
    operating_system: Option<String>,
    codex: Option<String>,
    codex_auth: Option<String>,
    jarvisctl: Option<String>,
    message: String,
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
    config_maps: Vec<EnvBindingStatus>,
    secrets: Vec<EnvBindingStatus>,
    volumes: Vec<VolumeBindingStatus>,
    replica_sets: Vec<ReplicaSetStatus>,
    sessions: Vec<String>,
    conditions: Vec<StatusCondition>,
    events: Vec<StatusEvent>,
}

#[derive(Debug, Clone, Serialize)]
struct ReplicaSetStatus {
    deployment_name: String,
    revision: u64,
    template_hash: String,
    replicas: usize,
    ready_replicas: usize,
    config_maps: Vec<EnvBindingStatus>,
    secrets: Vec<EnvBindingStatus>,
    volumes: Vec<VolumeBindingStatus>,
    sessions: Vec<String>,
    active: bool,
}

#[derive(Debug, Clone, Serialize)]
struct StatusCondition {
    #[serde(rename = "type")]
    condition_type: String,
    status: String,
    reason: String,
    message: String,
    last_transition_epoch_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
struct StatusEvent {
    #[serde(rename = "type")]
    event_type: String,
    reason: String,
    message: String,
    epoch_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    related: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ResourceAccessPolicyStatus {
    allowed_namespaces: Vec<String>,
    workload_selector: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
struct EnvBindingStatus {
    name: String,
    optional: bool,
    prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct VolumeBindingStatus {
    name: String,
    optional: bool,
    paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ServiceStatus {
    target_kind: String,
    endpoints: Vec<String>,
    strategy: ServiceRoutingStrategy,
    allowed_intents: Vec<String>,
    access_policy: ResourceAccessPolicyStatus,
}

#[derive(Debug, Clone, Serialize)]
struct NetworkPolicyStatus {
    selected_sessions: Vec<String>,
    policy_types: Vec<NetworkPolicyType>,
}

#[derive(Debug, Clone, Serialize)]
struct ConfigMapStatus {
    entries: usize,
    keys: Vec<String>,
    access_policy: ResourceAccessPolicyStatus,
}

#[derive(Debug, Clone, Serialize)]
struct SecretStatus {
    keys: Vec<String>,
    access_policy: ResourceAccessPolicyStatus,
}

#[derive(Debug, Clone, Serialize)]
struct VolumeStatus {
    paths: Vec<String>,
    access_policy: ResourceAccessPolicyStatus,
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

pub fn register_node(options: NodeRegisterOptions) -> anyhow::Result<Vec<String>> {
    let mut metadata = ResourceMetadata {
        name: options.name.trim().to_string(),
        namespace: None,
        labels: options.labels,
        annotations: BTreeMap::new(),
    };
    normalize_metadata(&mut metadata, true)?;
    if options.local {
        metadata
            .labels
            .insert("jarvisctl.io/local".to_string(), "true".to_string());
    }

    let mut capabilities = BTreeMap::new();
    if options.local {
        capabilities.extend(probe_local_capabilities());
    }

    let manifest = ResourceEnvelope {
        api_version: API_VERSION.to_string(),
        kind: "Node".to_string(),
        metadata,
        spec: NodeSpec {
            roles: options.roles,
            address: options.address,
            ssh_host: options.ssh_host,
            ssh_user: options.ssh_user,
            workspace_root: options.workspace_root,
            max_sessions: options.max_sessions,
            cordoned: false,
            taints: Vec::new(),
            capabilities,
        },
    };
    validate_node(&manifest)?;
    let name = manifest.metadata.name.clone();
    save_manifest(&ResourceManifest::Node(manifest))?;
    Ok(vec![format!("registered Node {}", name)])
}

pub fn set_node_cordoned(name: &str, cordoned: bool) -> anyhow::Result<String> {
    let manifest = load_manifest(ResourceKind::Node, name, None)?;
    let ResourceManifest::Node(mut node) = manifest else {
        bail!("resource '{}' is not a Node", name);
    };
    node.spec.cordoned = cordoned;
    save_manifest(&ResourceManifest::Node(node.clone()))?;
    Ok(format!(
        "{} Node {}",
        if cordoned { "cordoned" } else { "uncordoned" },
        node.metadata.name
    ))
}

pub fn sync_codex_auth_to_node(name: &str) -> anyhow::Result<String> {
    let manifest = load_manifest(ResourceKind::Node, name, None)?;
    let ResourceManifest::Node(node) = manifest else {
        bail!("resource '{}' is not a Node", name);
    };
    let files = local_codex_auth_files()?;
    if node_is_local(&node) {
        return Ok(format!(
            "Node '{}' is local; Codex auth already lives in {}",
            node.metadata.name,
            local_codex_dir()?.display()
        ));
    }
    sync_codex_auth_to_remote_node(&node, &files)?;
    Ok(format!(
        "Synced {} Codex auth/config file(s) to Node '{}' over SSH",
        files.len(),
        node.metadata.name
    ))
}

pub fn run_node_visit(options: NodeVisitOptions) -> anyhow::Result<NodeVisitResult> {
    ensure!(
        !options.prompt.trim().is_empty(),
        "visit prompt must not be empty"
    );
    let manifest = load_manifest(ResourceKind::Node, &options.node, None)?;
    let ResourceManifest::Node(node) = manifest else {
        bail!("resource '{}' is not a Node", options.node);
    };
    ensure!(
        !node_is_local(&node),
        "visit currently targets remote SSH nodes; '{}' is local",
        node.metadata.name
    );
    let target = node_ssh_target(&node.spec)
        .ok_or_else(|| anyhow!("Node '{}' has no SSH target", node.metadata.name))?;
    let namespace = options
        .namespace
        .clone()
        .unwrap_or_else(|| format!("visit-{}-{}", slugify(&node.metadata.name), now_epoch_ms()));
    let auth_files = local_codex_auth_files()?;
    sync_codex_auth_to_remote_node_for_namespace(&node, &auth_files, &namespace).with_context(
        || {
            format!(
                "failed to sync leased Codex auth before visit '{}' on Node '{}'",
                namespace, node.metadata.name
            )
        },
    )?;

    let visit_result = run_remote_codex_exec_visit(&target, &options, &namespace);
    let cleanup_result = cleanup_codex_auth_lease_on_remote_node(&node, &namespace);

    let mut result = visit_result?;
    result.cleanup_status = match cleanup_result {
        Ok(()) => "restored".to_string(),
        Err(error) => format!("failed: {error}"),
    };
    ensure!(
        result.cleanup_status == "restored",
        "visit '{}' completed, but Codex auth cleanup failed on Node '{}': {}",
        namespace,
        node.metadata.name,
        result.cleanup_status
    );
    Ok(result)
}

pub fn render_node_probe_output(name: &str, output: ControlPlaneOutput) -> anyhow::Result<String> {
    let manifest = load_manifest(ResourceKind::Node, name, None)?;
    let ResourceManifest::Node(node) = manifest else {
        bail!("resource '{}' is not a Node", name);
    };
    let status = probe_node_status(&node);
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(&status).context("failed to encode node status")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(&status).context("failed to encode node status")
        }
        ControlPlaneOutput::Table => Ok(render_node_status_table(&node.metadata.name, &status)),
    }
}

pub fn attach_cluster_runtime_session(namespace: &str, agent: &str) -> anyhow::Result<bool> {
    let Some(node) = remote_node_for_runtime_session(namespace)? else {
        return Ok(false);
    };
    run_remote_runtime_command_interactive(
        &node,
        vec![
            "jarvisctl".to_string(),
            "attach".to_string(),
            "--namespace".to_string(),
            namespace.to_string(),
            "--agent".to_string(),
            agent.to_string(),
        ],
    )?;
    Ok(true)
}

pub fn tell_cluster_runtime_session(
    namespace: &str,
    agent: &str,
    contents: &str,
    mode: &str,
) -> anyhow::Result<bool> {
    let Some(node) = remote_node_for_runtime_session(namespace)? else {
        return Ok(false);
    };
    run_remote_runtime_command(
        &node,
        vec![
            "jarvisctl".to_string(),
            "tell".to_string(),
            "--namespace".to_string(),
            namespace.to_string(),
            "--agent".to_string(),
            agent.to_string(),
            "--text".to_string(),
            contents.to_string(),
            "--mode".to_string(),
            mode.to_string(),
        ],
    )?;
    Ok(true)
}

pub fn interrupt_cluster_runtime_session(namespace: &str, agent: &str) -> anyhow::Result<bool> {
    let Some(node) = remote_node_for_runtime_session(namespace)? else {
        return Ok(false);
    };
    run_remote_runtime_command(
        &node,
        vec![
            "jarvisctl".to_string(),
            "interrupt".to_string(),
            "--namespace".to_string(),
            namespace.to_string(),
            "--agent".to_string(),
            agent.to_string(),
        ],
    )?;
    Ok(true)
}

pub fn delete_cluster_runtime_session(namespace: &str) -> anyhow::Result<bool> {
    let Some(node) = remote_node_for_runtime_session(namespace)? else {
        return Ok(false);
    };
    delete_remote_runtime_session(&node, namespace)?;
    Ok(true)
}

fn probe_node_status(node: &ResourceEnvelope<NodeSpec>) -> NodeStatus {
    let mut status = NodeStatus {
        available: false,
        schedulable: !node.spec.cordoned && node.spec.taints.is_empty(),
        roles: node.spec.roles.clone(),
        address: node.spec.address.clone(),
        ssh_target: node_ssh_target(&node.spec),
        architecture: None,
        operating_system: None,
        codex: None,
        codex_auth: None,
        jarvisctl: None,
        message: String::new(),
    };

    let command_output = if node_is_local(node) {
        run_probe_command(None)
    } else {
        run_probe_command(
            status
                .ssh_target
                .as_deref()
                .or(node.spec.address.as_deref()),
        )
    };

    match command_output {
        Ok(output) => {
            let values = parse_probe_output(&output);
            status.architecture = values
                .get("arch")
                .cloned()
                .or_else(|| node.spec.capabilities.get("arch").cloned());
            status.operating_system = values
                .get("os")
                .cloned()
                .or_else(|| node.spec.capabilities.get("os").cloned());
            status.codex = values
                .get("codex")
                .cloned()
                .or_else(|| node.spec.capabilities.get("codex").cloned());
            status.codex_auth = values
                .get("codex_auth")
                .cloned()
                .or_else(|| node.spec.capabilities.get("codex_auth").cloned());
            status.jarvisctl = values
                .get("jarvisctl")
                .cloned()
                .or_else(|| node.spec.capabilities.get("jarvisctl").cloned());
            status.available = true;
            status.message = "probe succeeded".to_string();
        }
        Err(error) => {
            status.message = error.to_string();
        }
    }
    status
}

fn render_node_status_table(name: &str, status: &NodeStatus) -> String {
    let target = status
        .ssh_target
        .as_deref()
        .or(status.address.as_deref())
        .unwrap_or("local");
    format!(
        "NAME\tAVAILABLE\tSCHEDULABLE\tTARGET\tARCH\tCODEX\tCODEX_AUTH\tJARVISCTL\tMESSAGE\n{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        name,
        status.available,
        status.schedulable,
        target,
        status.architecture.as_deref().unwrap_or("-"),
        status.codex.as_deref().unwrap_or("-"),
        status.codex_auth.as_deref().unwrap_or("-"),
        status.jarvisctl.as_deref().unwrap_or("-"),
        status.message
    )
}

fn node_is_local(node: &ResourceEnvelope<NodeSpec>) -> bool {
    node.metadata
        .labels
        .get("jarvisctl.io/local")
        .map(String::as_str)
        == Some("true")
        || node.spec.ssh_host.as_deref().is_none()
            && node.spec.address.as_deref().is_none()
            && node.metadata.name == local_hostname().unwrap_or_default()
}

fn node_ssh_target(spec: &NodeSpec) -> Option<String> {
    let host = spec.ssh_host.as_deref().or(spec.address.as_deref())?;
    Some(match spec.ssh_user.as_deref() {
        Some(user) if !user.trim().is_empty() => format!("{user}@{host}"),
        _ => host.to_string(),
    })
}

fn run_probe_command(target: Option<&str>) -> anyhow::Result<String> {
    let script = "printf 'arch='; uname -m 2>/dev/null || true; printf '\\nos='; uname -s 2>/dev/null || true; printf '\\ncodex='; (codex --version 2>/dev/null || command -v codex 2>/dev/null || true) | head -n 1; printf '\\njarvisctl='; (jarvisctl --version 2>/dev/null || command -v jarvisctl 2>/dev/null || true) | head -n 1; printf '\\ncodex_auth='; test -s \"$HOME/.codex/auth.json\" && echo present || echo missing; printf '\\n'";
    let output = if let Some(target) = target {
        ProcessCommand::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                target,
                script,
            ])
            .output()
            .with_context(|| format!("failed to run ssh probe for '{target}'"))?
    } else {
        ProcessCommand::new("sh")
            .args(["-lc", script])
            .output()
            .context("failed to run local node probe")?
    };
    if !output.status.success() {
        bail!(
            "probe exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn parse_probe_output(output: &str) -> BTreeMap<String, String> {
    output
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            let value = value.trim();
            if value.is_empty() {
                None
            } else {
                Some((key.trim().to_string(), value.to_string()))
            }
        })
        .collect()
}

fn probe_local_capabilities() -> BTreeMap<String, String> {
    run_probe_command(None)
        .map(|output| parse_probe_output(&output))
        .unwrap_or_default()
}

fn local_codex_dir() -> anyhow::Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".codex"))
}

fn local_codex_auth_files() -> anyhow::Result<Vec<String>> {
    let codex_dir = local_codex_dir()?;
    let candidates = ["auth.json", "config.toml", "version.json"];
    let mut files = Vec::new();
    for candidate in candidates {
        if codex_dir.join(candidate).is_file() {
            files.push(candidate.to_string());
        }
    }
    ensure!(
        files.iter().any(|file| file == "auth.json"),
        "local Codex auth file '{}' does not exist; run `codex login` on this machine first",
        codex_dir.join("auth.json").display()
    );
    Ok(files)
}

fn sync_codex_auth_to_remote_node(
    node: &ResourceEnvelope<NodeSpec>,
    files: &[String],
) -> anyhow::Result<()> {
    sync_codex_auth_to_remote_node_with_lease(node, files, None)
}

fn sync_codex_auth_to_remote_node_for_namespace(
    node: &ResourceEnvelope<NodeSpec>,
    files: &[String],
    runtime_namespace: &str,
) -> anyhow::Result<()> {
    sync_codex_auth_to_remote_node_with_lease(node, files, Some(runtime_namespace))
}

fn sync_codex_auth_to_remote_node_with_lease(
    node: &ResourceEnvelope<NodeSpec>,
    files: &[String],
    runtime_namespace: Option<&str>,
) -> anyhow::Result<()> {
    let target = node_ssh_target(&node.spec)
        .ok_or_else(|| anyhow!("Node '{}' has no SSH target", node.metadata.name))?;
    let codex_dir = local_codex_dir()?;
    let mut tar = ProcessCommand::new("tar")
        .arg("-C")
        .arg(&codex_dir)
        .arg("-czf")
        .arg("-")
        .args(files)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!(
                "failed to archive local Codex auth from '{}'",
                codex_dir.display()
            )
        })?;

    let lease_name = runtime_namespace.map(auth_lease_name);
    let remote_script = "set -eu; umask 077; mkdir -p \"$HOME/.codex\"; lease=\"${JARVIS_CODEX_AUTH_LEASE:-}\"; if [ -n \"$lease\" ]; then lease_dir=\"$HOME/.jarvis/codex/auth-leases/$lease\"; rm -rf \"$lease_dir.tmp\"; mkdir -p \"$lease_dir.tmp/backup\"; for f in auth.json config.toml version.json; do if [ -e \"$HOME/.codex/$f\" ]; then cp -p \"$HOME/.codex/$f\" \"$lease_dir.tmp/backup/$f\"; else : > \"$lease_dir.tmp/backup/$f.missing\"; fi; done; rm -rf \"$lease_dir\"; mv \"$lease_dir.tmp\" \"$lease_dir\"; fi; tar -xzf - -C \"$HOME/.codex\"; chmod 700 \"$HOME/.codex\"; chmod 600 \"$HOME/.codex/auth.json\" \"$HOME/.codex/config.toml\" \"$HOME/.codex/version.json\" 2>/dev/null || true";
    let mut ssh_args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        target.clone(),
    ];
    if let Some(lease_name) = lease_name.as_deref() {
        ssh_args.push("env".to_string());
        ssh_args.push(format!("JARVIS_CODEX_AUTH_LEASE={lease_name}"));
    }
    ssh_args.extend([
        "sh".to_string(),
        "-lc".to_string(),
        remote_script.to_string(),
    ]);
    let mut ssh = ProcessCommand::new("ssh")
        .args(ssh_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!(
                "failed to open SSH auth sync to Node '{}'",
                node.metadata.name
            )
        })?;

    {
        let mut tar_stdout = tar.stdout.take().context("failed to capture tar stdout")?;
        let mut ssh_stdin = ssh.stdin.take().context("failed to open ssh stdin")?;
        io::copy(&mut tar_stdout, &mut ssh_stdin)
            .context("failed to stream Codex auth archive over SSH")?;
    }

    let tar_status = tar
        .wait()
        .context("failed waiting for Codex auth archive")?;
    ensure!(
        tar_status.success(),
        "Codex auth archive failed with status {tar_status}"
    );
    let ssh_status = ssh
        .wait()
        .context("failed waiting for remote Codex auth install")?;
    ensure!(
        ssh_status.success(),
        "remote Codex auth install on Node '{}' failed with status {ssh_status}",
        node.metadata.name
    );
    Ok(())
}

fn cleanup_codex_auth_lease_on_remote_node(
    node: &ResourceEnvelope<NodeSpec>,
    runtime_namespace: &str,
) -> anyhow::Result<()> {
    let target = node_ssh_target(&node.spec)
        .ok_or_else(|| anyhow!("Node '{}' has no SSH target", node.metadata.name))?;
    let lease_name = auth_lease_name(runtime_namespace);
    let remote_script = "set -eu; lease=\"${JARVIS_CODEX_AUTH_LEASE:-}\"; [ -n \"$lease\" ] || exit 0; lease_dir=\"$HOME/.jarvis/codex/auth-leases/$lease\"; [ -d \"$lease_dir/backup\" ] || exit 0; mkdir -p \"$HOME/.codex\"; for f in auth.json config.toml version.json; do if [ -e \"$lease_dir/backup/$f\" ]; then cp -p \"$lease_dir/backup/$f\" \"$HOME/.codex/$f\"; elif [ -e \"$lease_dir/backup/$f.missing\" ]; then rm -f \"$HOME/.codex/$f\"; fi; done; chmod 700 \"$HOME/.codex\" 2>/dev/null || true; chmod 600 \"$HOME/.codex/auth.json\" \"$HOME/.codex/config.toml\" \"$HOME/.codex/version.json\" 2>/dev/null || true; rm -rf \"$lease_dir\"";
    let output = ProcessCommand::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            &target,
            "env",
            &format!("JARVIS_CODEX_AUTH_LEASE={lease_name}"),
            "sh",
            "-lc",
            remote_script,
        ])
        .output()
        .with_context(|| {
            format!(
                "failed to clean Codex auth lease for runtime '{}' on Node '{}'",
                runtime_namespace, node.metadata.name
            )
        })?;
    ensure!(
        output.status.success(),
        "Codex auth lease cleanup for runtime '{}' on Node '{}' failed with status {}: {}",
        runtime_namespace,
        node.metadata.name,
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn auth_lease_name(runtime_namespace: &str) -> String {
    slugify(runtime_namespace)
}

fn run_remote_codex_exec_visit(
    target: &str,
    options: &NodeVisitOptions,
    namespace: &str,
) -> anyhow::Result<NodeVisitResult> {
    let mut codex_args = vec![
        "codex".to_string(),
        "exec".to_string(),
        "--skip-git-repo-check".to_string(),
        "--color".to_string(),
        "never".to_string(),
    ];
    if let Some(sandbox_mode) = options.sandbox_mode.as_deref() {
        codex_args.push("--sandbox".to_string());
        codex_args.push(sandbox_mode.to_string());
    }
    if let Some(model) = options.model.as_deref() {
        codex_args.push("--model".to_string());
        codex_args.push(model.to_string());
    }
    if let Some(reasoning_effort) = options.reasoning_effort.as_deref() {
        codex_args.push("-c".to_string());
        codex_args.push(format!("reasoning_effort=\"{reasoning_effort}\""));
    }
    if options.ephemeral {
        codex_args.push("--ephemeral".to_string());
    }

    let cd_command = options
        .working_directory
        .as_deref()
        .map(|path| format!("cd {};", shell_escape(path)))
        .unwrap_or_else(|| "cd;".to_string());
    let output_name = slugify(namespace);
    let remote_script = format!(
        "set -u; mkdir -p \"$HOME/.jarvis/codex/visits\"; out=\"$HOME/.jarvis/codex/visits/{}.last-message.md\"; rm -f \"$out\"; {} {} --output-last-message \"$out\" -; visit_status=$?; printf '\\n__JARVIS_VISIT_LAST_MESSAGE_BEGIN__\\n'; if [ -f \"$out\" ]; then cat \"$out\"; fi; printf '\\n__JARVIS_VISIT_LAST_MESSAGE_END__\\n'; exit \"$visit_status\"",
        output_name,
        cd_command,
        shell_words::join(codex_args),
    );

    let timeout_duration = format!("{}s", options.timeout_seconds);
    let remote_command = shell_words::join([
        "sh".to_string(),
        "-lc".to_string(),
        remote_script.to_string(),
    ]);
    let mut child = ProcessCommand::new("timeout")
        .args([
            "--kill-after=5s",
            &timeout_duration,
            "ssh",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            target,
            &remote_command,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start remote Codex visit on '{target}'"))?;

    {
        let stdin = child.stdin.as_mut().context("failed to open visit stdin")?;
        stdin
            .write_all(options.prompt.as_bytes())
            .context("failed to stream visit prompt to remote Codex")?;
    }

    let output = child
        .wait_with_output()
        .context("failed waiting for remote Codex visit")?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let final_message = extract_visit_final_message(&stdout).unwrap_or_else(|| stdout.clone());
    let exit_status = output.status.code().unwrap_or(-1);
    ensure!(
        output.status.success(),
        "remote Codex visit '{}' on '{}' failed with status {}: {}",
        namespace,
        target,
        output.status,
        stderr.trim()
    );

    Ok(NodeVisitResult {
        node: options.node.clone(),
        namespace: namespace.to_string(),
        exit_status,
        final_message,
        stdout,
        stderr,
        cleanup_status: "pending".to_string(),
    })
}

fn extract_visit_final_message(stdout: &str) -> Option<String> {
    let (_, rest) = stdout.split_once("__JARVIS_VISIT_LAST_MESSAGE_BEGIN__")?;
    let (message, _) = rest.split_once("__JARVIS_VISIT_LAST_MESSAGE_END__")?;
    Some(message.trim().to_string())
}

fn shell_escape(value: &str) -> String {
    shell_words::join([value.to_string()])
}

fn local_hostname() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

impl<T> ResourceEnvelope<T> {
    fn namespace_key(&self) -> &str {
        self.metadata.namespace.as_deref().unwrap_or("default")
    }
}

fn env_binding_name(reference: &EnvBindingRef) -> &str {
    match reference {
        EnvBindingRef::Name(name) => name,
        EnvBindingRef::Ref(reference) => &reference.name,
    }
}

fn env_binding_optional(reference: &EnvBindingRef) -> bool {
    match reference {
        EnvBindingRef::Name(_) => false,
        EnvBindingRef::Ref(reference) => reference.optional,
    }
}

fn env_binding_prefix(reference: &EnvBindingRef) -> Option<&str> {
    match reference {
        EnvBindingRef::Name(_) => None,
        EnvBindingRef::Ref(reference) => reference.prefix.as_deref(),
    }
}

fn volume_binding_name(reference: &VolumeBindingRef) -> &str {
    match reference {
        VolumeBindingRef::Name(name) => name,
        VolumeBindingRef::Ref(reference) => &reference.name,
    }
}

fn volume_binding_optional(reference: &VolumeBindingRef) -> bool {
    match reference {
        VolumeBindingRef::Name(_) => false,
        VolumeBindingRef::Ref(reference) => reference.optional,
    }
}

fn volume_binding_paths(reference: &VolumeBindingRef) -> &[String] {
    match reference {
        VolumeBindingRef::Name(_) => &[],
        VolumeBindingRef::Ref(reference) => &reference.paths,
    }
}

fn env_binding_statuses(references: &[EnvBindingRef]) -> Vec<EnvBindingStatus> {
    references
        .iter()
        .map(|reference| EnvBindingStatus {
            name: env_binding_name(reference).to_string(),
            optional: env_binding_optional(reference),
            prefix: env_binding_prefix(reference).map(ToOwned::to_owned),
        })
        .collect()
}

fn volume_binding_statuses(references: &[VolumeBindingRef]) -> Vec<VolumeBindingStatus> {
    references
        .iter()
        .map(|reference| VolumeBindingStatus {
            name: volume_binding_name(reference).to_string(),
            optional: volume_binding_optional(reference),
            paths: volume_binding_paths(reference).to_vec(),
        })
        .collect()
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
        if !matches!(manifest, ResourceManifest::Deployment(_)) {
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
        if !matches!(manifest, ResourceManifest::Deployment(_)) {
            messages.push(format!("applied {}", manifest_ref(manifest)));
        }
    }
    messages.extend(reconcile_control_plane()?);
    Ok(messages)
}

fn load_source_manifests(
    files: &[PathBuf],
    kustomize: Option<&Path>,
) -> anyhow::Result<Vec<ResourceManifest>> {
    match (files.is_empty(), kustomize) {
        (false, None) => {
            let mut manifests = Vec::new();
            for path in files {
                let raw = fs::read_to_string(path)
                    .with_context(|| format!("failed to read manifest '{}'", path.display()))?;
                let mut parsed = parse_manifest_documents(&raw)?;
                resolve_manifest_relative_paths(
                    &mut parsed,
                    path.parent().unwrap_or_else(|| Path::new(".")),
                );
                manifests.extend(parsed);
            }
            Ok(manifests)
        }
        (true, Some(path)) => render_source_path(path, None, &BTreeMap::new()),
        (false, Some(_)) => bail!("use either --file or --kustomize, not both"),
        (true, None) => bail!("provide at least one --file or one --kustomize path"),
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
    resolve_service_target_for_source(service_name, control_namespace, None, None)
}

pub fn resolve_service_target_for_message(
    service_name: &str,
    control_namespace: Option<&str>,
    source_runtime_namespace: Option<&str>,
) -> anyhow::Result<ServiceResolution> {
    resolve_service_target_for_source(
        service_name,
        control_namespace,
        source_runtime_namespace,
        Some("conversation"),
    )
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
    intent: Option<&str>,
) -> anyhow::Result<ServiceResolution> {
    let namespace = normalize_namespaced_resource_namespace(control_namespace);
    let _ = reconcile_control_plane()?;
    let manifest = load_manifest(ResourceKind::Service, service_name, Some(&namespace))?;
    let ResourceManifest::Service(service) = manifest else {
        bail!("resource '{}' is not a Service", service_name);
    };
    ensure!(
        effective_service_target_kind(&service.spec) == ServiceTargetKind::Runtime,
        "service '{}/{}' targets workers and cannot be used as a runtime endpoint",
        namespace,
        service_name
    );
    ensure!(
        service_allows_intent(&service.spec, intent),
        "service '{}/{}' does not allow intent '{}'",
        namespace,
        service_name,
        intent.unwrap_or("unspecified")
    );

    let source = match source_runtime_namespace {
        Some(namespace) => Some(load_runtime_session_by_namespace(namespace)?),
        None => None,
    };
    if let Some(source) = source.as_ref() {
        let empty_labels = BTreeMap::new();
        let source_context = source.context.as_ref();
        let source_control_namespace = source_context
            .and_then(|context| context.control_namespace.as_deref())
            .unwrap_or(namespace.as_str());
        let source_labels = source_context
            .map(|context| &context.labels)
            .unwrap_or(&empty_labels);
        ensure!(
            service_allows_source_workload(&service.spec, source_control_namespace, source_labels),
            "service '{}/{}' denies source workload '{}' by access policy",
            namespace,
            service_name,
            source.namespace
        );
    }
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
    let _ = manifests;
    let _ = base_dir;
}

fn parse_manifest_value(value: Value) -> anyhow::Result<ResourceManifest> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("manifest is missing kind"))?;
    match kind {
        "Node" => {
            let mut manifest: ResourceEnvelope<NodeSpec> =
                serde_yaml::from_value(value).context("failed to decode Node manifest")?;
            normalize_metadata(&mut manifest.metadata, true)?;
            validate_node(&manifest)?;
            manifest.api_version = API_VERSION.to_string();
            manifest.kind = "Node".to_string();
            Ok(ResourceManifest::Node(manifest))
        }
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
        "Job" | "CronJob" | "Application" => bail!(
            "manifest kind '{}' is no longer part of the supported jarvisctl product surface; keep jarvisctl focused on Codex runtimes, operator control, and repeatable workspaces",
            kind
        ),
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
        "Worker" => bail!(
            "Worker resources are no longer part of the supported jarvisctl product surface; keep jarvisctl focused on Codex runtimes, session control, and repeatable workspaces"
        ),
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

fn validate_node(manifest: &ResourceEnvelope<NodeSpec>) -> anyhow::Result<()> {
    ensure!(
        manifest.spec.max_sessions.unwrap_or(1) > 0,
        "Node '{}' must set spec.maxSessions > 0",
        manifest.metadata.name
    );
    ensure!(
        !manifest
            .spec
            .roles
            .iter()
            .any(|role| role.trim().is_empty()),
        "Node '{}' has an empty role",
        manifest.metadata.name
    );
    ensure!(
        !manifest
            .spec
            .taints
            .iter()
            .any(|taint| taint.trim().is_empty()),
        "Node '{}' has an empty taint",
        manifest.metadata.name
    );
    Ok(())
}

fn validate_deployment(manifest: &ResourceEnvelope<DeploymentSpec>) -> anyhow::Result<()> {
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
                if manifest.spec.replicas == 0 {
                    return Ok(());
                }
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
    validate_template_bindings(
        &manifest.spec.template,
        "Deployment",
        &manifest.metadata.name,
    )?;
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
    validate_template_bindings(
        &manifest.spec.template,
        "ReplicaSet",
        &manifest.metadata.name,
    )?;
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

fn validate_template_bindings(
    template: &DeploymentTemplateSpec,
    kind: &str,
    name: &str,
) -> anyhow::Result<()> {
    for reference in &template.config_maps {
        ensure!(
            !env_binding_name(reference).trim().is_empty(),
            "{} '{}' has an empty configMaps entry",
            kind,
            name
        );
        if matches!(reference, EnvBindingRef::Ref(reference) if reference.prefix.as_deref() == Some(""))
        {
            bail!("{} '{}' has an empty configMaps prefix", kind, name);
        }
    }
    for reference in &template.secrets {
        ensure!(
            !env_binding_name(reference).trim().is_empty(),
            "{} '{}' has an empty secrets entry",
            kind,
            name
        );
        if matches!(reference, EnvBindingRef::Ref(reference) if reference.prefix.as_deref() == Some(""))
        {
            bail!("{} '{}' has an empty secrets prefix", kind, name);
        }
    }
    for reference in &template.volumes {
        ensure!(
            !volume_binding_name(reference).trim().is_empty(),
            "{} '{}' has an empty volumes entry",
            kind,
            name
        );
    }
    if let Some(kubernetes) = template.kubernetes.as_ref() {
        validate_kubernetes_runtime(kubernetes, kind, name)?;
    }
    Ok(())
}

fn reconcile_control_plane() -> anyhow::Result<Vec<String>> {
    let mut messages = Vec::new();
    reconcile_manifest_batch(
        load_manifests_by_kind(ResourceKind::Deployment, None)?,
        &mut messages,
    );

    Ok(messages)
}

fn reconcile_manifest_batch(manifests: Vec<ResourceManifest>, messages: &mut Vec<String>) {
    for manifest in manifests {
        match reconcile_manifest(&manifest) {
            Ok(Some(message)) => messages.push(message),
            Ok(None) => {}
            Err(err) => {
                let scope = manifest
                    .namespace()
                    .map(|namespace| format!("{namespace}/{}", manifest.name()))
                    .unwrap_or_else(|| manifest.name().to_string());
                error!(
                    "failed to reconcile {} '{}': {:#}",
                    manifest.kind().display_name(),
                    scope,
                    err
                );
            }
        }
    }
}

fn reconcile_manifest(manifest: &ResourceManifest) -> anyhow::Result<Option<String>> {
    match manifest {
        ResourceManifest::Deployment(deployment) => reconcile_deployment(deployment).map(Some),
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
    if desired == 0 {
        for replica_set in replica_sets.iter_mut() {
            if replica_set.spec.replicas == 0 {
                continue;
            }
            replica_set.spec.replicas = 0;
            save_manifest(&ResourceManifest::ReplicaSet(replica_set.clone()))?;
        }
        return Ok(());
    }
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
    let scheduled_node = select_node_for_template(&replica_set.spec.template)?;

    let config_maps = load_config_map_values(
        &control_namespace,
        &replica_set.spec.template.config_maps,
        &replica_set.spec.template.labels,
    )?;
    let secrets = load_secret_values(
        &control_namespace,
        &replica_set.spec.template.secrets,
        &replica_set.spec.template.labels,
    )?;
    let volumes = load_volume_paths(
        &control_namespace,
        &replica_set.spec.template.volumes,
        &replica_set.spec.template.labels,
    )?;
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
            scheduled_node.as_ref(),
        ),
    );
    let context_overlay = RuntimeContextMetadata {
        control_namespace: Some(control_namespace.clone()),
        deployment: Some(replica_set.spec.deployment_name.clone()),
        labels: replica_set_runtime_labels(replica_set, ordinal, scheduled_node.as_ref()),
        config_maps: replica_set
            .spec
            .template
            .config_maps
            .iter()
            .map(|reference| env_binding_name(reference).to_string())
            .collect(),
        secrets: replica_set
            .spec
            .template
            .secrets
            .iter()
            .map(|reference| env_binding_name(reference).to_string())
            .collect(),
        volumes: replica_set
            .spec
            .template
            .volumes
            .iter()
            .map(|reference| volume_binding_name(reference).to_string())
            .collect(),
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

    if let Some(node) = scheduled_node.as_ref().filter(|node| !node_is_local(node)) {
        return launch_remote_replica_set_replica(
            node,
            replica_set,
            runtime_namespace,
            working_directory,
            operator_message.or_else(|| replica_set.spec.template.operator_message.clone()),
            images,
            environment,
            context_overlay.labels.clone(),
            startup_delay_ms,
        );
    }

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

fn launch_remote_replica_set_replica(
    node: &ResourceEnvelope<NodeSpec>,
    replica_set: &ResourceEnvelope<ReplicaSetSpec>,
    runtime_namespace: &str,
    working_directory: Option<PathBuf>,
    operator_message: Option<String>,
    images: Vec<PathBuf>,
    environment: BTreeMap<String, String>,
    runtime_labels: BTreeMap<String, String>,
    startup_delay_ms: u64,
) -> anyhow::Result<()> {
    let target = node_ssh_target(&node.spec)
        .ok_or_else(|| anyhow!("Node '{}' has no SSH target", node.metadata.name))?;
    let driver = replica_set
        .spec
        .driver
        .unwrap_or(CodexRuntimeDriver::AppServer);
    ensure!(
        matches!(driver, CodexRuntimeDriver::AppServer),
        "remote Node '{}' currently supports only the codex app-server driver",
        node.metadata.name
    );
    let auth_files = local_codex_auth_files()?;
    sync_codex_auth_to_remote_node_for_namespace(node, &auth_files, runtime_namespace)
        .with_context(|| {
            format!(
                "failed to sync Codex auth before launching remote runtime '{}' on Node '{}'",
                runtime_namespace, node.metadata.name
            )
        })?;

    let mut remote_args = vec![
        "jarvisctl".to_string(),
        "codex".to_string(),
        "--driver".to_string(),
        "app-server".to_string(),
        "--task-note".to_string(),
        replica_set.spec.template.task_note.clone(),
        "--namespace".to_string(),
        runtime_namespace.to_string(),
        "--control-namespace".to_string(),
        replica_set.namespace_key().to_string(),
        "--deployment".to_string(),
        replica_set.spec.deployment_name.clone(),
        "--agents".to_string(),
        replica_set.spec.agents.to_string(),
        "--agent".to_string(),
        "agent0".to_string(),
        "--fresh".to_string(),
        "--startup-delay-ms".to_string(),
        startup_delay_ms.to_string(),
    ];
    if let Some(working_directory) = working_directory {
        remote_args.push("--working-directory".to_string());
        remote_args.push(working_directory.display().to_string());
    }
    if let Some(operator_message) = operator_message {
        remote_args.push("--message".to_string());
        remote_args.push(operator_message);
    }
    for (key, value) in runtime_labels {
        remote_args.push("--runtime-label".to_string());
        remote_args.push(format!("{key}={value}"));
    }
    for image in images {
        remote_args.push("--image".to_string());
        remote_args.push(image.display().to_string());
    }
    if !replica_set.spec.template.command.is_empty() {
        remote_args.push("--".to_string());
        remote_args.extend(replica_set.spec.template.command.clone());
    }

    let mut command_parts = vec!["env".to_string()];
    for (key, value) in environment {
        command_parts.push(format!("{key}={value}"));
    }
    command_parts.extend(remote_args);
    let remote_command = shell_words::join(command_parts);
    let output = ProcessCommand::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            &target,
            &remote_command,
        ])
        .output()
        .with_context(|| {
            format!(
                "failed to launch remote runtime '{}' on Node '{}'",
                runtime_namespace, node.metadata.name
            )
        })?;
    if !output.status.success() {
        let _ = cleanup_codex_auth_lease_on_remote_node(node, runtime_namespace);
        bail!(
            "remote launch '{}' on Node '{}' failed with status {}: {}",
            runtime_namespace,
            node.metadata.name,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
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

fn select_node_for_template(
    template: &DeploymentTemplateSpec,
) -> anyhow::Result<Option<ResourceEnvelope<NodeSpec>>> {
    let mut nodes = load_manifests_by_kind(ResourceKind::Node, None)?
        .into_iter()
        .filter_map(|manifest| match manifest {
            ResourceManifest::Node(node) => Some(node),
            _ => None,
        })
        .collect::<Vec<_>>();
    nodes.sort_by(|left, right| left.metadata.name.cmp(&right.metadata.name));
    if nodes.is_empty() {
        return Ok(None);
    }

    if template.node_selector.is_empty() {
        return Ok(nodes.into_iter().find(node_is_local));
    }

    nodes
        .into_iter()
        .find(|node| node_matches_template(node, template))
        .map(Some)
        .ok_or_else(|| {
            anyhow!(
                "no schedulable Node matched nodeSelector {}",
                format_selector(&template.node_selector)
            )
        })
}

fn node_matches_template(
    node: &ResourceEnvelope<NodeSpec>,
    template: &DeploymentTemplateSpec,
) -> bool {
    if node.spec.cordoned {
        return false;
    }
    if !node
        .spec
        .taints
        .iter()
        .all(|taint| template.tolerations.contains(taint))
    {
        return false;
    }
    let labels = node_effective_labels(node);
    template
        .node_selector
        .iter()
        .all(|(key, value)| labels.get(key) == Some(value))
}

fn node_effective_labels(node: &ResourceEnvelope<NodeSpec>) -> BTreeMap<String, String> {
    let mut labels = node.metadata.labels.clone();
    labels.insert(
        "kubernetes.io/hostname".to_string(),
        node.metadata.name.clone(),
    );
    labels.insert("jarvisctl.io/node".to_string(), node.metadata.name.clone());
    for role in &node.spec.roles {
        labels.insert(format!("node-role.jarvisctl.io/{role}"), "true".to_string());
    }
    for (key, value) in &node.spec.capabilities {
        labels.entry(key.clone()).or_insert_with(|| value.clone());
        labels
            .entry(format!("capability.jarvisctl.io/{key}"))
            .or_insert_with(|| value.clone());
    }
    labels
}

fn format_selector(selector: &BTreeMap<String, String>) -> String {
    selector
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn replica_set_runtime_labels(
    replica_set: &ResourceEnvelope<ReplicaSetSpec>,
    ordinal: usize,
    scheduled_node: Option<&ResourceEnvelope<NodeSpec>>,
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
    if let Some(node) = scheduled_node {
        labels.insert("jarvisctl.io/node".to_string(), node.metadata.name.clone());
    }
    labels
}

fn deployment_runtime_environment(
    control_namespace: &str,
    deployment_name: &str,
    replica_set_name: &str,
    revision: u64,
    runtime_namespace: &str,
    ordinal: usize,
    scheduled_node: Option<&ResourceEnvelope<NodeSpec>>,
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
    if let Some(node) = scheduled_node {
        environment.insert("JARVIS_NODE_NAME".to_string(), node.metadata.name.clone());
        if let Some(address) = node.spec.address.as_deref() {
            environment.insert("JARVIS_NODE_ADDRESS".to_string(), address.to_string());
        }
    }
    environment
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

fn load_namespace_defaults(control_namespace: &str) -> anyhow::Result<NamespaceSpec> {
    match load_manifest(ResourceKind::Namespace, control_namespace, None) {
        Ok(ResourceManifest::Namespace(namespace)) => Ok(namespace.spec),
        Ok(_) => bail!("resource '{}' is not a Namespace", control_namespace),
        Err(_) => Ok(NamespaceSpec::default()),
    }
}

fn resource_access_policy_status(policy: &ResourceAccessPolicy) -> ResourceAccessPolicyStatus {
    ResourceAccessPolicyStatus {
        allowed_namespaces: policy.allowed_namespaces.clone(),
        workload_selector: policy
            .workload_selector
            .as_ref()
            .map(|selector| selector.match_labels.clone())
            .unwrap_or_default(),
    }
}

fn access_policy_summary(policy: &ResourceAccessPolicy) -> String {
    let mut parts = Vec::new();
    if !policy.allowed_namespaces.is_empty() {
        parts.push(format!(
            "namespaces {}",
            policy.allowed_namespaces.join(",")
        ));
    }
    if let Some(selector) = policy.workload_selector.as_ref() {
        if !selector.match_labels.is_empty() {
            let labels = selector
                .match_labels
                .iter()
                .map(|(key, value)| format!("{}={}", key, value))
                .collect::<Vec<_>>()
                .join(",");
            parts.push(format!("selector {}", labels));
        }
    }
    if parts.is_empty() {
        "all workloads".to_string()
    } else {
        parts.join(" + ")
    }
}

fn access_policy_allows_workload(
    policy: &ResourceAccessPolicy,
    control_namespace: &str,
    workload_labels: &BTreeMap<String, String>,
) -> bool {
    let namespace_allowed = policy.allowed_namespaces.is_empty()
        || policy
            .allowed_namespaces
            .iter()
            .any(|namespace| namespace == control_namespace);
    let selector_allowed = policy
        .workload_selector
        .as_ref()
        .map(|selector| label_selector_matches(selector, workload_labels))
        .unwrap_or(true);
    namespace_allowed && selector_allowed
}

fn load_config_map_values(
    control_namespace: &str,
    references: &[EnvBindingRef],
    workload_labels: &BTreeMap<String, String>,
) -> anyhow::Result<BTreeMap<String, BTreeMap<String, String>>> {
    let mut values = BTreeMap::new();
    for reference in references {
        let name = env_binding_name(reference);
        let manifest = match load_manifest(ResourceKind::ConfigMap, name, Some(control_namespace)) {
            Ok(manifest) => manifest,
            Err(error) if env_binding_optional(reference) => {
                let _ = error;
                continue;
            }
            Err(error) => return Err(error),
        };
        let ResourceManifest::ConfigMap(config_map) = manifest else {
            bail!(
                "resource '{}/{}' is not a ConfigMap",
                control_namespace,
                name
            );
        };
        ensure!(
            access_policy_allows_workload(
                &config_map.spec.access_policy,
                control_namespace,
                workload_labels
            ),
            "configmap '{}/{}' is not allowed for this workload",
            control_namespace,
            name
        );
        let data = if let Some(prefix) = env_binding_prefix(reference) {
            config_map
                .spec
                .data
                .into_iter()
                .map(|(key, value)| (format!("{}{}", prefix, key), value))
                .collect::<BTreeMap<_, _>>()
        } else {
            config_map.spec.data
        };
        let key = env_binding_prefix(reference)
            .map(|prefix| format!("{}:{}", name, prefix))
            .unwrap_or_else(|| name.to_string());
        values.insert(key, data);
    }
    Ok(values)
}

fn load_secret_values(
    control_namespace: &str,
    references: &[EnvBindingRef],
    workload_labels: &BTreeMap<String, String>,
) -> anyhow::Result<BTreeMap<String, BTreeMap<String, String>>> {
    let mut values = BTreeMap::new();
    for reference in references {
        let name = env_binding_name(reference);
        let manifest = match load_manifest(ResourceKind::Secret, name, Some(control_namespace)) {
            Ok(manifest) => manifest,
            Err(error) if env_binding_optional(reference) => {
                let _ = error;
                continue;
            }
            Err(error) => return Err(error),
        };
        let ResourceManifest::Secret(secret) = manifest else {
            bail!("resource '{}/{}' is not a Secret", control_namespace, name);
        };
        ensure!(
            access_policy_allows_workload(
                &secret.spec.access_policy,
                control_namespace,
                workload_labels
            ),
            "secret '{}/{}' is not allowed for this workload",
            control_namespace,
            name
        );
        let data = if let Some(prefix) = env_binding_prefix(reference) {
            secret
                .spec
                .string_data
                .into_iter()
                .map(|(key, value)| (format!("{}{}", prefix, key), value))
                .collect::<BTreeMap<_, _>>()
        } else {
            secret.spec.string_data
        };
        let key = env_binding_prefix(reference)
            .map(|prefix| format!("{}:{}", name, prefix))
            .unwrap_or_else(|| name.to_string());
        values.insert(key, data);
    }
    Ok(values)
}

fn load_volume_paths(
    control_namespace: &str,
    references: &[VolumeBindingRef],
    workload_labels: &BTreeMap<String, String>,
) -> anyhow::Result<Vec<String>> {
    let mut paths = Vec::new();
    for reference in references {
        let name = volume_binding_name(reference);
        let manifest = match load_manifest(ResourceKind::Volume, name, Some(control_namespace)) {
            Ok(manifest) => manifest,
            Err(error) if volume_binding_optional(reference) => {
                let _ = error;
                continue;
            }
            Err(error) => return Err(error),
        };
        let ResourceManifest::Volume(volume) = manifest else {
            bail!("resource '{}/{}' is not a Volume", control_namespace, name);
        };
        ensure!(
            access_policy_allows_workload(
                &volume.spec.access_policy,
                control_namespace,
                workload_labels
            ),
            "volume '{}/{}' is not allowed for this workload",
            control_namespace,
            name
        );
        let selected_paths = if volume_binding_paths(reference).is_empty() {
            volume.spec.paths
        } else {
            volume
                .spec
                .paths
                .into_iter()
                .filter(|path| volume_binding_paths(reference).contains(path))
                .collect::<Vec<_>>()
        };
        for path in selected_paths {
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

fn service_matches_session(
    manifest: &ResourceEnvelope<ServiceSpec>,
    session: &NativeSessionMetadata,
) -> bool {
    if effective_service_target_kind(&manifest.spec) != ServiceTargetKind::Runtime {
        return false;
    }
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

fn effective_service_target_kind(spec: &ServiceSpec) -> ServiceTargetKind {
    spec.target_kind.clone().unwrap_or_default()
}

fn service_allows_intent(spec: &ServiceSpec, intent: Option<&str>) -> bool {
    if spec.allowed_intents.is_empty() {
        return true;
    }
    intent
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| spec.allowed_intents.iter().any(|allowed| allowed == value))
        .unwrap_or(false)
}

fn service_allows_source_workload(
    spec: &ServiceSpec,
    control_namespace: &str,
    workload_labels: &BTreeMap<String, String>,
) -> bool {
    access_policy_allows_workload(&spec.access_policy, control_namespace, workload_labels)
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

fn normalize_namespaced_resource_namespace(namespace: Option<&str>) -> String {
    namespace
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("default")
        .to_string()
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
    sessions.extend(collect_remote_runtime_sessions()?);
    Ok(sessions)
}

fn collect_remote_runtime_sessions() -> anyhow::Result<Vec<NativeSessionMetadata>> {
    let mut sessions = Vec::new();
    let nodes = load_manifests_by_kind(ResourceKind::Node, None)?
        .into_iter()
        .filter_map(|manifest| match manifest {
            ResourceManifest::Node(node) if !node_is_local(&node) => Some(node),
            _ => None,
        })
        .collect::<Vec<_>>();

    for node in nodes {
        let Some(target) = node_ssh_target(&node.spec) else {
            continue;
        };
        let output = ProcessCommand::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                &target,
                "jarvisctl list --json",
            ])
            .output();
        let Ok(output) = output else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
            continue;
        };
        let mut remote_sessions = if value.is_array() {
            serde_json::from_value::<Vec<NativeSessionMetadata>>(value).unwrap_or_default()
        } else {
            serde_json::from_value::<NativeSessionMetadata>(value)
                .map(|session| vec![session])
                .unwrap_or_default()
        };
        for session in &mut remote_sessions {
            let context = session
                .context
                .get_or_insert_with(RuntimeContextMetadata::default);
            context
                .labels
                .entry("jarvisctl.io/node".to_string())
                .or_insert_with(|| node.metadata.name.clone());
            context
                .labels
                .entry("jarvisctl.io/node-address".to_string())
                .or_insert_with(|| node.spec.address.clone().unwrap_or_default());
        }
        sessions.extend(remote_sessions);
    }
    Ok(sessions)
}

fn load_runtime_session_by_namespace(namespace: &str) -> anyhow::Result<NativeSessionMetadata> {
    collect_runtime_sessions()?
        .into_iter()
        .find(|session| session.namespace == namespace)
        .ok_or_else(|| anyhow!("runtime session '{}' does not exist", namespace))
}

fn delete_runtime_session(session: &NativeSessionMetadata) -> anyhow::Result<()> {
    if let Some(node_name) = session
        .context
        .as_ref()
        .and_then(|context| context.labels.get("jarvisctl.io/node"))
    {
        if let Ok(ResourceManifest::Node(node)) = load_manifest(ResourceKind::Node, node_name, None)
        {
            if !node_is_local(&node) {
                return delete_remote_runtime_session(&node, &session.namespace);
            }
        }
    }
    match session.backend.as_str() {
        "codex-app" => delete_codex_app_session(&session.namespace),
        _ => delete_native_session(&session.namespace),
    }
}

fn delete_remote_runtime_session(
    node: &ResourceEnvelope<NodeSpec>,
    runtime_namespace: &str,
) -> anyhow::Result<()> {
    run_remote_runtime_command(
        node,
        vec![
            "jarvisctl".to_string(),
            "delete".to_string(),
            "--namespace".to_string(),
            runtime_namespace.to_string(),
        ],
    )?;
    cleanup_codex_auth_lease_on_remote_node(node, runtime_namespace).with_context(|| {
        format!(
            "remote runtime '{}' was deleted, but Codex auth lease cleanup failed on Node '{}'",
            runtime_namespace, node.metadata.name
        )
    })?;
    Ok(())
}

fn remote_node_for_runtime_session(
    runtime_namespace: &str,
) -> anyhow::Result<Option<ResourceEnvelope<NodeSpec>>> {
    let Some(session) = collect_remote_runtime_sessions()?
        .into_iter()
        .find(|session| session.namespace == runtime_namespace)
    else {
        return Ok(None);
    };
    let Some(node_name) = session
        .context
        .as_ref()
        .and_then(|context| context.labels.get("jarvisctl.io/node"))
    else {
        return Ok(None);
    };
    match load_manifest(ResourceKind::Node, node_name, None) {
        Ok(ResourceManifest::Node(node)) if !node_is_local(&node) => Ok(Some(node)),
        _ => Ok(None),
    }
}

fn run_remote_runtime_command(
    node: &ResourceEnvelope<NodeSpec>,
    command: Vec<String>,
) -> anyhow::Result<()> {
    let target = node_ssh_target(&node.spec)
        .ok_or_else(|| anyhow!("Node '{}' has no SSH target", node.metadata.name))?;
    let remote_command = shell_words::join(command);
    let output = ProcessCommand::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            &target,
            &remote_command,
        ])
        .output()
        .with_context(|| {
            format!(
                "failed to run remote command on Node '{}'",
                node.metadata.name
            )
        })?;
    if !output.status.success() {
        bail!(
            "remote command on Node '{}' failed with status {}: {}",
            node.metadata.name,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn run_remote_runtime_command_interactive(
    node: &ResourceEnvelope<NodeSpec>,
    command: Vec<String>,
) -> anyhow::Result<()> {
    let target = node_ssh_target(&node.spec)
        .ok_or_else(|| anyhow!("Node '{}' has no SSH target", node.metadata.name))?;
    let remote_command = shell_words::join(command);
    let status = ProcessCommand::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            &target,
            &remote_command,
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| {
            format!(
                "failed to run remote command on Node '{}'",
                node.metadata.name
            )
        })?;
    if !status.success() {
        bail!(
            "remote command on Node '{}' failed with status {}",
            node.metadata.name,
            status
        );
    }
    Ok(())
}

fn parse_specific_kind(kind_arg: ControlPlaneResourceKindArg) -> anyhow::Result<ResourceKind> {
    match kind_arg {
        ControlPlaneResourceKindArg::Node => Ok(ResourceKind::Node),
        ControlPlaneResourceKindArg::Namespace => Ok(ResourceKind::Namespace),
        ControlPlaneResourceKindArg::Deployment => Ok(ResourceKind::Deployment),
        ControlPlaneResourceKindArg::ReplicaSet => Ok(ResourceKind::ReplicaSet),
        ControlPlaneResourceKindArg::Service => Ok(ResourceKind::Service),
        ControlPlaneResourceKindArg::NetworkPolicy => Ok(ResourceKind::NetworkPolicy),
        ControlPlaneResourceKindArg::ConfigMap => Ok(ResourceKind::ConfigMap),
        ControlPlaneResourceKindArg::Secret => Ok(ResourceKind::Secret),
        ControlPlaneResourceKindArg::Volume => Ok(ResourceKind::Volume),
        ControlPlaneResourceKindArg::All => bail!("'all' is not valid for this command"),
    }
}

impl ResourceKind {
    fn directory_name(self) -> &'static str {
        match self {
            ResourceKind::Node => "nodes",
            ResourceKind::Namespace => "namespaces",
            ResourceKind::Deployment => "deployments",
            ResourceKind::ReplicaSet => "replicasets",
            ResourceKind::Service => "services",
            ResourceKind::NetworkPolicy => "networkpolicies",
            ResourceKind::ConfigMap => "configmaps",
            ResourceKind::Secret => "secrets",
            ResourceKind::Volume => "volumes",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            ResourceKind::Node => "Node",
            ResourceKind::Namespace => "Namespace",
            ResourceKind::Deployment => "Deployment",
            ResourceKind::ReplicaSet => "ReplicaSet",
            ResourceKind::Service => "Service",
            ResourceKind::NetworkPolicy => "NetworkPolicy",
            ResourceKind::ConfigMap => "ConfigMap",
            ResourceKind::Secret => "Secret",
            ResourceKind::Volume => "Volume",
        }
    }
}

impl ResourceManifest {
    fn kind(&self) -> ResourceKind {
        match self {
            ResourceManifest::Node(_) => ResourceKind::Node,
            ResourceManifest::Namespace(_) => ResourceKind::Namespace,
            ResourceManifest::Deployment(_) => ResourceKind::Deployment,
            ResourceManifest::ReplicaSet(_) => ResourceKind::ReplicaSet,
            ResourceManifest::Service(_) => ResourceKind::Service,
            ResourceManifest::NetworkPolicy(_) => ResourceKind::NetworkPolicy,
            ResourceManifest::ConfigMap(_) => ResourceKind::ConfigMap,
            ResourceManifest::Secret(_) => ResourceKind::Secret,
            ResourceManifest::Volume(_) => ResourceKind::Volume,
        }
    }

    fn name(&self) -> &str {
        match self {
            ResourceManifest::Node(manifest) => &manifest.metadata.name,
            ResourceManifest::Namespace(manifest) => &manifest.metadata.name,
            ResourceManifest::Deployment(manifest) => &manifest.metadata.name,
            ResourceManifest::ReplicaSet(manifest) => &manifest.metadata.name,
            ResourceManifest::Service(manifest) => &manifest.metadata.name,
            ResourceManifest::NetworkPolicy(manifest) => &manifest.metadata.name,
            ResourceManifest::ConfigMap(manifest) => &manifest.metadata.name,
            ResourceManifest::Secret(manifest) => &manifest.metadata.name,
            ResourceManifest::Volume(manifest) => &manifest.metadata.name,
        }
    }

    fn namespace(&self) -> Option<&str> {
        match self {
            ResourceManifest::Node(_) => None,
            ResourceManifest::Namespace(_) => None,
            ResourceManifest::Deployment(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::ReplicaSet(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::Service(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::NetworkPolicy(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::ConfigMap(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::Secret(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::Volume(manifest) => manifest.metadata.namespace.as_deref(),
        }
    }

    fn metadata_mut(&mut self) -> &mut ResourceMetadata {
        match self {
            ResourceManifest::Node(manifest) => &mut manifest.metadata,
            ResourceManifest::Namespace(manifest) => &mut manifest.metadata,
            ResourceManifest::Deployment(manifest) => &mut manifest.metadata,
            ResourceManifest::ReplicaSet(manifest) => &mut manifest.metadata,
            ResourceManifest::Service(manifest) => &mut manifest.metadata,
            ResourceManifest::NetworkPolicy(manifest) => &mut manifest.metadata,
            ResourceManifest::ConfigMap(manifest) => &mut manifest.metadata,
            ResourceManifest::Secret(manifest) => &mut manifest.metadata,
            ResourceManifest::Volume(manifest) => &mut manifest.metadata,
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

fn short_revision(revision: &str) -> String {
    revision.chars().take(12).collect()
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
    fn config_map_bindings_apply_prefix_and_access_policy() {
        let _lock = home_env_lock().lock().unwrap();
        let _home = TempHomeGuard::new("jarvisctl-configmap-access-policy");

        let config_map = ResourceEnvelope {
            api_version: API_VERSION.to_string(),
            kind: "ConfigMap".to_string(),
            metadata: ResourceMetadata {
                name: "runtime-env".to_string(),
                namespace: Some("team-alpha".to_string()),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
            },
            spec: ConfigMapSpec {
                data: BTreeMap::from([("MODE".to_string(), "strict".to_string())]),
                access_policy: ResourceAccessPolicy {
                    allowed_namespaces: vec!["team-alpha".to_string()],
                    workload_selector: Some(LabelSelector {
                        match_labels: BTreeMap::from([("lane".to_string(), "junior".to_string())]),
                    }),
                },
            },
        };
        save_manifest(&ResourceManifest::ConfigMap(config_map)).unwrap();

        let values = load_config_map_values(
            "team-alpha",
            &[EnvBindingRef::Ref(NamedEnvBindingRef {
                name: "runtime-env".to_string(),
                optional: false,
                prefix: Some("APP_".to_string()),
            })],
            &BTreeMap::from([("lane".to_string(), "junior".to_string())]),
        )
        .unwrap();
        assert_eq!(
            values
                .get("runtime-env:APP_")
                .and_then(|entry| entry.get("APP_MODE"))
                .map(String::as_str),
            Some("strict")
        );

        let error = load_config_map_values(
            "team-alpha",
            &[EnvBindingRef::Name("runtime-env".to_string())],
            &BTreeMap::from([("lane".to_string(), "senior".to_string())]),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("configmap 'team-alpha/runtime-env' is not allowed for this workload"),
            "{}",
            error
        );
    }

    #[test]
    fn deployment_status_exposes_template_binding_metadata() {
        let _lock = home_env_lock().lock().unwrap();
        let _home = TempHomeGuard::new("jarvisctl-deployment-binding-status");

        let deployment = ResourceEnvelope {
            api_version: API_VERSION.to_string(),
            kind: "Deployment".to_string(),
            metadata: ResourceMetadata {
                name: "planner".to_string(),
                namespace: Some("team-alpha".to_string()),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
            },
            spec: DeploymentSpec {
                replicas: 0,
                template: DeploymentTemplateSpec {
                    task_note: "/tmp/demo.md".to_string(),
                    config_maps: vec![EnvBindingRef::Ref(NamedEnvBindingRef {
                        name: "planner-config".to_string(),
                        optional: false,
                        prefix: Some("CFG_".to_string()),
                    })],
                    secrets: vec![EnvBindingRef::Ref(NamedEnvBindingRef {
                        name: "planner-secret".to_string(),
                        optional: true,
                        prefix: None,
                    })],
                    volumes: vec![VolumeBindingRef::Ref(NamedVolumeBindingRef {
                        name: "shared-data".to_string(),
                        optional: false,
                        paths: vec!["/workspace/out".to_string()],
                    })],
                    ..DeploymentTemplateSpec::default()
                },
                ..DeploymentSpec::default()
            },
        };

        validate_deployment(&deployment).unwrap();
        let status = deployment_status(&deployment).unwrap();
        assert_eq!(status.config_maps.len(), 1);
        assert_eq!(status.config_maps[0].name, "planner-config");
        assert_eq!(status.config_maps[0].prefix.as_deref(), Some("CFG_"));
        assert!(!status.config_maps[0].optional);
        assert_eq!(status.secrets.len(), 1);
        assert_eq!(status.secrets[0].name, "planner-secret");
        assert!(status.secrets[0].optional);
        assert_eq!(status.volumes.len(), 1);
        assert_eq!(status.volumes[0].name, "shared-data");
        assert_eq!(status.volumes[0].paths, vec!["/workspace/out".to_string()]);
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
    fn kubernetes_compiler_renders_codex_runtime_deployment_and_service() {
        let _home_guard = home_env_lock().lock().unwrap();
        let temp_home = TempHomeGuard::new("jarvisctl-kube-runtime-render");
        let repo_dir = temp_home.root.join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        let ticket_path = temp_home.root.join("runtime-ticket.md");
        write_text_file(
            &ticket_path,
            &format!(
                r#"---
title: Kube Runtime Ticket
type: ticket
owner: codex
repo_path: {}
---

# Kube Runtime Ticket

## Request
- Launch Codex inside Kubernetes from the same ticket contract.
"#,
                repo_dir.display()
            ),
        );

        let manifests = parse_manifest_documents(&format!(
            r#"apiVersion: jarvisctl.io/v1alpha1
kind: Namespace
metadata:
  name: runtime-lab
spec: {{}}
---
apiVersion: jarvisctl.io/v1alpha1
kind: Deployment
metadata:
  name: codex-runtime
  namespace: runtime-lab
spec:
  replicas: 1
  agents: 1
  driver: app_server
  template:
    task_note: {ticket_path}
    working_directory: {repo_dir}
    labels:
      app: codex-runtime
      lane: codex
    kubernetes:
      image: ghcr.io/example/jarvisctl:dev
      imagePullPolicy: IfNotPresent
      controlPort: 47999
      workspaceHostPath: {repo_dir}
      workspaceMountPath: {repo_dir}
      env:
        HOME: /home/rootster
        EXTRA_FLAG: enabled
---
apiVersion: jarvisctl.io/v1alpha1
kind: Service
metadata:
  name: runtime-svc
  namespace: runtime-lab
spec:
  selector:
    app: codex-runtime
    lane: codex
"#,
            ticket_path = ticket_path.display(),
            repo_dir = repo_dir.display(),
        ))
        .unwrap();

        let compiled = compile_kubernetes_manifests(&manifests).unwrap();

        let launch_config = compiled
            .manifests
            .iter()
            .find(|manifest| {
                manifest.get("kind").and_then(serde_json::Value::as_str) == Some("ConfigMap")
                    && manifest
                        .get("metadata")
                        .and_then(|metadata| metadata.get("name"))
                        .and_then(serde_json::Value::as_str)
                        == Some("codex-runtime-codex-launch")
            })
            .expect("compiled launch config map");
        let launch_manifest_raw = launch_config["data"]["launch-manifest.json"]
            .as_str()
            .expect("launch manifest payload");
        let launch_manifest: serde_json::Value =
            serde_json::from_str(launch_manifest_raw).expect("launch manifest json");
        assert_eq!(launch_manifest["namespace"].as_str(), Some("codex-runtime"));
        assert_eq!(
            launch_manifest["working_directory"].as_str(),
            Some(repo_dir.to_string_lossy().as_ref())
        );
        assert_eq!(
            launch_manifest["context"]["control_namespace"].as_str(),
            Some("runtime-lab")
        );

        let deployment = compiled
            .manifests
            .iter()
            .find(|manifest| {
                manifest.get("kind").and_then(serde_json::Value::as_str) == Some("Deployment")
                    && manifest
                        .get("metadata")
                        .and_then(|metadata| metadata.get("name"))
                        .and_then(serde_json::Value::as_str)
                        == Some("codex-runtime")
            })
            .expect("compiled runtime deployment");
        let container = &deployment["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(
            container["image"].as_str(),
            Some("ghcr.io/example/jarvisctl:dev")
        );
        assert_eq!(
            container["workingDir"].as_str(),
            Some(repo_dir.to_string_lossy().as_ref())
        );
        assert_eq!(container["ports"][0]["containerPort"].as_u64(), Some(47999));
        assert_eq!(
            container["command"][2].as_str(),
            Some(
                "exec jarvisctl codex-app-session-serve --manifest /etc/jarvisctl/launch-manifest.json"
            )
        );
        assert_eq!(
            container["startupProbe"]["tcpSocket"]["port"].as_u64(),
            Some(47999)
        );
        assert_eq!(
            container["readinessProbe"]["tcpSocket"]["port"].as_u64(),
            Some(47999)
        );
        let env = container["env"].as_array().expect("runtime env");
        assert!(
            env.iter().any(|entry| {
                entry.get("name").and_then(serde_json::Value::as_str)
                    == Some("JARVISCTL_CODEX_APP_TCP_PORT")
                    && entry.get("value").and_then(serde_json::Value::as_str) == Some("47999")
            }),
            "expected TCP control port env"
        );
        assert!(
            env.iter().any(|entry| {
                entry.get("name").and_then(serde_json::Value::as_str) == Some("EXTRA_FLAG")
                    && entry.get("value").and_then(serde_json::Value::as_str) == Some("enabled")
            }),
            "expected runtime env passthrough"
        );
        let volume_mounts = container["volumeMounts"].as_array().expect("volume mounts");
        assert!(
            volume_mounts.iter().any(|entry| {
                entry.get("mountPath").and_then(serde_json::Value::as_str)
                    == Some(repo_dir.to_string_lossy().as_ref())
            }),
            "expected workspace hostPath mount"
        );

        let service = compiled
            .manifests
            .iter()
            .find(|manifest| {
                manifest.get("kind").and_then(serde_json::Value::as_str) == Some("Service")
                    && manifest
                        .get("metadata")
                        .and_then(|metadata| metadata.get("name"))
                        .and_then(serde_json::Value::as_str)
                        == Some("runtime-svc")
            })
            .expect("compiled runtime service");
        assert_eq!(service["spec"]["ports"][0]["port"].as_u64(), Some(47999));
        assert_eq!(
            service["spec"]["selector"]["app"].as_str(),
            Some("codex-runtime")
        );
        assert_eq!(service["spec"]["selector"]["lane"].as_str(), Some("codex"));
        assert_eq!(
            service["metadata"]["labels"]["jarvisctl.io/runtime-deployment"].as_str(),
            Some("codex-runtime")
        );
    }
}
