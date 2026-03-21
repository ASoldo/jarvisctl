use crate::SessionBackend;
use crate::codex::{
    CodexLaunchOptions, CodexRuntimeDriver, enrich_native_sessions, launch_codex_ticket,
};
use crate::codex_app::{collect_codex_app_sessions, delete_codex_app_session};
use crate::native::{
    NativeSessionMetadata, RuntimeContextMetadata, collect_native_sessions, delete_native_session,
};
use crate::ticket::slugify;
use anyhow::{Context, anyhow, bail, ensure};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_yaml::Value;
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const API_VERSION: &str = "jarvisctl.io/v1alpha1";

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
    Namespace,
    Deployment,
    Service,
    NetworkPolicy,
    ConfigMap,
    Secret,
    Volume,
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
            driver: None,
            startup_delay_ms: None,
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
    Namespace(ResourceEnvelope<NamespaceSpec>),
    Deployment(ResourceEnvelope<DeploymentSpec>),
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
struct DeploymentStatus {
    replicas: usize,
    ready_replicas: usize,
    sessions: Vec<String>,
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

#[derive(Debug, Clone)]
pub struct ServiceResolution {
    pub runtime_namespace: String,
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
        manifests.extend(parse_manifest_documents(&raw)?);
    }

    let mut messages = Vec::new();
    for manifest in &manifests {
        save_manifest(manifest)?;
        if !matches!(manifest, ResourceManifest::Deployment(_)) {
            messages.push(format!("applied {}", manifest_ref(manifest)));
        }
    }
    for manifest in &manifests {
        if let Some(message) = reconcile_manifest(manifest)? {
            messages.push(message);
        }
    }
    Ok(messages)
}

pub fn render_get_output(
    kind_arg: ControlPlaneResourceKindArg,
    namespace: Option<&str>,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
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
        !manifest.spec.template.task_note.trim().is_empty(),
        "Deployment '{}' must set spec.template.task_note",
        manifest.metadata.name
    );
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

fn save_manifest(manifest: &ResourceManifest) -> anyhow::Result<()> {
    let path = manifest_path(manifest.kind(), manifest.name(), manifest.namespace())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    let raw = serde_yaml::to_string(manifest).context("failed to encode manifest")?;
    fs::write(&path, raw).with_context(|| format!("failed to write '{}'", path.display()))
}

fn reconcile_manifest(manifest: &ResourceManifest) -> anyhow::Result<Option<String>> {
    match manifest {
        ResourceManifest::Deployment(deployment) => reconcile_deployment(deployment).map(Some),
        _ => Ok(None),
    }
}

fn reconcile_deployment(manifest: &ResourceEnvelope<DeploymentSpec>) -> anyhow::Result<String> {
    let control_namespace = manifest
        .metadata
        .namespace
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let deployment_name = manifest.metadata.name.clone();
    let desired_namespaces =
        desired_runtime_namespaces(&control_namespace, &deployment_name, manifest.spec.replicas);
    let desired_set: HashSet<String> = desired_namespaces.iter().cloned().collect();

    let current_sessions = collect_runtime_sessions()?;
    let managed_sessions = current_sessions
        .iter()
        .filter(|session| {
            session
                .context
                .as_ref()
                .and_then(|context| context.control_namespace.as_deref())
                == Some(control_namespace.as_str())
                && session
                    .context
                    .as_ref()
                    .and_then(|context| context.deployment.as_deref())
                    == Some(deployment_name.as_str())
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
        let healthy = session.agents.iter().any(|agent| agent.running);
        if healthy {
            existing_names.insert(session.namespace.clone());
        } else {
            delete_runtime_session(session)?;
        }
    }
    for (ordinal, runtime_namespace) in desired_namespaces.iter().enumerate() {
        if existing_names.contains(runtime_namespace) {
            continue;
        }
        launch_deployment_replica(manifest, ordinal, runtime_namespace)?;
    }

    let refreshed = collect_runtime_sessions()?;
    let ready_replicas = refreshed
        .iter()
        .filter(|session| desired_set.contains(&session.namespace))
        .filter(|session| session.agents.iter().any(|agent| agent.running))
        .count();
    Ok(format!(
        "applied deployment {}/{} (ready {}/{})",
        control_namespace, deployment_name, ready_replicas, manifest.spec.replicas
    ))
}

fn launch_deployment_replica(
    manifest: &ResourceEnvelope<DeploymentSpec>,
    ordinal: usize,
    runtime_namespace: &str,
) -> anyhow::Result<()> {
    let control_namespace = manifest
        .metadata
        .namespace
        .clone()
        .unwrap_or_else(|| "default".to_string());
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
        &runtime_identity_environment(
            &control_namespace,
            &manifest.metadata.name,
            runtime_namespace,
            ordinal,
        ),
    );
    let context_overlay = RuntimeContextMetadata {
        control_namespace: Some(control_namespace.clone()),
        deployment: Some(manifest.metadata.name.clone()),
        labels: deployment_labels(manifest, ordinal),
        config_maps: manifest.spec.template.config_maps.clone(),
        secrets: manifest.spec.template.secrets.clone(),
        volumes: manifest.spec.template.volumes.clone(),
        ..RuntimeContextMetadata::default()
    };
    let operator_message = build_control_plane_operator_message(
        manifest,
        ordinal,
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

    launch_codex_ticket(CodexLaunchOptions {
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
    })?;
    Ok(())
}

fn build_control_plane_operator_message(
    manifest: &ResourceEnvelope<DeploymentSpec>,
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
        format!(
            "- Control namespace: {}",
            manifest.metadata.namespace.as_deref().unwrap_or("default")
        ),
        format!("- Deployment: {}", manifest.metadata.name),
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

fn deployment_labels(
    manifest: &ResourceEnvelope<DeploymentSpec>,
    ordinal: usize,
) -> BTreeMap<String, String> {
    let mut labels = manifest.metadata.labels.clone();
    labels.extend(manifest.spec.template.labels.clone());
    labels.insert(
        "jarvisctl.io/control-namespace".to_string(),
        manifest
            .metadata
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string()),
    );
    labels.insert(
        "jarvisctl.io/deployment".to_string(),
        manifest.metadata.name.clone(),
    );
    labels.insert(
        "jarvisctl.io/replica-ordinal".to_string(),
        ordinal.to_string(),
    );
    labels
}

fn desired_runtime_namespaces(
    control_namespace: &str,
    deployment_name: &str,
    replicas: usize,
) -> Vec<String> {
    (0..replicas)
        .map(|ordinal| {
            format!(
                "{}--{}--r{}",
                slugify(control_namespace),
                slugify(deployment_name),
                ordinal
            )
        })
        .collect()
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
                detail: status.sessions.join(", "),
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
    let desired_namespaces = desired_runtime_namespaces(
        manifest.metadata.namespace.as_deref().unwrap_or("default"),
        &manifest.metadata.name,
        manifest.spec.replicas,
    );
    let desired_set: HashSet<String> = desired_namespaces.iter().cloned().collect();
    let sessions = collect_runtime_sessions()?
        .into_iter()
        .filter(|session| desired_set.contains(&session.namespace))
        .collect::<Vec<_>>();
    let ready_replicas = sessions
        .iter()
        .filter(|session| session.agents.iter().any(|agent| agent.running))
        .count();
    Ok(DeploymentStatus {
        replicas: manifest.spec.replicas,
        ready_replicas,
        sessions: desired_namespaces,
    })
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

fn describe_status(manifest: &ResourceManifest) -> anyhow::Result<serde_json::Value> {
    match manifest {
        ResourceManifest::Namespace(namespace) => Ok(serde_json::to_value(namespace_status(
            &namespace.metadata.name,
        )?)?),
        ResourceManifest::Deployment(deployment) => {
            Ok(serde_json::to_value(deployment_status(deployment)?)?)
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
        ResourceKind::Deployment,
        ResourceKind::Service,
        ResourceKind::NetworkPolicy,
        ResourceKind::ConfigMap,
        ResourceKind::Secret,
        ResourceKind::Volume,
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
        ControlPlaneResourceKindArg::Service => Ok(ResourceKind::Service),
        ControlPlaneResourceKindArg::NetworkPolicy => Ok(ResourceKind::NetworkPolicy),
        ControlPlaneResourceKindArg::ConfigMap => Ok(ResourceKind::ConfigMap),
        ControlPlaneResourceKindArg::Secret => Ok(ResourceKind::Secret),
        ControlPlaneResourceKindArg::Volume => Ok(ResourceKind::Volume),
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
            ResourceKind::Service => "services",
            ResourceKind::NetworkPolicy => "networkpolicies",
            ResourceKind::ConfigMap => "configmaps",
            ResourceKind::Secret => "secrets",
            ResourceKind::Volume => "volumes",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            ResourceKind::Namespace => "Namespace",
            ResourceKind::Deployment => "Deployment",
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
            ResourceManifest::Namespace(_) => ResourceKind::Namespace,
            ResourceManifest::Deployment(_) => ResourceKind::Deployment,
            ResourceManifest::Service(_) => ResourceKind::Service,
            ResourceManifest::NetworkPolicy(_) => ResourceKind::NetworkPolicy,
            ResourceManifest::ConfigMap(_) => ResourceKind::ConfigMap,
            ResourceManifest::Secret(_) => ResourceKind::Secret,
            ResourceManifest::Volume(_) => ResourceKind::Volume,
        }
    }

    fn name(&self) -> &str {
        match self {
            ResourceManifest::Namespace(manifest) => &manifest.metadata.name,
            ResourceManifest::Deployment(manifest) => &manifest.metadata.name,
            ResourceManifest::Service(manifest) => &manifest.metadata.name,
            ResourceManifest::NetworkPolicy(manifest) => &manifest.metadata.name,
            ResourceManifest::ConfigMap(manifest) => &manifest.metadata.name,
            ResourceManifest::Secret(manifest) => &manifest.metadata.name,
            ResourceManifest::Volume(manifest) => &manifest.metadata.name,
        }
    }

    fn namespace(&self) -> Option<&str> {
        match self {
            ResourceManifest::Namespace(_) => None,
            ResourceManifest::Deployment(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::Service(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::NetworkPolicy(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::ConfigMap(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::Secret(manifest) => manifest.metadata.namespace.as_deref(),
            ResourceManifest::Volume(manifest) => manifest.metadata.namespace.as_deref(),
        }
    }
}

fn default_replicas() -> usize {
    1
}

fn default_agents() -> usize {
    1
}
