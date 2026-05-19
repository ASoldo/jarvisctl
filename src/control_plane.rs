use crate::SessionBackend;
use crate::codex::{
    CodexLaunchOptions, CodexRuntimeDriver, codex_app_manifest_from_prepared,
    enrich_native_sessions, launch_codex_ticket, prepare_codex_ticket_launch,
};
use crate::codex_app::{
    CodexAppInputMode, collect_codex_app_sessions, delete_codex_app_session,
    tell_codex_app_with_mode,
};
use crate::native::{
    NativeSessionMetadata, RuntimeContextMetadata, collect_native_sessions, delete_native_session,
};
use crate::ticket::slugify;
use anyhow::{Context, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::Utc;
use clap::ValueEnum;
use ring::{aead, rand};
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
    render_rollout_status_output, render_worker_validation_output, wait_for_rollout_status_output,
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
    #[value(alias = "worker", alias = "workers")]
    Worker,
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
    Worker,
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
    pub from_node: Option<String>,
    pub role: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub retries: usize,
    pub prompt: String,
    pub working_directory: Option<String>,
    pub namespace: Option<String>,
    pub timeout_seconds: u64,
    pub sandbox_mode: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub ephemeral: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestrationPolicy {
    #[serde(default = "default_policy_role")]
    pub default_role: String,
    #[serde(default)]
    pub default_labels: BTreeMap<String, String>,
    #[serde(default = "default_policy_retries")]
    pub retries: usize,
    #[serde(default = "default_policy_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_policy_fanout_concurrency")]
    pub fanout_max_concurrency: usize,
    #[serde(default = "default_policy_cleanup_retention_days")]
    pub cleanup_retention_days: u64,
    #[serde(default = "default_policy_remote_index_timeout_seconds")]
    pub remote_index_timeout_seconds: u64,
}

impl Default for OrchestrationPolicy {
    fn default() -> Self {
        Self {
            default_role: default_policy_role(),
            default_labels: BTreeMap::new(),
            retries: default_policy_retries(),
            timeout_seconds: default_policy_timeout_seconds(),
            fanout_max_concurrency: default_policy_fanout_concurrency(),
            cleanup_retention_days: default_policy_cleanup_retention_days(),
            remote_index_timeout_seconds: default_policy_remote_index_timeout_seconds(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NodeStartSessionOptions {
    pub node: String,
    pub role: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub retries: usize,
    pub task_note: PathBuf,
    pub namespace: Option<String>,
    pub resume_session_id: Option<String>,
    pub working_directory: Option<PathBuf>,
    pub message: Option<String>,
    pub startup_delay_ms: u64,
    pub command: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NodePairSessionOptions {
    pub first_node: String,
    pub second_node: String,
    pub first_task_note: PathBuf,
    pub second_task_note: PathBuf,
    pub first_namespace: Option<String>,
    pub second_namespace: Option<String>,
    pub namespace_prefix: Option<String>,
    pub message: Option<String>,
    pub startup_delay_ms: u64,
    pub retries: usize,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct NodeScheduleOptions {
    pub role: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub exclude: Vec<String>,
    pub require_codex_auth: bool,
}

#[derive(Debug, Clone, Default)]
pub struct NodeFanoutOptions {
    pub nodes: Vec<String>,
    pub role: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub prompt: String,
    pub timeout_seconds: u64,
    pub sandbox_mode: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub ephemeral: bool,
    pub max_concurrency: usize,
}

#[derive(Debug, Clone, Default)]
pub struct NodeLinksOptions {
    pub from: Vec<String>,
    pub to: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeScheduleResult {
    pub node: String,
    pub target: String,
    pub score: i64,
    pub reasons: Vec<String>,
    pub facts: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeVisitResult {
    pub node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_node: Option<String>,
    pub namespace: String,
    pub exit_status: i32,
    pub final_message: String,
    pub stdout: String,
    pub stderr: String,
    pub cleanup_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeStartSessionResult {
    pub node: String,
    pub namespace: String,
    pub task_note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_task_note: Option<String>,
    pub exit_status: i32,
    pub stdout: String,
    pub stderr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    pub retryable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodePairMemberResult {
    pub role: String,
    pub node: String,
    pub namespace: String,
    pub task_note: String,
    pub exit_status: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    pub retryable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodePairSessionResult {
    pub coordination_id: String,
    pub coordination_note: String,
    pub members: Vec<NodePairMemberResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodePreflightResult {
    pub ok: bool,
    pub generated_at_epoch_ms: u128,
    pub issues: Vec<String>,
    pub doctors: Vec<NodeDoctorCheck>,
    pub links: Vec<NodeLinkCheck>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeInspectResult {
    pub node: String,
    pub target: String,
    pub available: bool,
    pub facts: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeCleanupResult {
    pub node: String,
    pub target: String,
    pub restored_leases: Vec<String>,
    pub skipped_active_leases: Vec<String>,
    pub removed_visit_artifacts: usize,
    pub stderr: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeReconcileResult {
    pub doctors: Vec<NodeDoctorCheck>,
    pub cleanups: Vec<NodeCleanupResult>,
    pub failures: Vec<NodeFanoutFailure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CapsuleKeyRotationResult {
    pub key_path: String,
    pub synced_nodes: Vec<String>,
    pub failures: Vec<NodeFanoutFailure>,
}

fn default_policy_role() -> String {
    "worker".to_string()
}

fn default_policy_retries() -> usize {
    1
}

fn default_policy_timeout_seconds() -> u64 {
    900
}

fn default_policy_fanout_concurrency() -> usize {
    4
}

fn default_policy_cleanup_retention_days() -> u64 {
    7
}

fn default_policy_remote_index_timeout_seconds() -> u64 {
    25
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisitIndexEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_source: Option<String>,
    pub namespace: String,
    pub node: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_node: Option<String>,
    pub status: String,
    pub started_at_epoch_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at_epoch_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterIndexResult {
    pub generated_at_epoch_ms: u128,
    pub sessions: Vec<NativeSessionMetadata>,
    pub visits: Vec<VisitIndexEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthAuditEvent {
    pub ts_epoch_ms: u128,
    pub event: String,
    pub node: String,
    pub namespace: String,
    pub status: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeDoctorCheck {
    pub node: String,
    pub available: bool,
    pub schedulable: bool,
    pub issues: Vec<String>,
    pub facts: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeFanoutFailure {
    pub node: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeFanoutResult {
    pub requested_nodes: Vec<String>,
    pub results: Vec<NodeVisitResult>,
    pub failures: Vec<NodeFanoutFailure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeLinkCheck {
    pub from: String,
    pub to: String,
    pub ok: bool,
    pub exit_status: i32,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NodeBootstrapOptions {
    pub name: String,
    pub address: Option<String>,
    pub ssh_host: String,
    pub ssh_user: Option<String>,
    pub roles: Vec<String>,
    pub labels: BTreeMap<String, String>,
    pub workspace_root: Option<String>,
    pub max_sessions: Option<usize>,
    pub codex_path: Option<String>,
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
    Worker,
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
    #[serde(default, rename = "className", skip_serializing_if = "Option::is_none")]
    pub class_name: Option<String>,
    #[serde(
        default,
        rename = "fallbackClassNames",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub fallback_class_names: Vec<String>,
    #[serde(
        default,
        rename = "requiredCapabilities",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub required_capabilities: Vec<String>,
    #[serde(
        default,
        rename = "preferredProviders",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub preferred_providers: Vec<String>,
    #[serde(
        default,
        rename = "accessPolicy",
        skip_serializing_if = "ResourceAccessPolicy::is_empty"
    )]
    pub access_policy: ResourceAccessPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkerSecretRef {
    pub name: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkerSpec {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub role: String,
    #[serde(
        default,
        rename = "outputMode",
        skip_serializing_if = "Option::is_none"
    )]
    pub output_mode: Option<String>,
    #[serde(
        default,
        rename = "systemPrompt",
        skip_serializing_if = "Option::is_none"
    )]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, rename = "numCtx", skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u64>,
    #[serde(
        default,
        rename = "numPredict",
        skip_serializing_if = "Option::is_none"
    )]
    pub num_predict: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub classes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    #[serde(
        default,
        rename = "maxConcurrent",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_concurrent: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locality: Option<String>,
    #[serde(
        default,
        rename = "apiKeySecretRef",
        skip_serializing_if = "Option::is_none"
    )]
    pub api_key_secret_ref: Option<WorkerSecretRef>,
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
    Worker(ResourceEnvelope<WorkerSpec>),
    NetworkPolicy(ResourceEnvelope<NetworkPolicySpec>),
    ConfigMap(ResourceEnvelope<ConfigMapSpec>),
    Secret(ResourceEnvelope<SecretSpec>),
    Volume(ResourceEnvelope<VolumeSpec>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
struct WorkerDescribeEnvelope {
    manifest: serde_json::Value,
    status: WorkerStatus,
}

#[derive(Debug, Clone, Serialize)]
struct WorkerStatus {
    endpoint: String,
    loaded: bool,
    locality: String,
    model: String,
    output_mode: String,
    provider: String,
    role: String,
    capabilities: Vec<String>,
    classes: Vec<String>,
    pool: Option<String>,
    max_concurrent: usize,
    active_runs: usize,
    pending_runs: usize,
    available_slots: usize,
    admission: String,
    admission_code: String,
    admission_reason: String,
    service_name: String,
    service_namespace: String,
    endpoints: Vec<String>,
    allowed_intents: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerValidationReport {
    pub status: String,
    pub workers: usize,
    pub ready_workers: usize,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerOffloadReport {
    pub job_name: String,
    pub namespace: String,
    pub service_name: String,
    pub phase: String,
    pub selected_class: Option<String>,
    pub fallback_class: bool,
    pub worker: Option<String>,
    pub worker_namespace: Option<String>,
    pub worker_provider: Option<String>,
    pub worker_model: Option<String>,
    pub worker_locality: Option<String>,
    pub validation_state: Option<String>,
    pub validation_message: Option<String>,
    pub artifact_path: Option<String>,
    pub output_path: Option<String>,
    pub response: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorkerOffloadOptions {
    pub service_name: String,
    pub control_namespace: Option<String>,
    pub via_runtime_namespace: Option<String>,
    pub prompt: String,
    pub intent: Option<String>,
    pub output_path: Option<PathBuf>,
    pub job_name: Option<String>,
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
    if options.from_node.is_some() {
        return run_node_relay_visit(options);
    }
    let mut options = options;
    let mut attempted = Vec::new();
    let attempts = options.retries.saturating_add(1);
    let mut last_error: Option<anyhow::Error> = None;
    for _ in 0..attempts {
        if options.node == "auto" || !attempted.is_empty() {
            let scheduled = schedule_node(NodeScheduleOptions {
                role: options.role.clone().or_else(|| Some("worker".to_string())),
                labels: options.labels.clone(),
                exclude: attempted.clone(),
                require_codex_auth: true,
            })?;
            options.node = scheduled.node;
        }
        let node_name = options.node.clone();
        attempted.push(node_name);
        match run_node_visit_direct(options.clone()) {
            Ok(result) => return Ok(result),
            Err(error) => {
                let retryable = failure_is_retryable(&classify_failure(&error.to_string()));
                last_error = Some(error);
                if !retryable {
                    break;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("visit did not run any attempts")))
}

fn run_node_visit_direct(options: NodeVisitOptions) -> anyhow::Result<NodeVisitResult> {
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
    sync_capsule_key_to_remote_node(&node)?;
    append_auth_audit_event(
        "auth_lease_create_start",
        &node.metadata.name,
        &namespace,
        "start",
        "",
    )?;
    sync_codex_auth_to_remote_node_for_namespace(&node, &auth_files, &namespace).with_context(
        || {
            format!(
                "failed to sync leased Codex auth before visit '{}' on Node '{}'",
                namespace, node.metadata.name
            )
        },
    )?;
    append_auth_audit_event(
        "auth_lease_create_complete",
        &node.metadata.name,
        &namespace,
        "ok",
        "",
    )?;

    let started_at_epoch_ms = now_epoch_ms();
    write_visit_index_entry(&VisitIndexEntry {
        index_source: None,
        namespace: namespace.clone(),
        node: options.node.clone(),
        from_node: options.from_node.clone(),
        status: "running".to_string(),
        started_at_epoch_ms,
        finished_at_epoch_ms: None,
        exit_status: None,
        archive_path: None,
        failure_class: None,
        retryable: None,
    })?;
    let visit_result = run_remote_codex_exec_visit(&target, &options, &namespace);
    let finished_at_epoch_ms = now_epoch_ms();
    append_auth_audit_event(
        "auth_lease_restore_start",
        &node.metadata.name,
        &namespace,
        "start",
        "",
    )?;
    let cleanup_result = cleanup_codex_auth_lease_on_remote_node(&node, &namespace);
    append_auth_audit_event(
        "auth_lease_restore_complete",
        &node.metadata.name,
        &namespace,
        if cleanup_result.is_ok() {
            "ok"
        } else {
            "failed"
        },
        cleanup_result
            .as_ref()
            .err()
            .map(|error| error.to_string())
            .as_deref()
            .unwrap_or(""),
    )?;

    let mut result = visit_result?;
    result.cleanup_status = match cleanup_result {
        Ok(()) => "restored".to_string(),
        Err(error) => format!("failed: {error}"),
    };
    result.archive_path = Some(
        archive_visit_result(&options, &result, started_at_epoch_ms, finished_at_epoch_ms)?
            .display()
            .to_string(),
    );
    write_visit_index_entry(&VisitIndexEntry {
        index_source: None,
        namespace: namespace.clone(),
        node: result.node.clone(),
        from_node: result.from_node.clone(),
        status: if result.exit_status == 0 && result.cleanup_status == "restored" {
            "finished".to_string()
        } else {
            "failed".to_string()
        },
        started_at_epoch_ms,
        finished_at_epoch_ms: Some(finished_at_epoch_ms),
        exit_status: Some(result.exit_status),
        archive_path: result.archive_path.clone(),
        failure_class: result.failure_class.clone(),
        retryable: result.retryable,
    })?;
    ensure!(
        result.cleanup_status == "restored",
        "visit '{}' completed, but Codex auth cleanup failed on Node '{}': {}",
        namespace,
        node.metadata.name,
        result.cleanup_status
    );
    ensure!(
        result.exit_status == 0,
        "remote Codex visit '{}' on Node '{}' failed with exit status {}: {}",
        namespace,
        node.metadata.name,
        result.exit_status,
        result.stderr.trim()
    );
    Ok(result)
}

pub fn inspect_node(name: &str) -> anyhow::Result<NodeInspectResult> {
    let manifest = load_manifest(ResourceKind::Node, name, None)?;
    let ResourceManifest::Node(node) = manifest else {
        bail!("resource '{}' is not a Node", name);
    };
    let target = node_ssh_target(&node.spec).unwrap_or_else(|| "local".to_string());
    let output = run_node_inspect_command(if node_is_local(&node) {
        None
    } else {
        Some(target.as_str())
    })?;
    let mut facts = parse_probe_output(&output);
    if let Some(workspace_root) = node.spec.workspace_root.as_deref() {
        facts.insert(
            "configured_workspace_root".to_string(),
            workspace_root.to_string(),
        );
    }
    Ok(NodeInspectResult {
        node: node.metadata.name,
        target,
        available: true,
        facts,
    })
}

pub fn cleanup_node(name: &str, max_age_days: u64) -> anyhow::Result<NodeCleanupResult> {
    let manifest = load_manifest(ResourceKind::Node, name, None)?;
    let ResourceManifest::Node(node) = manifest else {
        bail!("resource '{}' is not a Node", name);
    };
    let target = node_ssh_target(&node.spec).unwrap_or_else(|| "local".to_string());
    run_node_cleanup_command(
        if node_is_local(&node) {
            None
        } else {
            Some(target.as_str())
        },
        &node.metadata.name,
        &target,
        max_age_days,
    )
}

pub fn schedule_node(options: NodeScheduleOptions) -> anyhow::Result<NodeScheduleResult> {
    let nodes = load_manifests_by_kind(ResourceKind::Node, None)?
        .into_iter()
        .filter_map(|manifest| match manifest {
            ResourceManifest::Node(node) => Some(node),
            _ => None,
        })
        .collect::<Vec<_>>();
    ensure!(!nodes.is_empty(), "no registered Nodes exist");

    let mut candidates = Vec::new();
    for node in nodes {
        if node_is_local(&node) || node.spec.cordoned || !node.spec.taints.is_empty() {
            continue;
        }
        if options
            .exclude
            .iter()
            .any(|name| name == &node.metadata.name)
        {
            continue;
        }
        if let Some(role) = options.role.as_deref() {
            if !node.spec.roles.iter().any(|candidate| candidate == role) {
                continue;
            }
        }
        let labels = node_effective_labels(&node);
        if options
            .labels
            .iter()
            .any(|(key, value)| labels.get(key) != Some(value))
        {
            continue;
        }
        if node_is_local(&node) {
            continue;
        }
        let Ok(inspect) = inspect_node(&node.metadata.name) else {
            continue;
        };
        if options.require_codex_auth
            && inspect.facts.get("codex_auth").map(String::as_str) != Some("present")
        {
            continue;
        }
        if inspect.facts.get("codex_cli").is_none() || inspect.facts.get("jarvisctl").is_none() {
            continue;
        }

        let active_sessions = inspect
            .facts
            .get("active_sessions")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        let max_sessions = node.spec.max_sessions.unwrap_or(1).max(1) as i64;
        let mut score = 100 - active_sessions * 20 + max_sessions * 5;
        let mut reasons = vec![
            format!("active_sessions={active_sessions}"),
            format!("max_sessions={max_sessions}"),
        ];
        if inspect.facts.get("memory").map(String::as_str) == Some("present") {
            score += 5;
            reasons.push("memory=present".to_string());
        }
        if inspect.facts.get("vault").map(String::as_str) == Some("present") {
            score += 5;
            reasons.push("vault=present".to_string());
        }
        if let Some(arch) = inspect.facts.get("arch") {
            reasons.push(format!("arch={arch}"));
        }
        candidates.push(NodeScheduleResult {
            node: node.metadata.name,
            target: inspect.target,
            score,
            reasons,
            facts: inspect.facts,
        });
    }

    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.node.cmp(&right.node))
    });
    candidates
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no schedulable remote Node matched the requested constraints"))
}

pub fn doctor_nodes() -> anyhow::Result<Vec<NodeDoctorCheck>> {
    let nodes = load_manifests_by_kind(ResourceKind::Node, None)?
        .into_iter()
        .filter_map(|manifest| match manifest {
            ResourceManifest::Node(node) => Some(node),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut checks = Vec::new();
    for node in nodes {
        let mut issues = Vec::new();
        if node.spec.cordoned {
            issues.push("cordoned".to_string());
        }
        if !node.spec.taints.is_empty() {
            issues.push(format!("taints={}", node.spec.taints.join(",")));
        }
        let inspect = inspect_node(&node.metadata.name);
        let (available, facts) = match inspect {
            Ok(inspect) => (true, inspect.facts),
            Err(error) => {
                issues.push(format!("inspect_failed={error}"));
                (false, BTreeMap::new())
            }
        };
        if available {
            for (key, expected) in [
                ("codex_auth", "present"),
                ("memory", "present"),
                ("vault", "present"),
            ] {
                if facts.get(key).map(String::as_str) != Some(expected) {
                    issues.push(format!("{key}_not_{expected}"));
                }
            }
            if facts.get("codex_cli").is_none() {
                issues.push("codex_missing".to_string());
            }
            if facts.get("jarvisctl").is_none() {
                issues.push("jarvisctl_missing".to_string());
            }
        }
        checks.push(NodeDoctorCheck {
            node: node.metadata.name,
            available,
            schedulable: available && issues.is_empty(),
            issues,
            facts,
        });
    }
    checks.sort_by(|left, right| left.node.cmp(&right.node));
    Ok(checks)
}

pub fn preflight_nodes() -> anyhow::Result<NodePreflightResult> {
    let doctors = doctor_nodes()?;
    let links = check_node_links(NodeLinksOptions::default())?;
    let mut issues = Vec::new();

    for check in &doctors {
        if !check.available {
            issues.push(format!("node_unavailable={}", check.node));
        }
        if !check.schedulable {
            issues.push(format!(
                "node_unschedulable={}:{}",
                check.node,
                if check.issues.is_empty() {
                    "unknown".to_string()
                } else {
                    check.issues.join(",")
                }
            ));
        }
    }

    for link in &links {
        if !link.ok {
            issues.push(format!(
                "link_failed={}->{}:{}",
                link.from,
                link.to,
                link.failure_class
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .unwrap_or("unknown")
            ));
        }
    }

    append_version_consistency_issue(&doctors, "jarvisctl", &mut issues);
    append_version_consistency_issue(&doctors, "codex_cli", &mut issues);

    Ok(NodePreflightResult {
        ok: issues.is_empty(),
        generated_at_epoch_ms: now_epoch_ms(),
        issues,
        doctors,
        links,
    })
}

fn append_version_consistency_issue(
    doctors: &[NodeDoctorCheck],
    fact_key: &str,
    issues: &mut Vec<String>,
) {
    let mut versions = BTreeMap::<String, Vec<String>>::new();
    for check in doctors {
        if !check.available {
            continue;
        }
        let version = check
            .facts
            .get(fact_key)
            .cloned()
            .unwrap_or_else(|| "missing".to_string());
        versions
            .entry(version)
            .or_default()
            .push(check.node.clone());
    }
    if versions.len() <= 1 {
        return;
    }
    let detail = versions
        .into_iter()
        .map(|(version, nodes)| format!("{}=[{}]", version, nodes.join(",")))
        .collect::<Vec<_>>()
        .join(";");
    issues.push(format!("{fact_key}_version_mismatch={detail}"));
}

pub fn check_node_links(options: NodeLinksOptions) -> anyhow::Result<Vec<NodeLinkCheck>> {
    let nodes = load_manifests_by_kind(ResourceKind::Node, None)?
        .into_iter()
        .filter_map(|manifest| match manifest {
            ResourceManifest::Node(node) => Some(node),
            _ => None,
        })
        .collect::<Vec<_>>();
    ensure!(!nodes.is_empty(), "no registered Nodes exist");

    let from_filter = options.from.into_iter().collect::<BTreeSet<_>>();
    let to_filter = options.to.into_iter().collect::<BTreeSet<_>>();
    let mut checks = Vec::new();
    for source in &nodes {
        if !from_filter.is_empty() && !from_filter.contains(&source.metadata.name) {
            continue;
        }
        for target in &nodes {
            if source.metadata.name == target.metadata.name {
                continue;
            }
            if !to_filter.is_empty() && !to_filter.contains(&target.metadata.name) {
                continue;
            }
            checks.push(check_node_link(source, target));
        }
    }
    checks.sort_by(|left, right| {
        left.from
            .cmp(&right.from)
            .then_with(|| left.to.cmp(&right.to))
    });
    Ok(checks)
}

fn check_node_link(
    source: &ResourceEnvelope<NodeSpec>,
    target: &ResourceEnvelope<NodeSpec>,
) -> NodeLinkCheck {
    let result = if node_is_local(source) {
        check_direct_ssh_link(target)
    } else {
        check_relay_ssh_link(source, target)
    };
    match result {
        Ok((status, stdout, stderr)) => {
            let ok = status == 0;
            let detail = if ok {
                stdout.trim().to_string()
            } else {
                stderr.trim().to_string()
            };
            NodeLinkCheck {
                from: source.metadata.name.clone(),
                to: target.metadata.name.clone(),
                ok,
                exit_status: status,
                failure_class: (!ok).then(|| classify_failure(&format!("{stderr}\n{stdout}"))),
                auth_url: extract_auth_url(&format!("{stderr}\n{stdout}")),
                detail,
            }
        }
        Err(error) => {
            let detail = error.to_string();
            NodeLinkCheck {
                from: source.metadata.name.clone(),
                to: target.metadata.name.clone(),
                ok: false,
                exit_status: -1,
                failure_class: Some(classify_failure(&detail)),
                auth_url: extract_auth_url(&detail),
                detail,
            }
        }
    }
}

fn check_direct_ssh_link(
    target: &ResourceEnvelope<NodeSpec>,
) -> anyhow::Result<(i32, String, String)> {
    let target_label = node_link_target(target);
    run_link_probe_command(vec![
        "timeout".to_string(),
        "--kill-after=5s".to_string(),
        "25s".to_string(),
        "ssh".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        target_label,
        link_probe_script().to_string(),
    ])
}

fn check_relay_ssh_link(
    source: &ResourceEnvelope<NodeSpec>,
    target: &ResourceEnvelope<NodeSpec>,
) -> anyhow::Result<(i32, String, String)> {
    let source_target = node_ssh_target(&source.spec)
        .ok_or_else(|| anyhow!("Node '{}' has no SSH target", source.metadata.name))?;
    let target_label = node_link_target(target);
    let nested = shell_words::join([
        "timeout".to_string(),
        "--kill-after=5s".to_string(),
        "25s".to_string(),
        "ssh".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        target_label,
        link_probe_script().to_string(),
    ]);
    run_link_probe_command(vec![
        "timeout".to_string(),
        "--kill-after=5s".to_string(),
        "35s".to_string(),
        "ssh".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        source_target,
        nested,
    ])
}

fn run_link_probe_command(command: Vec<String>) -> anyhow::Result<(i32, String, String)> {
    let mut parts = command.into_iter();
    let program = parts
        .next()
        .ok_or_else(|| anyhow!("link probe command is empty"))?;
    let output = ProcessCommand::new(program)
        .args(parts.collect::<Vec<_>>())
        .output()
        .context("failed to run node link probe")?;
    Ok((
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    ))
}

fn node_link_target(node: &ResourceEnvelope<NodeSpec>) -> String {
    node_ssh_target(&node.spec).unwrap_or_else(|| node.metadata.name.clone())
}

fn link_probe_script() -> &'static str {
    "printf 'jarvisctl='; (jarvisctl --version 2>/dev/null || command -v jarvisctl 2>/dev/null || true) | head -n 1"
}

pub fn run_node_fanout(options: NodeFanoutOptions) -> anyhow::Result<NodeFanoutResult> {
    ensure!(
        !options.prompt.trim().is_empty(),
        "fanout prompt must not be empty"
    );
    let mut requested_nodes = if options.nodes.is_empty() {
        matching_remote_nodes(options.role.as_deref(), &options.labels)?
    } else {
        options.nodes.clone()
    };
    requested_nodes.sort();
    requested_nodes.dedup();
    ensure!(
        !requested_nodes.is_empty(),
        "no remote nodes matched fanout target constraints"
    );

    let mut results = Vec::new();
    let mut failures = Vec::new();
    let max_concurrency = options.max_concurrency.max(1);
    for batch in requested_nodes.chunks(max_concurrency) {
        let mut handles = Vec::new();
        for node in batch {
            let node = node.clone();
            let options = options.clone();
            handles.push(std::thread::spawn(move || {
                let visit = run_node_visit(NodeVisitOptions {
                    node: node.clone(),
                    from_node: None,
                    role: None,
                    labels: BTreeMap::new(),
                    retries: 0,
                    prompt: options.prompt,
                    working_directory: None,
                    namespace: Some(format!("fanout-{}-{}", slugify(&node), now_epoch_ms())),
                    timeout_seconds: options.timeout_seconds,
                    sandbox_mode: options.sandbox_mode,
                    model: options.model,
                    reasoning_effort: options.reasoning_effort,
                    ephemeral: options.ephemeral,
                });
                match visit {
                    Ok(result) => Ok(result),
                    Err(error) => Err(NodeFanoutFailure {
                        node,
                        error: error.to_string(),
                    }),
                }
            }));
        }
        for handle in handles {
            match handle.join() {
                Ok(Ok(result)) => results.push(result),
                Ok(Err(failure)) => failures.push(failure),
                Err(_) => failures.push(NodeFanoutFailure {
                    node: "unknown".to_string(),
                    error: "fanout worker thread panicked".to_string(),
                }),
            }
        }
    }
    Ok(NodeFanoutResult {
        requested_nodes,
        results,
        failures,
    })
}

fn matching_remote_nodes(
    role: Option<&str>,
    labels: &BTreeMap<String, String>,
) -> anyhow::Result<Vec<String>> {
    let mut nodes = Vec::new();
    for manifest in load_manifests_by_kind(ResourceKind::Node, None)? {
        let ResourceManifest::Node(node) = manifest else {
            continue;
        };
        if node_is_local(&node) || node.spec.cordoned || !node.spec.taints.is_empty() {
            continue;
        }
        if let Some(role) = role {
            if !node.spec.roles.iter().any(|candidate| candidate == role) {
                continue;
            }
        }
        let effective = node_effective_labels(&node);
        if labels
            .iter()
            .any(|(key, value)| effective.get(key) != Some(value))
        {
            continue;
        }
        nodes.push(node.metadata.name);
    }
    Ok(nodes)
}

pub fn cluster_index() -> anyhow::Result<ClusterIndexResult> {
    let mut visits = read_visit_index_entries()?;
    if env::var_os("JARVIS_NODE_INDEX_LOCAL_ONLY").is_none() {
        let policy = load_or_create_orchestration_policy()?;
        visits.extend(collect_remote_visit_index_entries(
            policy.remote_index_timeout_seconds,
        )?);
        visits.sort_by(|left, right| right.started_at_epoch_ms.cmp(&left.started_at_epoch_ms));
    }
    Ok(ClusterIndexResult {
        generated_at_epoch_ms: now_epoch_ms(),
        sessions: collect_runtime_sessions()?,
        visits,
    })
}

pub fn read_auth_audit_events(limit: Option<usize>) -> anyhow::Result<Vec<AuthAuditEvent>> {
    let path = jarvis_codex_dir()?.join("audit.jsonl");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read audit log '{}'", path.display()))?;
    let mut events = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<AuthAuditEvent>(line).ok())
        .collect::<Vec<_>>();
    events.sort_by(|left, right| right.ts_epoch_ms.cmp(&left.ts_epoch_ms));
    if let Some(limit) = limit {
        events.truncate(limit);
    }
    Ok(events)
}

pub fn orchestration_policy_path() -> anyhow::Result<PathBuf> {
    Ok(jarvis_codex_dir()?.join("orchestration.yaml"))
}

pub fn load_or_create_orchestration_policy() -> anyhow::Result<OrchestrationPolicy> {
    let path = orchestration_policy_path()?;
    if path.exists() {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        let mut policy: OrchestrationPolicy =
            serde_yaml::from_str(&raw).context("failed to parse orchestration policy")?;
        if policy.default_role.trim().is_empty() {
            policy.default_role = default_policy_role();
        }
        return Ok(policy);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let policy = OrchestrationPolicy::default();
    atomic_write_string(&path, &serde_yaml::to_string(&policy)?)?;
    Ok(policy)
}

pub fn start_node_session(
    options: NodeStartSessionOptions,
) -> anyhow::Result<NodeStartSessionResult> {
    let mut attempted = Vec::new();
    let attempts = options.retries.saturating_add(1);
    let mut last_result: Option<NodeStartSessionResult> = None;
    for _ in 0..attempts {
        let node_name = if options.node == "auto" || !attempted.is_empty() {
            schedule_node(NodeScheduleOptions {
                role: options.role.clone().or_else(|| Some(default_policy_role())),
                labels: options.labels.clone(),
                exclude: attempted.clone(),
                require_codex_auth: true,
            })?
            .node
        } else {
            options.node.clone()
        };
        attempted.push(node_name.clone());
        let result = start_node_session_once(&options, &node_name)?;
        if result.exit_status == 0 {
            return Ok(result);
        }
        if !result.retryable {
            return Ok(result);
        }
        last_result = Some(result);
    }
    last_result.ok_or_else(|| anyhow!("remote session did not run any attempts"))
}

fn start_node_session_once(
    options: &NodeStartSessionOptions,
    node_name: &str,
) -> anyhow::Result<NodeStartSessionResult> {
    let manifest = load_manifest(ResourceKind::Node, node_name, None)?;
    let ResourceManifest::Node(node) = manifest else {
        bail!("resource '{}' is not a Node", node_name);
    };
    if node_is_local(&node) {
        return start_local_node_session_once(options, &node);
    }
    let target = node_ssh_target(&node.spec)
        .ok_or_else(|| anyhow!("Node '{}' has no SSH target", node.metadata.name))?;
    let namespace = options.namespace.clone().unwrap_or_else(|| {
        format!(
            "codex-remote-{}-{}",
            slugify(&node.metadata.name),
            now_epoch_ms()
        )
    });
    let auth_files = local_codex_auth_files()?;
    append_auth_audit_event(
        "auth_lease_create_start",
        &node.metadata.name,
        &namespace,
        "start",
        "remote_session",
    )?;
    sync_codex_auth_to_remote_node_for_namespace(&node, &auth_files, &namespace)?;
    append_auth_audit_event(
        "auth_lease_create_complete",
        &node.metadata.name,
        &namespace,
        "ok",
        "remote_session",
    )?;
    let (remote_task_note, staged_task_note) =
        stage_task_note_for_remote_session(&node, &target, &namespace, &options.task_note)?;

    let mut remote_args = vec![
        "jarvisctl".to_string(),
        "codex".to_string(),
        "--driver".to_string(),
        "app-server".to_string(),
        "--task-note".to_string(),
        remote_task_note.display().to_string(),
        "--namespace".to_string(),
        namespace.clone(),
        "--runtime-label".to_string(),
        format!("jarvisctl.io/node={}", node.metadata.name),
        "--runtime-label".to_string(),
        format!(
            "jarvisctl.io/node-address={}",
            node.spec.address.clone().unwrap_or_default()
        ),
        "--agent".to_string(),
        "agent0".to_string(),
        "--startup-delay-ms".to_string(),
        options.startup_delay_ms.to_string(),
    ];
    if let Some(resume_session_id) = options.resume_session_id.as_deref() {
        remote_args.push("--resume-session-id".to_string());
        remote_args.push(resume_session_id.to_string());
    }
    if let Some(working_directory) = options.working_directory.as_deref() {
        remote_args.push("--working-directory".to_string());
        remote_args.push(working_directory.display().to_string());
    }
    if let Some(message) = options.message.as_deref() {
        remote_args.push("--message".to_string());
        remote_args.push(message.to_string());
    }
    if !options.command.is_empty() {
        remote_args.push("--".to_string());
        remote_args.extend(options.command.clone());
    }
    let remote_command = shell_words::join(remote_args);
    let output = ProcessCommand::new("timeout")
        .args([
            "--kill-after=5s",
            "45s",
            "ssh",
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
                "failed to launch remote session '{}' on Node '{}'",
                namespace, node.metadata.name
            )
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_status = output.status.code().unwrap_or(-1);
    let failure_class =
        (exit_status != 0).then(|| classify_failure(&format!("{stderr}\n{stdout}")));
    let retryable = failure_class
        .as_deref()
        .map(failure_is_retryable)
        .unwrap_or(false);
    if exit_status != 0 {
        let _ = cleanup_codex_auth_lease_on_remote_node(&node, &namespace);
    }
    Ok(NodeStartSessionResult {
        node: node.metadata.name,
        namespace,
        task_note: remote_task_note.display().to_string(),
        staged_task_note: staged_task_note.map(|path| path.display().to_string()),
        exit_status,
        stdout,
        stderr,
        failure_class,
        retryable,
    })
}

fn start_local_node_session_once(
    options: &NodeStartSessionOptions,
    node: &ResourceEnvelope<NodeSpec>,
) -> anyhow::Result<NodeStartSessionResult> {
    let namespace = options.namespace.clone().unwrap_or_else(|| {
        format!(
            "codex-local-{}-{}",
            slugify(&node.metadata.name),
            now_epoch_ms()
        )
    });
    let mut labels = BTreeMap::new();
    labels.insert("jarvisctl.io/node".to_string(), node.metadata.name.clone());
    labels.insert(
        "jarvisctl.io/node-address".to_string(),
        node.spec.address.clone().unwrap_or_default(),
    );
    labels.insert("jarvisctl.io/node-local".to_string(), "true".to_string());

    let result = launch_codex_ticket(CodexLaunchOptions {
        backend: SessionBackend::Native,
        driver: CodexRuntimeDriver::AppServer,
        task_note: options.task_note.clone(),
        namespace: Some(namespace.clone()),
        agents: 1,
        agent: "agent0".to_string(),
        fresh_session: options.resume_session_id.is_none(),
        resume_session_id: options.resume_session_id.clone(),
        working_directory: options.working_directory.clone(),
        prompt_file: None,
        operator_message: options.message.clone(),
        images: Vec::new(),
        environment: BTreeMap::new(),
        context_overlay: RuntimeContextMetadata {
            labels,
            ..RuntimeContextMetadata::default()
        },
        extra_runtime_args: Vec::new(),
        startup_delay_ms: options.startup_delay_ms,
        command: options.command.clone(),
    });

    match result {
        Ok(record) => Ok(NodeStartSessionResult {
            node: node.metadata.name.clone(),
            namespace,
            task_note: options.task_note.display().to_string(),
            staged_task_note: None,
            exit_status: 0,
            stdout: serde_json::to_string_pretty(&record).unwrap_or_default(),
            stderr: String::new(),
            failure_class: None,
            retryable: false,
        }),
        Err(error) => Ok(NodeStartSessionResult {
            node: node.metadata.name.clone(),
            namespace,
            task_note: options.task_note.display().to_string(),
            staged_task_note: None,
            exit_status: 1,
            stdout: String::new(),
            stderr: error.to_string(),
            failure_class: Some(classify_failure(&error.to_string())),
            retryable: false,
        }),
    }
}

fn stage_task_note_for_remote_session(
    node: &ResourceEnvelope<NodeSpec>,
    target: &str,
    namespace: &str,
    task_note: &Path,
) -> anyhow::Result<(PathBuf, Option<PathBuf>)> {
    if !task_note.exists() {
        return Ok((task_note.to_path_buf(), None));
    }
    ensure!(
        task_note.is_file(),
        "task note '{}' is not a file",
        task_note.display()
    );
    let remote_home = run_shell_probe(Some(target), "printf '%s' \"$HOME\"", "remote home lookup")?;
    let remote_home = remote_home.trim();
    ensure!(
        !remote_home.is_empty(),
        "Node '{}' returned an empty HOME path",
        node.metadata.name
    );
    let remote_dir = PathBuf::from(remote_home)
        .join(".jarvis")
        .join("codex")
        .join("task-notes")
        .join(slugify(namespace));
    let remote_name = task_note
        .file_name()
        .and_then(|name| name.to_str())
        .map(slugify)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "task-note-md".to_string());
    let remote_path = remote_dir.join(format!("{remote_name}.md"));
    let mkdir_script = format!(
        "mkdir -p {}",
        shell_words::quote(&remote_dir.display().to_string())
    );
    run_shell_probe(
        Some(target),
        &mkdir_script,
        "remote task note staging prepare",
    )?;
    let destination = format!("{target}:{}", remote_path.display());
    let status = ProcessCommand::new("scp")
        .args([
            "-q",
            task_note
                .to_str()
                .ok_or_else(|| anyhow!("task note path is not valid UTF-8"))?,
            &destination,
        ])
        .status()
        .with_context(|| {
            format!(
                "failed to stage task note '{}' on Node '{}'",
                task_note.display(),
                node.metadata.name
            )
        })?;
    ensure!(
        status.success(),
        "task note staging to Node '{}' failed with {status}",
        node.metadata.name
    );
    let staged = remote_path;
    Ok((staged.clone(), Some(staged)))
}

pub fn start_node_pair_session(
    options: NodePairSessionOptions,
) -> anyhow::Result<NodePairSessionResult> {
    ensure!(
        options.first_node != options.second_node,
        "paired node session requires two distinct nodes"
    );
    let coordination_id = options
        .namespace_prefix
        .clone()
        .unwrap_or_else(|| format!("pair-{}", now_epoch_ms()));
    let first_namespace = options.first_namespace.clone().unwrap_or_else(|| {
        format!(
            "{}-{}",
            slugify(&coordination_id),
            slugify(&options.first_node)
        )
    });
    let second_namespace = options.second_namespace.clone().unwrap_or_else(|| {
        format!(
            "{}-{}",
            slugify(&coordination_id),
            slugify(&options.second_node)
        )
    });
    let coordination_note = write_pair_coordination_note(
        &coordination_id,
        &options,
        &first_namespace,
        &second_namespace,
    )?;

    let shared_message = options.message.clone().unwrap_or_else(|| {
        "Coordinate with the paired agent. Exchange concise findings through operator messages, keep node-specific work on your own machine, and ask for partner input when blocked.".to_string()
    });
    let first_intro = pair_intro_message(
        &coordination_id,
        &coordination_note,
        &options.first_node,
        &first_namespace,
        &options.second_node,
        &second_namespace,
        &shared_message,
    );
    let second_intro = pair_intro_message(
        &coordination_id,
        &coordination_note,
        &options.second_node,
        &second_namespace,
        &options.first_node,
        &first_namespace,
        &shared_message,
    );

    let first = start_node_session(NodeStartSessionOptions {
        node: options.first_node.clone(),
        role: Some(default_policy_role()),
        labels: BTreeMap::new(),
        retries: options.retries,
        task_note: options.first_task_note.clone(),
        namespace: Some(first_namespace.clone()),
        resume_session_id: None,
        working_directory: None,
        message: Some(first_intro),
        startup_delay_ms: options.startup_delay_ms,
        command: options.command.clone(),
    })?;
    let second = start_node_session(NodeStartSessionOptions {
        node: options.second_node.clone(),
        role: Some(default_policy_role()),
        labels: BTreeMap::new(),
        retries: options.retries,
        task_note: options.second_task_note.clone(),
        namespace: Some(second_namespace.clone()),
        resume_session_id: None,
        working_directory: None,
        message: Some(second_intro),
        startup_delay_ms: options.startup_delay_ms,
        command: options.command,
    })?;

    if first.exit_status == 0 && second.exit_status == 0 {
        let first_node = load_node_manifest(&first.node)?;
        let second_node = load_node_manifest(&second.node)?;
        let _ = send_runtime_message_to_node_session(
            &first_node,
            &first.namespace,
            &pair_partner_ready_message(&second.node, &second.namespace),
        );
        let _ = send_runtime_message_to_node_session(
            &second_node,
            &second.namespace,
            &pair_partner_ready_message(&first.node, &first.namespace),
        );
    }

    Ok(NodePairSessionResult {
        coordination_id,
        coordination_note: coordination_note.display().to_string(),
        members: vec![
            NodePairMemberResult {
                role: "first".to_string(),
                node: first.node,
                namespace: first.namespace,
                task_note: options.first_task_note.display().to_string(),
                exit_status: first.exit_status,
                failure_class: first.failure_class,
                retryable: first.retryable,
            },
            NodePairMemberResult {
                role: "second".to_string(),
                node: second.node,
                namespace: second.namespace,
                task_note: options.second_task_note.display().to_string(),
                exit_status: second.exit_status,
                failure_class: second.failure_class,
                retryable: second.retryable,
            },
        ],
    })
}

fn load_node_manifest(name: &str) -> anyhow::Result<ResourceEnvelope<NodeSpec>> {
    match load_manifest(ResourceKind::Node, name, None)? {
        ResourceManifest::Node(node) => Ok(node),
        _ => bail!("resource '{}' is not a Node", name),
    }
}

fn write_pair_coordination_note(
    coordination_id: &str,
    options: &NodePairSessionOptions,
    first_namespace: &str,
    second_namespace: &str,
) -> anyhow::Result<PathBuf> {
    let dir = jarvis_codex_dir()?.join("pairs");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.md", slugify(coordination_id)));
    let body = format!(
        "# {coordination_id}\n\n- first_node: {}\n- first_namespace: {first_namespace}\n- first_task_note: {}\n- second_node: {}\n- second_namespace: {second_namespace}\n- second_task_note: {}\n- created_at_epoch_ms: {}\n\n## Coordination\n\n{}\n",
        options.first_node,
        options.first_task_note.display(),
        options.second_node,
        options.second_task_note.display(),
        now_epoch_ms(),
        options
            .message
            .as_deref()
            .unwrap_or("Paired session started.")
    );
    atomic_write_string(&path, &body)?;
    Ok(path)
}

fn pair_intro_message(
    coordination_id: &str,
    coordination_note: &Path,
    own_node: &str,
    own_namespace: &str,
    partner_node: &str,
    partner_namespace: &str,
    shared_message: &str,
) -> String {
    format!(
        "Paired cluster workload '{coordination_id}' started.\n\nYour node: {own_node}\nYour namespace: {own_namespace}\nPartner node: {partner_node}\nPartner namespace: {partner_namespace}\nCoordination note on the control plane: {}\n\nProtocol:\n- Do your node-local part of the task in your own workspace.\n- Send partner messages with: `jarvisctl tell --namespace {partner_namespace} --text '<message>' --mode auto`.\n- Keep partner messages concise and include what you need or what you learned.\n- If you spawn subagents, keep their outputs summarized in this namespace before relaying across nodes.\n- Record final node-local outcome in your ticket.\n\nShared operator message:\n{shared_message}",
        coordination_note.display()
    )
}

fn pair_partner_ready_message(partner_node: &str, partner_namespace: &str) -> String {
    format!(
        "Partner runtime is online: node={partner_node} namespace={partner_namespace}. Begin coordination when your ticket needs partner input."
    )
}

fn send_runtime_message_to_node_session(
    node: &ResourceEnvelope<NodeSpec>,
    namespace: &str,
    message: &str,
) -> anyhow::Result<()> {
    if node_is_local(node) {
        tell_codex_app_with_mode(namespace, message, CodexAppInputMode::Auto)
    } else {
        run_remote_runtime_command(
            node,
            vec![
                "jarvisctl".to_string(),
                "tell".to_string(),
                "--namespace".to_string(),
                namespace.to_string(),
                "--agent".to_string(),
                "agent0".to_string(),
                "--text".to_string(),
                message.to_string(),
                "--mode".to_string(),
                "auto".to_string(),
            ],
        )
    }
}

pub fn reconcile_nodes(max_age_days: u64) -> anyhow::Result<NodeReconcileResult> {
    let doctors = doctor_nodes()?;
    let mut cleanups = Vec::new();
    let mut failures = Vec::new();
    for check in &doctors {
        if !check.available {
            continue;
        }
        match cleanup_node(&check.node, max_age_days) {
            Ok(cleanup) => cleanups.push(cleanup),
            Err(error) => failures.push(NodeFanoutFailure {
                node: check.node.clone(),
                error: error.to_string(),
            }),
        }
    }
    Ok(NodeReconcileResult {
        doctors,
        cleanups,
        failures,
    })
}

pub fn rotate_capsule_key(sync: bool) -> anyhow::Result<CapsuleKeyRotationResult> {
    let path = jarvis_capsule_key_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let rng = rand::SystemRandom::new();
    let mut key = [0_u8; 32];
    rand::SecureRandom::fill(&rng, &mut key)
        .map_err(|_| anyhow!("failed to generate capsule key"))?;
    atomic_write_bytes(&path, &key)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }

    let mut synced_nodes = Vec::new();
    let mut failures = Vec::new();
    if sync {
        for manifest in load_manifests_by_kind(ResourceKind::Node, None)? {
            let ResourceManifest::Node(node) = manifest else {
                continue;
            };
            if node_is_local(&node) {
                continue;
            }
            match sync_capsule_key_to_remote_node(&node) {
                Ok(()) => synced_nodes.push(node.metadata.name),
                Err(error) => failures.push(NodeFanoutFailure {
                    node: node.metadata.name,
                    error: error.to_string(),
                }),
            }
        }
    }
    Ok(CapsuleKeyRotationResult {
        key_path: path.display().to_string(),
        synced_nodes,
        failures,
    })
}

pub fn migrate_session_to_node(
    namespace: &str,
    to_node: &str,
    timeout_seconds: u64,
) -> anyhow::Result<NodeVisitResult> {
    let session = load_runtime_session_by_namespace(namespace)?;
    let session_json = serde_json::to_string_pretty(&session)?;
    let prompt = format!(
        "Resume-style migration capsule for Jarvis runtime session `{namespace}`.\n\nSession metadata:\n```json\n{session_json}\n```\n\nUse this destination node's own vault, memory, and filesystem. Reconstruct the useful working context for this running conversation, inspect any local state that helps, save any relevant notes in this node's memory/vault if appropriate, and return a concise migration summary with next actions."
    );
    run_node_visit(NodeVisitOptions {
        node: to_node.to_string(),
        from_node: None,
        role: None,
        labels: BTreeMap::new(),
        retries: 0,
        prompt,
        working_directory: None,
        namespace: Some(format!(
            "migrate-{}-to-{}-{}",
            slugify(namespace),
            slugify(to_node),
            now_epoch_ms()
        )),
        timeout_seconds,
        sandbox_mode: Some("read-only".to_string()),
        model: None,
        reasoning_effort: None,
        ephemeral: false,
    })
}

pub fn bootstrap_node(options: NodeBootstrapOptions) -> anyhow::Result<Vec<String>> {
    let target = match options.ssh_user.as_deref() {
        Some(user) => format!("{}@{}", user, options.ssh_host),
        None => options.ssh_host.clone(),
    };
    run_shell_probe(
        Some(&target),
        "set -eu; mkdir -p \"$HOME/.local/bin\" \"$HOME/.cargo/bin\" \"$HOME/.jarvis/codex\"",
        "node bootstrap prepare",
    )?;
    let local_arch = run_shell_probe(None, "uname -m", "local bootstrap arch")?
        .trim()
        .to_string();
    let remote_arch = run_shell_probe(Some(&target), "uname -m", "node bootstrap arch")?
        .trim()
        .to_string();
    if local_arch == remote_arch {
        let current_exe =
            env::current_exe().context("failed to locate current jarvisctl binary")?;
        let upload_target = format!("{target}:~/.local/bin/jarvisctl.upload");
        let status = ProcessCommand::new("scp")
            .args([
                "-q",
                current_exe
                    .to_str()
                    .ok_or_else(|| anyhow!("jarvisctl path is not valid UTF-8"))?,
                &upload_target,
            ])
            .status()
            .with_context(|| format!("failed to copy jarvisctl to '{target}'"))?;
        ensure!(
            status.success(),
            "scp jarvisctl to '{target}' failed with {status}"
        );
        run_shell_probe(
            Some(&target),
            "set -eu; mv \"$HOME/.local/bin/jarvisctl.upload\" \"$HOME/.local/bin/jarvisctl\"; chmod 0755 \"$HOME/.local/bin/jarvisctl\"",
            "node bootstrap jarvisctl install",
        )?;
    } else {
        run_shell_probe(
            Some(&target),
            "command -v jarvisctl >/dev/null",
            "node bootstrap jarvisctl check",
        )
        .with_context(|| {
            format!(
                "remote architecture is {remote_arch}, local architecture is {local_arch}; install or build jarvisctl on the remote node first"
            )
        })?;
    }

    let codex_path = options
        .codex_path
        .unwrap_or_else(|| "$HOME/.nvm/versions/node/v24.15.0/bin/codex".to_string());
    let bootstrap_script = format!(
        r#"set -eu
if [ -x "$HOME/.local/bin/jarvisctl" ]; then
  ln -sf "$HOME/.local/bin/jarvisctl" "$HOME/.cargo/bin/jarvisctl"
else
  jarvis_bin="$(command -v jarvisctl)"
  ln -sf "$jarvis_bin" "$HOME/.cargo/bin/jarvisctl"
fi
rm -f "$HOME/.cargo/bin/codex"
cat > "$HOME/.cargo/bin/codex" <<'EOF'
#!/bin/sh
CODEX_BIN="{codex_path}"
case "$CODEX_BIN" in
  \$HOME/*) CODEX_BIN="$HOME/${{CODEX_BIN#\$HOME/}}" ;;
esac
if [ "$CODEX_BIN" = "$HOME/.cargo/bin/codex" ]; then
  echo "bootstrap codex wrapper points to itself" >&2
  exit 126
fi
CODEX_DIR="$(dirname "$CODEX_BIN")"
export PATH="$CODEX_DIR:$PATH"
exec "$CODEX_BIN" "$@"
EOF
chmod 0755 "$HOME/.cargo/bin/codex"
"#,
        codex_path = codex_path
    );
    run_shell_probe(Some(&target), &bootstrap_script, "node bootstrap install")?;
    let mut messages = register_node(NodeRegisterOptions {
        name: options.name,
        address: options.address,
        ssh_host: Some(options.ssh_host),
        ssh_user: options.ssh_user,
        roles: options.roles,
        labels: options.labels,
        workspace_root: options.workspace_root,
        max_sessions: options.max_sessions,
        local: false,
    })?;
    messages.push(format!("bootstrapped remote wrappers on {target}"));
    Ok(messages)
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

pub fn respond_cluster_runtime_server_request(
    namespace: &str,
    request_id: &str,
    response: Option<&serde_json::Value>,
    error: Option<&str>,
) -> anyhow::Result<bool> {
    let Some(node) = remote_node_for_runtime_session(namespace)? else {
        return Ok(false);
    };
    let args = build_respond_request_args(namespace, request_id, response, error)?;
    run_remote_runtime_command(&node, args)?;
    Ok(true)
}

fn build_respond_request_args(
    namespace: &str,
    request_id: &str,
    response: Option<&serde_json::Value>,
    error: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let mut args = vec![
        "jarvisctl".to_string(),
        "respond-request".to_string(),
        "--namespace".to_string(),
        namespace.to_string(),
        "--request-id".to_string(),
        request_id.to_string(),
    ];
    if let Some(error) = error.map(str::trim).filter(|value| !value.is_empty()) {
        args.push("--error".to_string());
        args.push(error.to_string());
    } else if let Some(response) = response {
        args.push("--response-json".to_string());
        args.push(response.to_string());
    } else {
        bail!("provide either a JSON response or an error");
    }
    Ok(args)
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
        ProcessCommand::new("timeout")
            .args([
                "--kill-after=5s",
                "20s",
                "ssh",
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
    let output = ProcessCommand::new("timeout")
        .args([
            "--kill-after=5s",
            "45s",
            "ssh",
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
        "set -u; mkdir -p \"$HOME/.jarvis/codex/visits\"; capsule=\"$HOME/.jarvis/codex/visits/{}.capsule.json\"; out=\"$HOME/.jarvis/codex/visits/{}.last-message.md\"; rm -f \"$capsule\" \"$out\"; cat > \"$capsule\"; jarvisctl capsule-open < \"$capsule\" | ({} {} --output-last-message \"$out\" -); visit_status=$?; printf '\\n__JARVIS_VISIT_LAST_MESSAGE_BEGIN__\\n'; if [ -f \"$out\" ]; then cat \"$out\"; fi; printf '\\n__JARVIS_VISIT_LAST_MESSAGE_END__\\n'; rm -f \"$capsule\"; exit \"$visit_status\"",
        output_name,
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
        let protected_prompt = protect_visit_capsule(&options.prompt)?;
        let stdin = child.stdin.as_mut().context("failed to open visit stdin")?;
        stdin
            .write_all(protected_prompt.as_bytes())
            .context("failed to stream visit prompt to remote Codex")?;
    }

    let output = child
        .wait_with_output()
        .context("failed waiting for remote Codex visit")?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let final_message = extract_visit_final_message(&stdout).unwrap_or_else(|| stdout.clone());
    let exit_status = output.status.code().unwrap_or(-1);
    let failure_class =
        (exit_status != 0).then(|| classify_failure(&format!("{stderr}\n{stdout}")));
    let retryable = failure_class.as_deref().map(failure_is_retryable);

    Ok(NodeVisitResult {
        node: options.node.clone(),
        from_node: None,
        namespace: namespace.to_string(),
        exit_status,
        final_message,
        stdout,
        stderr,
        cleanup_status: "pending".to_string(),
        archive_path: None,
        failure_class,
        retryable,
    })
}

fn run_node_relay_visit(options: NodeVisitOptions) -> anyhow::Result<NodeVisitResult> {
    let mut options = options;
    if options.node == "auto" {
        let scheduled = schedule_node(NodeScheduleOptions {
            role: options.role.clone().or_else(|| Some("worker".to_string())),
            labels: options.labels.clone(),
            require_codex_auth: true,
            exclude: options.from_node.clone().into_iter().collect(),
        })?;
        options.node = scheduled.node;
    }
    let from_node_name = options
        .from_node
        .as_deref()
        .context("relay visit missing from_node")?;
    let manifest = load_manifest(ResourceKind::Node, from_node_name, None)?;
    let ResourceManifest::Node(from_node) = manifest else {
        bail!("resource '{}' is not a Node", from_node_name);
    };
    ensure!(
        !node_is_local(&from_node),
        "relay source '{}' must be a remote SSH node",
        from_node.metadata.name
    );
    let target = node_ssh_target(&from_node.spec)
        .ok_or_else(|| anyhow!("Node '{}' has no SSH target", from_node.metadata.name))?;
    sync_capsule_key_to_remote_node(&from_node)?;
    let namespace = options.namespace.clone().unwrap_or_else(|| {
        format!(
            "visit-{}-to-{}-{}",
            slugify(&from_node.metadata.name),
            slugify(&options.node),
            now_epoch_ms()
        )
    });
    let started_at_epoch_ms = now_epoch_ms();
    write_visit_index_entry(&VisitIndexEntry {
        index_source: None,
        namespace: namespace.clone(),
        node: options.node.clone(),
        from_node: Some(from_node.metadata.name.clone()),
        status: "running".to_string(),
        started_at_epoch_ms,
        finished_at_epoch_ms: None,
        exit_status: None,
        archive_path: None,
        failure_class: None,
        retryable: None,
    })?;
    let mut result = run_remote_jarvis_visit(&target, &options, &namespace)?;
    let finished_at_epoch_ms = now_epoch_ms();
    result.from_node = Some(from_node.metadata.name.clone());
    result.archive_path = Some(
        archive_visit_result(&options, &result, started_at_epoch_ms, finished_at_epoch_ms)?
            .display()
            .to_string(),
    );
    write_visit_index_entry(&VisitIndexEntry {
        index_source: None,
        namespace: namespace.clone(),
        node: result.node.clone(),
        from_node: result.from_node.clone(),
        status: if result.exit_status == 0 {
            "finished".to_string()
        } else {
            "failed".to_string()
        },
        started_at_epoch_ms,
        finished_at_epoch_ms: Some(finished_at_epoch_ms),
        exit_status: Some(result.exit_status),
        archive_path: result.archive_path.clone(),
        failure_class: result.failure_class.clone(),
        retryable: result.retryable,
    })?;
    ensure!(
        result.exit_status == 0,
        "relay visit '{}' from Node '{}' failed with exit status {}: {}",
        namespace,
        from_node.metadata.name,
        result.exit_status,
        result.stderr.trim()
    );
    Ok(result)
}

fn run_remote_jarvis_visit(
    target: &str,
    options: &NodeVisitOptions,
    namespace: &str,
) -> anyhow::Result<NodeVisitResult> {
    let mut visit_args = vec![
        "jarvisctl".to_string(),
        "visit".to_string(),
        "--node".to_string(),
        options.node.clone(),
        "--namespace".to_string(),
        namespace.to_string(),
        "--timeout-seconds".to_string(),
        options.timeout_seconds.to_string(),
        "--sandbox".to_string(),
        options
            .sandbox_mode
            .clone()
            .unwrap_or_else(|| "read-only".to_string()),
        "--full".to_string(),
        "--protected-capsule".to_string(),
    ];
    if let Some(working_directory) = options.working_directory.as_deref() {
        visit_args.push("--working-directory".to_string());
        visit_args.push(working_directory.to_string());
    }
    if let Some(role) = options.role.as_deref() {
        visit_args.push("--role".to_string());
        visit_args.push(role.to_string());
    }
    for (key, value) in &options.labels {
        visit_args.push("--label".to_string());
        visit_args.push(format!("{key}={value}"));
    }
    if options.retries > 0 {
        visit_args.push("--retries".to_string());
        visit_args.push(options.retries.to_string());
    }
    if let Some(model) = options.model.as_deref() {
        visit_args.push("--model".to_string());
        visit_args.push(model.to_string());
    }
    if let Some(reasoning_effort) = options.reasoning_effort.as_deref() {
        visit_args.push("--reasoning-effort".to_string());
        visit_args.push(reasoning_effort.to_string());
    }
    if options.ephemeral {
        visit_args.push("--ephemeral".to_string());
    }

    let remote_script = format!(
        "set -eu; tmp=$(mktemp -t jarvisctl-visit.XXXXXX); trap 'rm -f \"$tmp\"' EXIT; cat > \"$tmp\"; {} --file \"$tmp\"",
        shell_words::join(visit_args),
    );
    let remote_command = shell_words::join([
        "sh".to_string(),
        "-lc".to_string(),
        remote_script.to_string(),
    ]);
    let timeout_duration = format!("{}s", options.timeout_seconds.saturating_add(30));
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
        .with_context(|| format!("failed to start relay visit on '{target}'"))?;
    {
        let protected_prompt = protect_visit_capsule(&options.prompt)?;
        let stdin = child
            .stdin
            .as_mut()
            .context("failed to open relay visit stdin")?;
        stdin
            .write_all(protected_prompt.as_bytes())
            .context("failed to stream relay visit prompt")?;
    }
    let output = child
        .wait_with_output()
        .context("failed waiting for relay visit")?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if output.status.success() {
        let trimmed = stdout.trim();
        if trimmed.starts_with('{') {
            let mut result: NodeVisitResult =
                serde_json::from_str(trimmed).context("failed to parse relay visit JSON")?;
            result.stderr = if result.stderr.is_empty() {
                stderr
            } else if stderr.trim().is_empty() {
                result.stderr
            } else {
                format!("{}\n{}", result.stderr, stderr)
            };
            return Ok(result);
        }
    }
    Ok(NodeVisitResult {
        node: options.node.clone(),
        from_node: options.from_node.clone(),
        namespace: namespace.to_string(),
        exit_status: output.status.code().unwrap_or(-1),
        final_message: stdout.clone(),
        stdout,
        stderr,
        cleanup_status: "unknown".to_string(),
        archive_path: None,
        failure_class: Some(classify_failure("relay visit failed")),
        retryable: Some(true),
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

fn classify_failure(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("permission denied")
        || lower.contains("auth")
        || lower.contains("login")
        || lower.contains("unauthorized")
    {
        "auth".to_string()
    } else if lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("operation timed out")
        || lower.contains("exit status 124")
    {
        "timeout".to_string()
    } else if lower.contains("could not resolve")
        || lower.contains("no route to host")
        || lower.contains("connection refused")
        || lower.contains("connection timed out")
        || lower.contains("ssh")
    {
        "transport".to_string()
    } else if lower.contains("command not found")
        || lower.contains("no such file or directory")
        || lower.contains("jarvisctl_missing")
        || lower.contains("codex_missing")
    {
        "missing_tool".to_string()
    } else if lower.contains("usage limit")
        || lower.contains("rate limit")
        || lower.contains("quota")
    {
        "codex_limit".to_string()
    } else if lower.contains("sandbox") || lower.contains("approval") {
        "policy".to_string()
    } else {
        "unknown".to_string()
    }
}

fn failure_is_retryable(classification: &str) -> bool {
    matches!(
        classification,
        "timeout" | "transport" | "codex_limit" | "unknown"
    )
}

fn extract_auth_url(message: &str) -> Option<String> {
    message
        .split_whitespace()
        .find(|part| part.starts_with("https://login.tailscale.com/a/"))
        .map(|part| part.trim_end_matches(['.', ',', ';']).to_string())
}

#[derive(Debug, Serialize)]
struct VisitArchiveRecord<'a> {
    version: u32,
    started_at_epoch_ms: u128,
    finished_at_epoch_ms: u128,
    duration_ms: u128,
    prompt: &'a str,
    options: VisitArchiveOptions<'a>,
    result: &'a NodeVisitResult,
}

#[derive(Debug, Serialize)]
struct VisitArchiveOptions<'a> {
    node: &'a str,
    from_node: Option<&'a str>,
    working_directory: Option<&'a str>,
    namespace: Option<&'a str>,
    timeout_seconds: u64,
    sandbox_mode: Option<&'a str>,
    model: Option<&'a str>,
    reasoning_effort: Option<&'a str>,
    ephemeral: bool,
}

fn archive_visit_result(
    options: &NodeVisitOptions,
    result: &NodeVisitResult,
    started_at_epoch_ms: u128,
    finished_at_epoch_ms: u128,
) -> anyhow::Result<PathBuf> {
    let archive_dir = jarvis_codex_visits_dir()?;
    fs::create_dir_all(&archive_dir)
        .with_context(|| format!("failed to create '{}'", archive_dir.display()))?;
    let filename = format!(
        "{}-{}-{}.json",
        started_at_epoch_ms,
        slugify(&result.node),
        slugify(&result.namespace)
    );
    let path = archive_dir.join(filename);
    let record = VisitArchiveRecord {
        version: 1,
        started_at_epoch_ms,
        finished_at_epoch_ms,
        duration_ms: finished_at_epoch_ms.saturating_sub(started_at_epoch_ms),
        prompt: &options.prompt,
        options: VisitArchiveOptions {
            node: &options.node,
            from_node: options.from_node.as_deref(),
            working_directory: options.working_directory.as_deref(),
            namespace: options.namespace.as_deref(),
            timeout_seconds: options.timeout_seconds,
            sandbox_mode: options.sandbox_mode.as_deref(),
            model: options.model.as_deref(),
            reasoning_effort: options.reasoning_effort.as_deref(),
            ephemeral: options.ephemeral,
        },
        result,
    };
    let raw = serde_json::to_string_pretty(&record).context("failed to encode visit archive")?;
    atomic_write_string(&path, &raw)?;
    Ok(path)
}

fn jarvis_codex_visits_dir() -> anyhow::Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join(".jarvis")
        .join("codex")
        .join("visits"))
}

fn jarvis_codex_dir() -> anyhow::Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".jarvis").join("codex"))
}

fn jarvis_capsule_key_path() -> anyhow::Result<PathBuf> {
    Ok(jarvis_codex_dir()?.join("capsule.key"))
}

fn load_or_create_capsule_key() -> anyhow::Result<[u8; 32]> {
    let path = jarvis_capsule_key_path()?;
    if path.exists() {
        let raw = fs::read(&path)
            .with_context(|| format!("failed to read capsule key '{}'", path.display()))?;
        ensure!(
            raw.len() == 32,
            "capsule key '{}' must be 32 bytes",
            path.display()
        );
        let mut key = [0_u8; 32];
        key.copy_from_slice(&raw);
        return Ok(key);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let rng = rand::SystemRandom::new();
    let mut key = [0_u8; 32];
    rand::SecureRandom::fill(&rng, &mut key)
        .map_err(|_| anyhow!("failed to generate capsule key"))?;
    fs::write(&path, key)
        .with_context(|| format!("failed to write capsule key '{}'", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(key)
}

#[derive(Debug, Serialize, Deserialize)]
struct ProtectedCapsule {
    version: u32,
    algorithm: String,
    nonce: String,
    ciphertext: String,
}

pub fn protect_visit_capsule(prompt: &str) -> anyhow::Result<String> {
    let key = load_or_create_capsule_key()?;
    let unbound = aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &key)
        .map_err(|_| anyhow!("failed to initialize capsule key"))?;
    let sealing_key = aead::LessSafeKey::new(unbound);
    let rng = rand::SystemRandom::new();
    let mut nonce_bytes = [0_u8; 12];
    rand::SecureRandom::fill(&rng, &mut nonce_bytes)
        .map_err(|_| anyhow!("failed to generate nonce"))?;
    let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = prompt.as_bytes().to_vec();
    sealing_key
        .seal_in_place_append_tag(
            nonce,
            aead::Aad::from(b"jarvisctl-visit-capsule-v1"),
            &mut in_out,
        )
        .map_err(|_| anyhow!("failed to encrypt visit capsule"))?;
    let envelope = ProtectedCapsule {
        version: 1,
        algorithm: "CHACHA20-POLY1305".to_string(),
        nonce: BASE64.encode(nonce_bytes),
        ciphertext: BASE64.encode(in_out),
    };
    serde_json::to_string(&envelope).context("failed to encode protected capsule")
}

pub fn open_visit_capsule(raw: &str) -> anyhow::Result<String> {
    let envelope: ProtectedCapsule =
        serde_json::from_str(raw).context("failed to parse protected capsule")?;
    ensure!(
        envelope.version == 1,
        "unsupported capsule version {}",
        envelope.version
    );
    ensure!(
        envelope.algorithm == "CHACHA20-POLY1305",
        "unsupported capsule algorithm {}",
        envelope.algorithm
    );
    let key = load_or_create_capsule_key()?;
    let nonce_raw = BASE64
        .decode(envelope.nonce.as_bytes())
        .context("failed to decode capsule nonce")?;
    ensure!(nonce_raw.len() == 12, "capsule nonce must be 12 bytes");
    let mut nonce_bytes = [0_u8; 12];
    nonce_bytes.copy_from_slice(&nonce_raw);
    let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = BASE64
        .decode(envelope.ciphertext.as_bytes())
        .context("failed to decode capsule ciphertext")?;
    let unbound = aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &key)
        .map_err(|_| anyhow!("failed to initialize capsule key"))?;
    let opening_key = aead::LessSafeKey::new(unbound);
    let plaintext = opening_key
        .open_in_place(
            nonce,
            aead::Aad::from(b"jarvisctl-visit-capsule-v1"),
            &mut in_out,
        )
        .map_err(|_| anyhow!("failed to authenticate or decrypt capsule"))?;
    String::from_utf8(plaintext.to_vec()).context("capsule plaintext was not UTF-8")
}

fn sync_capsule_key_to_remote_node(node: &ResourceEnvelope<NodeSpec>) -> anyhow::Result<()> {
    if node_is_local(node) {
        let _ = load_or_create_capsule_key()?;
        return Ok(());
    }
    let target = node_ssh_target(&node.spec)
        .ok_or_else(|| anyhow!("Node '{}' has no SSH target", node.metadata.name))?;
    let key_path = jarvis_capsule_key_path()?;
    let _ = load_or_create_capsule_key()?;
    let remote = format!("{target}:~/.jarvis/codex/capsule.key");
    run_shell_probe(
        Some(&target),
        "set -eu; mkdir -p \"$HOME/.jarvis/codex\"; chmod 700 \"$HOME/.jarvis\" \"$HOME/.jarvis/codex\" 2>/dev/null || true",
        "capsule key prepare",
    )?;
    let status = ProcessCommand::new("scp")
        .args([
            "-q",
            key_path
                .to_str()
                .ok_or_else(|| anyhow!("capsule key path is not valid UTF-8"))?,
            &remote,
        ])
        .status()
        .with_context(|| {
            format!(
                "failed to copy capsule key to Node '{}'",
                node.metadata.name
            )
        })?;
    ensure!(
        status.success(),
        "capsule key copy to Node '{}' failed with {status}",
        node.metadata.name
    );
    run_shell_probe(
        Some(&target),
        "chmod 600 \"$HOME/.jarvis/codex/capsule.key\"",
        "capsule key permissions",
    )?;
    Ok(())
}

fn append_auth_audit_event(
    event: &str,
    node: &str,
    namespace: &str,
    status: &str,
    detail: &str,
) -> anyhow::Result<()> {
    let path = jarvis_codex_dir()?.join("audit.jsonl");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let record = json!({
        "ts_epoch_ms": now_epoch_ms(),
        "event": event,
        "node": node,
        "namespace": namespace,
        "status": status,
        "detail": detail,
    });
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open audit log '{}'", path.display()))?;
    writeln!(file, "{}", serde_json::to_string(&record)?)?;
    Ok(())
}

fn visit_index_dir() -> anyhow::Result<PathBuf> {
    Ok(jarvis_codex_dir()?.join("visit-index"))
}

fn write_visit_index_entry(entry: &VisitIndexEntry) -> anyhow::Result<()> {
    let dir = visit_index_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", slugify(&entry.namespace)));
    atomic_write_string(&path, &serde_json::to_string_pretty(entry)?)?;
    Ok(())
}

fn read_visit_index_entries() -> anyhow::Result<Vec<VisitIndexEntry>> {
    let dir = visit_index_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(entry) = serde_json::from_str::<VisitIndexEntry>(&raw) {
                entries.push(entry);
            }
        }
    }
    entries.sort_by(|left, right| right.started_at_epoch_ms.cmp(&left.started_at_epoch_ms));
    Ok(entries)
}

fn collect_remote_visit_index_entries(
    timeout_seconds: u64,
) -> anyhow::Result<Vec<VisitIndexEntry>> {
    let nodes = load_manifests_by_kind(ResourceKind::Node, None)?
        .into_iter()
        .filter_map(|manifest| match manifest {
            ResourceManifest::Node(node) if !node_is_local(&node) => Some(node),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut entries = Vec::new();
    for node in nodes {
        let Some(target) = node_ssh_target(&node.spec) else {
            continue;
        };
        let output = ProcessCommand::new("timeout")
            .args([
                "--kill-after=5s",
                &format!("{}s", timeout_seconds.max(1)),
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                &target,
                "JARVIS_NODE_INDEX_LOCAL_ONLY=1 jarvisctl node index --output json",
            ])
            .output();
        let Ok(output) = output else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let Ok(index) = serde_json::from_slice::<ClusterIndexResult>(&output.stdout) else {
            continue;
        };
        for mut entry in index.visits {
            entry.index_source = Some(node.metadata.name.clone());
            entries.push(entry);
        }
    }
    Ok(entries)
}

fn run_node_inspect_command(target: Option<&str>) -> anyhow::Result<String> {
    let script = r#"printf 'hostname='
(cat /proc/sys/kernel/hostname 2>/dev/null || uname -n 2>/dev/null || true) | head -n 1
printf 'cwd='
pwd
printf 'home='
printf '%s\n' "$HOME"
printf 'arch='
uname -m 2>/dev/null || true
printf '\ncodex_cli='
(codex --version 2>/dev/null || command -v codex 2>/dev/null || true) | head -n 1
printf 'jarvisctl='
(jarvisctl --version 2>/dev/null || command -v jarvisctl 2>/dev/null || true) | head -n 1
printf 'active_sessions='
jarvisctl list --json 2>/dev/null | grep -c '"namespace"' || true
printf 'vault_path=%s/codex\n' "$HOME"
printf 'vault='
test -d "$HOME/codex" && echo present || echo missing
printf 'vault_entries='
if [ -d "$HOME/codex" ]; then find "$HOME/codex" -mindepth 1 -maxdepth 1 -printf '%f,' 2>/dev/null | sed 's/,$//'; fi
printf '\n'
printf 'memory='
test -d "$HOME/.codex/memories" && echo present || echo missing
printf 'work_dir='
test -d "$HOME/work" && echo present || echo missing
printf 'work_entries='
if [ -d "$HOME/work" ]; then find "$HOME/work" -mindepth 1 -maxdepth 1 -printf '%f,' 2>/dev/null | sed 's/,$//'; fi
printf '\n'
printf 'legacy_jarvisctl='
test -d "$HOME/documents/jarvisctl" && echo present || echo missing
printf 'auth_leases='
find "$HOME/.jarvis/codex/auth-leases" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l
printf 'visit_artifacts='
find "$HOME/.jarvis/codex/visits" -type f 2>/dev/null | wc -l
printf 'codex_auth='
test -s "$HOME/.codex/auth.json" && echo present || echo missing"#;
    run_shell_probe(target, script, "node inspect")
}

fn run_shell_probe(target: Option<&str>, script: &str, label: &str) -> anyhow::Result<String> {
    let output = if let Some(target) = target {
        let remote_command =
            shell_words::join(["sh".to_string(), "-lc".to_string(), script.to_string()]);
        ProcessCommand::new("timeout")
            .args([
                "--kill-after=5s",
                "30s",
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                target,
                &remote_command,
            ])
            .output()
            .with_context(|| format!("failed to run {label} for '{target}'"))?
    } else {
        ProcessCommand::new("sh")
            .args(["-lc", script])
            .output()
            .with_context(|| format!("failed to run local {label}"))?
    };
    ensure!(
        output.status.success(),
        "{label} exited with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_node_cleanup_command(
    target: Option<&str>,
    node_name: &str,
    target_label: &str,
    max_age_days: u64,
) -> anyhow::Result<NodeCleanupResult> {
    let script = format!(
        r#"set -u
lease_root="$HOME/.jarvis/codex/auth-leases"
visit_root="$HOME/.jarvis/codex/visits"
active="$(jarvisctl list --json 2>/dev/null || printf '[]')"
restored=""
skipped=""
mkdir -p "$HOME/.codex"
if [ -d "$lease_root" ]; then
  for lease_dir in "$lease_root"/*; do
    [ -d "$lease_dir" ] || continue
    lease="$(basename "$lease_dir")"
    case "$active" in
      *"\"$lease\""*) skipped="${{skipped}}${{lease}},"; continue ;;
    esac
    if [ -d "$lease_dir/backup" ]; then
      for f in auth.json config.toml version.json; do
        if [ -e "$lease_dir/backup/$f" ]; then
          cp -p "$lease_dir/backup/$f" "$HOME/.codex/$f"
        elif [ -e "$lease_dir/backup/$f.missing" ]; then
          rm -f "$HOME/.codex/$f"
        fi
      done
    fi
    rm -rf "$lease_dir"
    restored="${{restored}}${{lease}},"
  done
fi
chmod 700 "$HOME/.codex" 2>/dev/null || true
chmod 600 "$HOME/.codex/auth.json" "$HOME/.codex/config.toml" "$HOME/.codex/version.json" 2>/dev/null || true
removed=0
if [ -d "$visit_root" ]; then
  while IFS= read -r file; do
    [ -n "$file" ] || continue
    rm -f "$file" && removed=$((removed + 1))
  done <<EOF
$(find "$visit_root" -type f -mtime +{} 2>/dev/null)
EOF
fi
printf 'restored_leases=%s\n' "${{restored%,}}"
printf 'skipped_active_leases=%s\n' "${{skipped%,}}"
printf 'removed_visit_artifacts=%s\n' "$removed"
"#,
        max_age_days
    );

    let output = if let Some(target) = target {
        let remote_command = shell_words::join(["sh".to_string(), "-lc".to_string(), script]);
        ProcessCommand::new("timeout")
            .args([
                "--kill-after=5s",
                "45s",
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                target,
                &remote_command,
            ])
            .output()
            .with_context(|| format!("failed to run cleanup for '{target}'"))?
    } else {
        ProcessCommand::new("sh")
            .args(["-lc", &script])
            .output()
            .context("failed to run local cleanup")?
    };
    ensure!(
        output.status.success(),
        "cleanup exited with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let values = parse_probe_output(&String::from_utf8_lossy(&output.stdout));
    Ok(NodeCleanupResult {
        node: node_name.to_string(),
        target: target_label.to_string(),
        restored_leases: split_csv(values.get("restored_leases")),
        skipped_active_leases: split_csv(values.get("skipped_active_leases")),
        removed_visit_artifacts: values
            .get("removed_visit_artifacts")
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

fn split_csv(value: Option<&String>) -> Vec<String> {
    value
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
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

pub fn validate_worker_lanes() -> anyhow::Result<WorkerValidationReport> {
    let workers = list_worker_summaries(None)?;
    let ready_workers = workers
        .iter()
        .filter(|worker| !worker.detail.contains("no resolved endpoints"))
        .count();
    let status = if ready_workers > 0 {
        "passed"
    } else {
        "skipped"
    }
    .to_string();
    let detail = if ready_workers > 0 {
        format!(
            "{ready_workers}/{} worker lane(s) have ready endpoints",
            workers.len()
        )
    } else if workers.is_empty() {
        "no service-backed worker lanes are registered".to_string()
    } else {
        format!(
            "{} worker lane(s) registered but none have ready endpoints",
            workers.len()
        )
    };
    ensure!(ready_workers > 0, "{detail}");
    Ok(WorkerValidationReport {
        status,
        workers: workers.len(),
        ready_workers,
        detail,
    })
}

pub fn run_worker_offload(options: WorkerOffloadOptions) -> anyhow::Result<WorkerOffloadReport> {
    let control_namespace =
        normalize_namespaced_resource_namespace(options.control_namespace.as_deref());
    let service_name = options.service_name.trim().to_string();
    ensure!(
        !service_name.is_empty(),
        "worker offload requires --service"
    );
    let prompt = options.prompt.trim().to_string();
    ensure!(!prompt.is_empty(), "worker offload requires --prompt");
    let service_manifest = load_manifest(
        ResourceKind::Service,
        &service_name,
        Some(&control_namespace),
    )?;
    let ResourceManifest::Service(service) = service_manifest else {
        bail!(
            "resource '{}/{}' is not a Service",
            control_namespace,
            service_name
        );
    };
    if effective_service_target_kind(&service.spec) == ServiceTargetKind::Worker {
        let worker = select_worker_for_service(&service)?;
        let output_path = options.output_path.map(|path| expand_home_pathbuf(&path));
        let job_name = options.job_name.unwrap_or_else(|| {
            format!(
                "{}-{}-{}",
                slugify(&worker.metadata.name),
                slugify(&service_name),
                now_epoch_ms()
            )
        });
        let response = format!(
            "Accepted bounded offload for worker `{}/{}` through service `{}/{}`. Prompt bytes: {}.",
            worker
                .metadata
                .namespace
                .as_deref()
                .unwrap_or(control_namespace.as_str()),
            worker.metadata.name,
            control_namespace,
            service_name,
            prompt.len()
        );
        if let Some(path) = output_path.as_ref() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create '{}'", parent.display()))?;
            }
            fs::write(path, &response)
                .with_context(|| format!("failed to write '{}'", path.display()))?;
        }
        return Ok(WorkerOffloadReport {
            job_name,
            namespace: control_namespace.clone(),
            service_name,
            phase: "accepted".to_string(),
            selected_class: service.spec.class_name.clone(),
            fallback_class: false,
            worker: Some(worker.metadata.name.clone()),
            worker_namespace: worker.metadata.namespace.clone(),
            worker_provider: Some(worker.spec.provider.clone()),
            worker_model: Some(worker.spec.model.clone()),
            worker_locality: worker.spec.locality.clone(),
            validation_state: Some("accepted".to_string()),
            validation_message: Some(
                "Worker service matched a registered worker manifest.".to_string(),
            ),
            artifact_path: None,
            output_path: output_path.map(|path| path.display().to_string()),
            response: Some(response),
        });
    }
    let target = resolve_service_target_for_message(
        &service_name,
        Some(&control_namespace),
        options.via_runtime_namespace.as_deref(),
    )?;
    let intent_prefix = options
        .intent
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|intent| format!("Intent: {intent}\n\n"))
        .unwrap_or_default();
    let message = format!(
        "{intent_prefix}Bounded worker offload request via service `{}/{}`.\n\n{}\n\nReturn a concise result and mention any artifact path you create.",
        control_namespace, service_name, prompt
    );
    if !tell_cluster_runtime_session(&target.runtime_namespace, "agent0", &message, "auto")? {
        tell_codex_app_with_mode(&target.runtime_namespace, &message, CodexAppInputMode::Auto)?;
    }
    let output_path = options.output_path.map(|path| expand_home_pathbuf(&path));
    let job_name = options.job_name.unwrap_or_else(|| {
        format!(
            "{}-{}-{}",
            slugify(&target.runtime_namespace),
            slugify(&service_name),
            now_epoch_ms()
        )
    });
    let response = format!(
        "Dispatched bounded offload to runtime `{}` through service `{}/{}`.",
        target.runtime_namespace, control_namespace, service_name
    );
    if let Some(path) = output_path.as_ref() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create '{}'", parent.display()))?;
        }
        fs::write(path, &response)
            .with_context(|| format!("failed to write '{}'", path.display()))?;
    }
    Ok(WorkerOffloadReport {
        job_name,
        namespace: control_namespace.clone(),
        service_name,
        phase: "dispatched".to_string(),
        selected_class: Some("runtime".to_string()),
        fallback_class: false,
        worker: Some(target.runtime_namespace),
        worker_namespace: Some(control_namespace),
        worker_provider: Some("codex".to_string()),
        worker_model: Some("codex".to_string()),
        worker_locality: Some("cluster".to_string()),
        validation_state: Some("accepted".to_string()),
        validation_message: Some("Service endpoint resolved and prompt was delivered.".to_string()),
        artifact_path: None,
        output_path: output_path.map(|path| path.display().to_string()),
        response: Some(response),
    })
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
        "Worker" => {
            let mut manifest: ResourceEnvelope<WorkerSpec> =
                serde_yaml::from_value(value).context("failed to decode Worker manifest")?;
            normalize_metadata(&mut manifest.metadata, false)?;
            validate_worker(&manifest)?;
            manifest.api_version = API_VERSION.to_string();
            manifest.kind = "Worker".to_string();
            Ok(ResourceManifest::Worker(manifest))
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

fn validate_worker(manifest: &ResourceEnvelope<WorkerSpec>) -> anyhow::Result<()> {
    ensure!(
        !manifest.spec.provider.trim().is_empty(),
        "Worker '{}' must set spec.provider",
        manifest.metadata.name
    );
    ensure!(
        !manifest.spec.model.trim().is_empty(),
        "Worker '{}' must set spec.model",
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
    let output = ProcessCommand::new("timeout")
        .args([
            "--kill-after=5s",
            "45s",
            "ssh",
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

fn expand_home_pathbuf(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    path.to_path_buf()
}

fn select_worker_for_service(
    service: &ResourceEnvelope<ServiceSpec>,
) -> anyhow::Result<ResourceEnvelope<WorkerSpec>> {
    let namespace = service.metadata.namespace.as_deref();
    let mut workers = load_manifests_by_kind(ResourceKind::Worker, namespace)?
        .into_iter()
        .filter_map(|manifest| match manifest {
            ResourceManifest::Worker(worker) => Some(worker),
            _ => None,
        })
        .filter(|worker| worker_matches_service(service, worker))
        .collect::<Vec<_>>();
    workers.sort_by(|left, right| left.metadata.name.cmp(&right.metadata.name));
    workers.into_iter().next().ok_or_else(|| {
        anyhow!(
            "worker service '{}/{}' has no matching Worker manifests",
            service.metadata.namespace.as_deref().unwrap_or("default"),
            service.metadata.name
        )
    })
}

fn worker_matches_service(
    service: &ResourceEnvelope<ServiceSpec>,
    worker: &ResourceEnvelope<WorkerSpec>,
) -> bool {
    if !service
        .spec
        .selector
        .iter()
        .all(|(key, value)| worker.metadata.labels.get(key) == Some(value))
    {
        return false;
    }
    if let Some(class_name) = service.spec.class_name.as_ref() {
        if !worker.spec.classes.iter().any(|class| class == class_name) {
            return false;
        }
    }
    if !service.spec.required_capabilities.is_empty()
        && !service.spec.required_capabilities.iter().all(|capability| {
            worker
                .spec
                .capabilities
                .iter()
                .any(|value| value == capability)
        })
    {
        return false;
    }
    if !service.spec.preferred_providers.is_empty()
        && !service
            .spec
            .preferred_providers
            .iter()
            .any(|provider| provider == &worker.spec.provider)
    {
        return false;
    }
    true
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
        let output = ProcessCommand::new("timeout")
            .args([
                "--kill-after=5s",
                "25s",
                "ssh",
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
    let output = ProcessCommand::new("timeout")
        .args([
            "--kill-after=5s",
            "45s",
            "ssh",
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
        ControlPlaneResourceKindArg::Worker => Ok(ResourceKind::Worker),
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
            ResourceKind::Worker => "workers",
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
            ResourceKind::Worker => "Worker",
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
            ResourceManifest::Worker(_) => ResourceKind::Worker,
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
            ResourceManifest::Worker(manifest) => &manifest.metadata.name,
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
            ResourceManifest::Worker(manifest) => manifest.metadata.namespace.as_deref(),
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
            ResourceManifest::Worker(manifest) => &mut manifest.metadata,
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
    use crate::native::NativeAgentMetadata;
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

    fn write_codex_app_metadata(home: &Path, metadata: &NativeSessionMetadata) {
        let session_dir = home
            .join(".jarvis")
            .join("codex-app")
            .join("sessions")
            .join(&metadata.namespace);
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("metadata.json"),
            serde_json::to_string_pretty(metadata).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn service_backed_worker_lanes_are_listed_and_validated() {
        let _lock = home_env_lock().lock().unwrap();
        let home = TempHomeGuard::new("jarvisctl-service-worker-lane");
        write_codex_app_metadata(
            &home.root,
            &NativeSessionMetadata {
                namespace: "worker-runtime".to_string(),
                backend: "codex-app".to_string(),
                created_at_epoch_ms: now_epoch_ms(),
                working_directory: None,
                shell_command: "codex".to_string(),
                context: Some(RuntimeContextMetadata {
                    control_namespace: Some("team-alpha".to_string()),
                    labels: BTreeMap::from([("app".to_string(), "worker-runtime".to_string())]),
                    ..RuntimeContextMetadata::default()
                }),
                agents: vec![NativeAgentMetadata {
                    name: "agent0".to_string(),
                    pid: 42,
                    running: true,
                    exit_code: None,
                }],
            },
        );
        save_manifest(&ResourceManifest::Service(ResourceEnvelope {
            api_version: API_VERSION.to_string(),
            kind: "Service".to_string(),
            metadata: ResourceMetadata {
                name: "worker-lane".to_string(),
                namespace: Some("team-alpha".to_string()),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
            },
            spec: ServiceSpec {
                selector: BTreeMap::from([("app".to_string(), "worker-runtime".to_string())]),
                allowed_intents: vec!["summarize".to_string()],
                ..ServiceSpec::default()
            },
        }))
        .unwrap();

        let workers = list_worker_summaries(Some("team-alpha")).unwrap();
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].kind, "Worker");
        assert_eq!(workers[0].name, "worker-lane");
        assert!(workers[0].detail.contains("worker-runtime"));

        let detail = worker_describe_envelope("worker-lane", Some("team-alpha")).unwrap();
        assert!(detail.status.loaded);
        assert_eq!(detail.status.available_slots, 1);
        assert_eq!(detail.status.allowed_intents, vec!["summarize"]);

        let validation = validate_worker_lanes().unwrap();
        assert_eq!(validation.status, "passed");
        assert_eq!(validation.ready_workers, 1);
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

    #[test]
    fn classifies_retryable_and_non_retryable_failures() {
        assert_eq!(
            classify_failure("ERROR: You've hit your usage limit."),
            "codex_limit"
        );
        assert!(failure_is_retryable("codex_limit"));

        assert_eq!(classify_failure("Permission denied (publickey)."), "auth");
        assert!(!failure_is_retryable("auth"));

        assert_eq!(
            classify_failure("ssh: connect to host node port 22: Connection timed out"),
            "timeout"
        );
        assert!(failure_is_retryable("timeout"));

        assert_eq!(
            classify_failure("jarvisctl: command not found"),
            "missing_tool"
        );
        assert!(!failure_is_retryable("missing_tool"));
    }

    #[test]
    fn extracts_tailscale_auth_url_from_probe_output() {
        let message = "# Tailscale SSH requires an additional check.\n# To authenticate, visit: https://login.tailscale.com/a/l6c9487034871a";
        assert_eq!(
            extract_auth_url(message).as_deref(),
            Some("https://login.tailscale.com/a/l6c9487034871a")
        );
        assert!(extract_auth_url("plain ssh failure").is_none());
    }

    #[test]
    fn builds_remote_app_server_request_response_command() {
        let response = serde_json::json!({"approved": true, "mode": "headless"});
        assert_eq!(
            build_respond_request_args("codex-ns", "req-42", Some(&response), None).unwrap(),
            vec![
                "jarvisctl".to_string(),
                "respond-request".to_string(),
                "--namespace".to_string(),
                "codex-ns".to_string(),
                "--request-id".to_string(),
                "req-42".to_string(),
                "--response-json".to_string(),
                "{\"approved\":true,\"mode\":\"headless\"}".to_string(),
            ]
        );

        assert_eq!(
            build_respond_request_args("codex-ns", "req-42", None, Some("denied")).unwrap(),
            vec![
                "jarvisctl".to_string(),
                "respond-request".to_string(),
                "--namespace".to_string(),
                "codex-ns".to_string(),
                "--request-id".to_string(),
                "req-42".to_string(),
                "--error".to_string(),
                "denied".to_string(),
            ]
        );
    }
}
