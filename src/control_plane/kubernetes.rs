use super::*;
use std::process::{Command as ProcessCommand, Stdio};

#[derive(Debug, Clone)]
pub(crate) struct KubernetesCompilation {
    pub(crate) manifests: Vec<serde_json::Value>,
    pub(crate) warnings: Vec<String>,
}

pub fn render_kubernetes_resources(
    files: &[PathBuf],
    kustomize: Option<&Path>,
    output: KubernetesRenderOutput,
) -> anyhow::Result<String> {
    let manifests = load_source_manifests(files, kustomize)?;
    let compiled = compile_kubernetes_manifests(&manifests)?;
    ensure!(
        !compiled.manifests.is_empty(),
        "no Kubernetes resources were generated from the provided jarvisctl manifests"
    );
    match output {
        KubernetesRenderOutput::Json => serde_json::to_string_pretty(&compiled.manifests)
            .context("failed to encode Kubernetes manifests"),
        KubernetesRenderOutput::Yaml => render_kubernetes_yaml_documents(&compiled.manifests),
    }
}

pub fn apply_kubernetes_resources(
    files: &[PathBuf],
    kustomize: Option<&Path>,
    kubectl_context: Option<&str>,
    dry_run_server: bool,
) -> anyhow::Result<String> {
    let manifests = load_source_manifests(files, kustomize)?;
    let compiled = compile_kubernetes_manifests(&manifests)?;
    ensure!(
        !compiled.manifests.is_empty(),
        "no Kubernetes resources were generated from the provided jarvisctl manifests"
    );

    let mut messages = Vec::new();
    let namespace_manifests = compiled
        .manifests
        .iter()
        .filter(|manifest| {
            manifest.get("kind").and_then(serde_json::Value::as_str) == Some("Namespace")
        })
        .cloned()
        .collect::<Vec<_>>();
    let namespaced_manifests = compiled
        .manifests
        .iter()
        .filter(|manifest| {
            manifest.get("kind").and_then(serde_json::Value::as_str) != Some("Namespace")
        })
        .cloned()
        .collect::<Vec<_>>();

    let mut dry_run_mode = if dry_run_server { Some("server") } else { None };
    if dry_run_server {
        let missing_namespaces = namespaced_manifests
            .iter()
            .filter_map(|manifest| {
                manifest
                    .get("metadata")
                    .and_then(|metadata| metadata.get("namespace"))
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|namespace| !kubernetes_namespace_exists(kubectl_context, namespace))
            .collect::<Vec<_>>();
        if !missing_namespaces.is_empty() {
            dry_run_mode = Some("client");
            messages.push(format!(
                "server dry-run downgraded to client dry-run because these target namespaces do not exist yet: {}",
                missing_namespaces.join(", ")
            ));
        }
    }

    if !namespace_manifests.is_empty() {
        if let Some(output) =
            kubectl_apply_rendered_documents(&namespace_manifests, kubectl_context, dry_run_mode)?
        {
            messages.push(output);
        }
    }
    if !namespaced_manifests.is_empty() {
        if let Some(output) =
            kubectl_apply_rendered_documents(&namespaced_manifests, kubectl_context, dry_run_mode)?
        {
            messages.push(output);
        }
    }
    if !compiled.warnings.is_empty() {
        messages.push(format!(
            "compiler warnings:\n{}",
            compiled
                .warnings
                .iter()
                .map(|warning| format!("- {}", warning))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    Ok(messages.join("\n"))
}

pub(crate) fn compile_kubernetes_manifests(
    manifests: &[ResourceManifest],
) -> anyhow::Result<KubernetesCompilation> {
    let mut rendered = Vec::new();
    let mut warnings = Vec::new();
    for manifest in manifests {
        match manifest {
            ResourceManifest::Node(node) => warnings.push(format!(
                "skipped Node '{}' because Jarvis nodes are control-plane inventory, not Kubernetes objects",
                node.metadata.name
            )),
            ResourceManifest::Namespace(namespace) => {
                rendered.push(kubernetes_namespace_value(namespace));
            }
            ResourceManifest::ConfigMap(config_map) => {
                rendered.push(kubernetes_config_map_value(config_map));
            }
            ResourceManifest::Secret(secret) => {
                rendered.push(kubernetes_secret_value(secret));
            }
            ResourceManifest::NetworkPolicy(policy) => {
                rendered.push(kubernetes_network_policy_value(policy));
            }
            ResourceManifest::Service(service) => {
                if let Some(value) = kubernetes_runtime_service_value(service, manifests)? {
                    rendered.push(value);
                } else {
                    warnings.push(format!(
                        "skipped runtime Service '{}/{}' because no matching Kubernetes runtime Deployment exposed a control port",
                        service.namespace_key(),
                        service.metadata.name
                    ));
                }
            }
            ResourceManifest::Deployment(deployment) => {
                if let Some(values) = kubernetes_deployment_values(deployment, manifests)? {
                    rendered.extend(values);
                } else {
                    warnings.push(format!(
                        "skipped Deployment '{}/{}' because spec.template.kubernetes is not set",
                        deployment.namespace_key(),
                        deployment.metadata.name
                    ));
                }
            }
            ResourceManifest::ReplicaSet(replica_set) => warnings.push(format!(
                "skipped ReplicaSet '{}/{}' because ReplicaSets are controller-generated local rollout history",
                replica_set.namespace_key(),
                replica_set.metadata.name
            )),
            ResourceManifest::Volume(volume) => warnings.push(format!(
                "skipped Volume '{}/{}' because jarvisctl Volume resources do not yet declare a Kubernetes storage class or claim template",
                volume.namespace_key(),
                volume.metadata.name
            )),
        }
    }
    Ok(KubernetesCompilation {
        manifests: rendered,
        warnings,
    })
}

pub(crate) fn validate_kubernetes_runtime(
    runtime: &KubernetesRuntimeSpec,
    kind: &str,
    name: &str,
) -> anyhow::Result<()> {
    if let Some(image) = runtime.image.as_deref() {
        ensure!(
            !image.trim().is_empty(),
            "{} '{}' has an empty kubernetes.image",
            kind,
            name
        );
    }
    if let Some(policy) = runtime.image_pull_policy.as_deref() {
        ensure!(
            !policy.trim().is_empty(),
            "{} '{}' has an empty kubernetes.imagePullPolicy",
            kind,
            name
        );
    }
    if let Some(service_account_name) = runtime.service_account_name.as_deref() {
        ensure!(
            !service_account_name.trim().is_empty(),
            "{} '{}' has an empty kubernetes.serviceAccountName",
            kind,
            name
        );
    }
    if let Some(control_port) = runtime.control_port {
        ensure!(
            control_port > 0,
            "{} '{}' must set kubernetes.controlPort > 0",
            kind,
            name
        );
    }
    if runtime.workspace_host_path.is_some() || runtime.workspace_mount_path.is_some() {
        ensure!(
            runtime
                .workspace_host_path
                .as_deref()
                .map(str::trim)
                .is_some_and(|value| !value.is_empty()),
            "{} '{}' must set both kubernetes.workspaceHostPath and kubernetes.workspaceMountPath together",
            kind,
            name
        );
        ensure!(
            runtime
                .workspace_mount_path
                .as_deref()
                .map(str::trim)
                .is_some_and(|value| !value.is_empty()),
            "{} '{}' must set both kubernetes.workspaceHostPath and kubernetes.workspaceMountPath together",
            kind,
            name
        );
    }
    for mount in &runtime.host_path_mounts {
        ensure!(
            !mount.host_path.trim().is_empty(),
            "{} '{}' has a kubernetes.hostPathMounts entry with an empty hostPath",
            kind,
            name
        );
        ensure!(
            !mount.mount_path.trim().is_empty(),
            "{} '{}' has a kubernetes.hostPathMounts entry with an empty mountPath",
            kind,
            name
        );
    }
    for (key, value) in &runtime.env {
        ensure!(
            !key.trim().is_empty(),
            "{} '{}' has an empty kubernetes.env key",
            kind,
            name
        );
        ensure!(
            !value.trim().is_empty(),
            "{} '{}' has an empty kubernetes.env value for '{}'",
            kind,
            name,
            key
        );
    }
    Ok(())
}

fn render_kubernetes_yaml_documents(manifests: &[serde_json::Value]) -> anyhow::Result<String> {
    let mut rendered = Vec::new();
    for manifest in manifests {
        rendered
            .push(serde_yaml::to_string(manifest).context("failed to encode Kubernetes manifest")?);
    }
    Ok(rendered.join("---\n"))
}

fn kubectl_apply_rendered_documents(
    manifests: &[serde_json::Value],
    kubectl_context: Option<&str>,
    dry_run_mode: Option<&str>,
) -> anyhow::Result<Option<String>> {
    if manifests.is_empty() {
        return Ok(None);
    }
    let rendered = render_kubernetes_yaml_documents(manifests)?;
    let mut command = ProcessCommand::new("kubectl");
    if let Some(context) = kubectl_context
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        command.arg("--context").arg(context);
    }
    command.arg("apply").arg("-f").arg("-");
    if let Some(mode) = dry_run_mode {
        command.arg(format!("--dry-run={mode}"));
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn kubectl apply")?;
    use std::io::Write as _;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("failed to open kubectl stdin"))?
        .write_all(rendered.as_bytes())
        .context("failed to stream rendered manifests to kubectl")?;
    let output = child
        .wait_with_output()
        .context("failed while waiting for kubectl apply")?;
    if !output.status.success() {
        bail!(
            "kubectl apply failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut chunks = Vec::new();
    let stdout = String::from_utf8(output.stdout).context("kubectl returned non-utf8 stdout")?;
    let stderr = String::from_utf8(output.stderr).context("kubectl returned non-utf8 stderr")?;
    if !stdout.trim().is_empty() {
        chunks.push(stdout.trim().to_string());
    }
    if !stderr.trim().is_empty() {
        chunks.push(stderr.trim().to_string());
    }
    Ok((!chunks.is_empty()).then(|| chunks.join("\n")))
}

fn kubernetes_namespace_exists(kubectl_context: Option<&str>, namespace: &str) -> bool {
    let mut command = ProcessCommand::new("kubectl");
    if let Some(context) = kubectl_context
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        command.arg("--context").arg(context);
    }
    command
        .args(["get", "namespace", namespace, "-o", "name"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn kubernetes_metadata_value(
    metadata: &ResourceMetadata,
    cluster_scoped: bool,
    extra_labels: BTreeMap<String, String>,
    extra_annotations: BTreeMap<String, String>,
    explicit_name: Option<String>,
) -> serde_json::Value {
    let mut labels = metadata.labels.clone();
    labels.extend(extra_labels);
    let mut annotations = metadata.annotations.clone();
    annotations.extend(extra_annotations);
    let mut value = serde_json::Map::new();
    value.insert(
        "name".to_string(),
        json!(explicit_name.unwrap_or_else(|| metadata.name.clone())),
    );
    if !cluster_scoped {
        value.insert(
            "namespace".to_string(),
            json!(normalize_namespaced_resource_namespace(
                metadata.namespace.as_deref()
            )),
        );
    }
    if !labels.is_empty() {
        value.insert("labels".to_string(), json!(labels));
    }
    if !annotations.is_empty() {
        value.insert("annotations".to_string(), json!(annotations));
    }
    serde_json::Value::Object(value)
}

fn kubernetes_namespace_value(manifest: &ResourceEnvelope<NamespaceSpec>) -> serde_json::Value {
    json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": kubernetes_metadata_value(
            &manifest.metadata,
            true,
            BTreeMap::from([("app.kubernetes.io/managed-by".to_string(), "jarvisctl".to_string())]),
            BTreeMap::new(),
            None,
        ),
    })
}

fn kubernetes_config_map_value(manifest: &ResourceEnvelope<ConfigMapSpec>) -> serde_json::Value {
    json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": kubernetes_metadata_value(
            &manifest.metadata,
            false,
            BTreeMap::from([("app.kubernetes.io/managed-by".to_string(), "jarvisctl".to_string())]),
            BTreeMap::new(),
            None,
        ),
        "data": manifest.spec.data,
    })
}

fn kubernetes_secret_value(manifest: &ResourceEnvelope<SecretSpec>) -> serde_json::Value {
    json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "type": "Opaque",
        "metadata": kubernetes_metadata_value(
            &manifest.metadata,
            false,
            BTreeMap::from([("app.kubernetes.io/managed-by".to_string(), "jarvisctl".to_string())]),
            BTreeMap::new(),
            None,
        ),
        "stringData": manifest.spec.string_data,
    })
}

fn kubernetes_network_policy_value(
    manifest: &ResourceEnvelope<NetworkPolicySpec>,
) -> serde_json::Value {
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": kubernetes_metadata_value(
            &manifest.metadata,
            false,
            BTreeMap::from([("app.kubernetes.io/managed-by".to_string(), "jarvisctl".to_string())]),
            BTreeMap::new(),
            None,
        ),
        "spec": {
            "podSelector": manifest.spec.pod_selector,
            "policyTypes": effective_network_policy_types(&manifest.spec),
            "ingress": manifest.spec.ingress,
            "egress": manifest.spec.egress,
        }
    })
}

fn kubernetes_deployment_values(
    manifest: &ResourceEnvelope<DeploymentSpec>,
    sources: &[ResourceManifest],
) -> anyhow::Result<Option<Vec<serde_json::Value>>> {
    let Some(_) = manifest.spec.template.kubernetes.as_ref() else {
        return Ok(None);
    };
    ensure!(
        manifest.spec.agents == 1,
        "Deployment '{}/{}' currently compiles to Kubernetes only with spec.agents = 1",
        manifest.namespace_key(),
        manifest.metadata.name
    );
    ensure!(
        manifest.spec.replicas <= 1,
        "Deployment '{}/{}' currently compiles to Kubernetes only with spec.replicas <= 1",
        manifest.namespace_key(),
        manifest.metadata.name
    );
    ensure!(
        manifest
            .spec
            .driver
            .unwrap_or(CodexRuntimeDriver::AppServer)
            == CodexRuntimeDriver::AppServer,
        "Deployment '{}/{}' currently compiles to Kubernetes only with the app_server driver",
        manifest.namespace_key(),
        manifest.metadata.name
    );

    let namespace_defaults = namespace_defaults_from_sources(manifest.namespace_key(), sources);
    let template = resolved_deployment_template(manifest, &namespace_defaults);
    let runtime = template
        .kubernetes
        .as_ref()
        .ok_or_else(|| anyhow!("deployment template lost kubernetes runtime during resolution"))?;
    let working_directory = template
        .working_directory
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "Deployment '{}/{}' needs spec.template.working_directory or Namespace default_working_directory for Kubernetes runtime rendering",
                manifest.namespace_key(),
                manifest.metadata.name
            )
        })?;
    if let Some(workspace_mount_path) = runtime.workspace_mount_path.as_deref() {
        ensure!(
            workspace_mount_path == working_directory,
            "Deployment '{}/{}' must keep kubernetes.workspaceMountPath equal to spec.template.working_directory for the current hostPath-based Kubernetes runtime proof",
            manifest.namespace_key(),
            manifest.metadata.name
        );
    }
    if let Some(workspace_host_path) = runtime.workspace_host_path.as_deref() {
        ensure!(
            workspace_host_path == working_directory,
            "Deployment '{}/{}' must keep kubernetes.workspaceHostPath equal to spec.template.working_directory for the current hostPath-based Kubernetes runtime proof",
            manifest.namespace_key(),
            manifest.metadata.name
        );
    }

    let control_namespace = manifest.namespace_key().to_string();
    let control_port = runtime
        .control_port
        .unwrap_or(DEFAULT_KUBERNETES_RUNTIME_CONTROL_PORT);
    let pod_labels = kubernetes_runtime_pod_labels(manifest);
    let deployment_metadata_labels = pod_labels.clone();
    let launch_config_map_name =
        format!("{}-codex-launch", slugify(&manifest.metadata.name)).to_string();

    let prepared = prepare_codex_ticket_launch(&CodexLaunchOptions {
        backend: SessionBackend::Native,
        driver: CodexRuntimeDriver::AppServer,
        task_note: PathBuf::from(&template.task_note),
        namespace: Some(manifest.metadata.name.clone()),
        agents: manifest.spec.agents,
        agent: "agent0".to_string(),
        fresh_session: true,
        resume_session_id: None,
        working_directory: Some(PathBuf::from(working_directory)),
        prompt_file: None,
        operator_message: template.operator_message.clone(),
        images: template.images.iter().map(PathBuf::from).collect(),
        environment: runtime.env.clone(),
        context_overlay: RuntimeContextMetadata {
            control_namespace: Some(control_namespace.clone()),
            deployment: Some(manifest.metadata.name.clone()),
            labels: pod_labels.clone(),
            config_maps: template
                .config_maps
                .iter()
                .map(|reference| env_binding_name(reference).to_string())
                .collect(),
            secrets: template
                .secrets
                .iter()
                .map(|reference| env_binding_name(reference).to_string())
                .collect(),
            volumes: template
                .volumes
                .iter()
                .map(|reference| volume_binding_name(reference).to_string())
                .collect(),
            ..RuntimeContextMetadata::default()
        },
        extra_runtime_args: Vec::new(),
        startup_delay_ms: manifest.spec.startup_delay_ms.unwrap_or(1500),
        command: template.command.clone(),
    })?;
    let launch_manifest = codex_app_manifest_from_prepared(&prepared);
    let launch_manifest_raw = serde_json::to_string_pretty(&launch_manifest)
        .context("failed to encode Kubernetes Codex launch manifest")?;

    let (extra_volumes, extra_volume_mounts) =
        kubernetes_runtime_host_path_volumes(&template, runtime, sources, &control_namespace)?;
    let env_from = kubernetes_runtime_env_from(&template);
    let image = runtime
        .image
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(DEFAULT_KUBERNETES_RUNTIME_IMAGE);
    let image_pull_policy = runtime
        .image_pull_policy
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("IfNotPresent");
    let home_dir = runtime
        .env
        .get("HOME")
        .cloned()
        .unwrap_or_else(|| "/home/rootster".to_string());
    let path_value = runtime
        .env
        .get("PATH")
        .cloned()
        .unwrap_or_else(|| format!("{home_dir}/.local/bin:/usr/local/bin:/usr/bin:/bin"));
    let mut container_env = vec![
        json!({"name": "HOME", "value": home_dir}),
        json!({"name": "PATH", "value": path_value}),
        json!({"name": "JARVISCTL_CODEX_APP_TCP_HOST", "value": "0.0.0.0"}),
        json!({"name": "JARVISCTL_CODEX_APP_TCP_PORT", "value": control_port.to_string()}),
        json!({"name": "JARVIS_KUBERNETES_RUNTIME", "value": "true"}),
    ];
    for (key, value) in &runtime.env {
        if key == "HOME" || key == "PATH" {
            continue;
        }
        container_env.push(json!({"name": key, "value": value}));
    }

    let mut volumes = vec![json!({
        "name": "codex-launch-manifest",
        "configMap": {
            "name": launch_config_map_name,
            "items": [{
                "key": "launch-manifest.json",
                "path": "launch-manifest.json",
            }]
        }
    })];
    volumes.extend(extra_volumes);

    let mut volume_mounts = vec![json!({
        "name": "codex-launch-manifest",
        "mountPath": "/etc/jarvisctl/launch-manifest.json",
        "subPath": "launch-manifest.json",
        "readOnly": true,
    })];
    volume_mounts.extend(extra_volume_mounts);

    let mut container = json!({
        "name": "codex-runtime",
        "image": image,
        "imagePullPolicy": image_pull_policy,
        "command": ["/bin/sh", "-lc", "exec jarvisctl codex-app-session-serve --manifest /etc/jarvisctl/launch-manifest.json"],
        "env": container_env,
        "ports": [{
            "name": "control",
            "containerPort": control_port,
            "protocol": "TCP",
        }],
        "volumeMounts": volume_mounts,
        "workingDir": working_directory,
        "startupProbe": {
            "tcpSocket": {
                "port": control_port,
            },
            "periodSeconds": 2,
            "timeoutSeconds": 1,
            "failureThreshold": 60,
        },
        "readinessProbe": {
            "tcpSocket": {
                "port": control_port,
            },
            "periodSeconds": 2,
            "timeoutSeconds": 1,
            "failureThreshold": 15,
        },
    });
    if !env_from.is_empty() {
        container["envFrom"] = json!(env_from);
    }

    let mut pod_spec = json!({
        "restartPolicy": "Always",
        "containers": [container],
        "volumes": volumes,
    });
    if let Some(service_account_name) = runtime
        .service_account_name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        pod_spec["serviceAccountName"] = json!(service_account_name);
    }

    Ok(Some(vec![
        json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": kubernetes_metadata_value(
                &manifest.metadata,
                false,
                BTreeMap::from([
                    ("app.kubernetes.io/managed-by".to_string(), "jarvisctl".to_string()),
                    ("jarvisctl.io/resource-kind".to_string(), "CodexLaunchManifest".to_string()),
                    ("jarvisctl.io/runtime-namespace".to_string(), manifest.metadata.name.clone()),
                ]),
                BTreeMap::from([
                    ("jarvisctl.io/source-kind".to_string(), "Deployment".to_string()),
                    ("jarvisctl.io/source-name".to_string(), manifest.metadata.name.clone()),
                ]),
                Some(launch_config_map_name.clone()),
            ),
            "data": {
                "launch-manifest.json": launch_manifest_raw,
            }
        }),
        json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": kubernetes_metadata_value(
                &manifest.metadata,
                false,
                BTreeMap::from([
                    ("app.kubernetes.io/managed-by".to_string(), "jarvisctl".to_string()),
                    ("jarvisctl.io/runtime-driver".to_string(), "codex-app".to_string()),
                    ("jarvisctl.io/runtime-namespace".to_string(), manifest.metadata.name.clone()),
                ]),
                BTreeMap::new(),
                None,
            ),
            "spec": {
                "replicas": manifest.spec.replicas,
                "selector": {
                    "matchLabels": pod_labels,
                },
                "template": {
                    "metadata": {
                        "labels": deployment_metadata_labels,
                    },
                    "spec": pod_spec,
                }
            }
        }),
    ]))
}

fn kubernetes_runtime_service_value(
    manifest: &ResourceEnvelope<ServiceSpec>,
    sources: &[ResourceManifest],
) -> anyhow::Result<Option<serde_json::Value>> {
    let targets = kubernetes_runtime_service_targets(manifest, sources)?;
    let Some(target) = targets.first() else {
        return Ok(None);
    };
    ensure!(
        targets.len() == 1,
        "runtime Service '{}/{}' matched multiple runtime deployments: {:?}",
        manifest.namespace_key(),
        manifest.metadata.name,
        targets
            .iter()
            .map(|deployment| deployment.metadata.name.as_str())
            .collect::<Vec<_>>()
    );
    let control_port = target
        .spec
        .template
        .kubernetes
        .as_ref()
        .and_then(|runtime| runtime.control_port)
        .unwrap_or(DEFAULT_KUBERNETES_RUNTIME_CONTROL_PORT);
    Ok(Some(json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": kubernetes_metadata_value(
            &manifest.metadata,
            false,
            BTreeMap::from([
                ("app.kubernetes.io/managed-by".to_string(), "jarvisctl".to_string()),
                (
                    "jarvisctl.io/runtime-deployment".to_string(),
                    target.metadata.name.clone(),
                ),
            ]),
            BTreeMap::from([
                ("jarvisctl.io/source-kind".to_string(), "Service".to_string()),
                ("jarvisctl.io/source-name".to_string(), manifest.metadata.name.clone()),
            ]),
            None,
        ),
        "spec": {
            "selector": manifest.spec.selector,
            "ports": [{
                "name": "control",
                "port": control_port,
                "targetPort": control_port,
                "protocol": "TCP",
            }]
        }
    })))
}

fn kubernetes_runtime_service_targets<'a>(
    manifest: &ResourceEnvelope<ServiceSpec>,
    sources: &'a [ResourceManifest],
) -> anyhow::Result<Vec<&'a ResourceEnvelope<DeploymentSpec>>> {
    let mut matched_targets = Vec::new();
    for source in sources {
        let ResourceManifest::Deployment(deployment) = source else {
            continue;
        };
        if deployment.namespace_key() != manifest.namespace_key() {
            continue;
        }
        let Some(_) = deployment.spec.template.kubernetes.as_ref() else {
            continue;
        };
        if !selector_matches_labels(
            &manifest.spec.selector,
            &kubernetes_runtime_pod_labels(deployment),
        ) {
            continue;
        }
        matched_targets.push(deployment);
    }
    Ok(matched_targets)
}

fn selector_matches_labels(
    selector: &BTreeMap<String, String>,
    labels: &BTreeMap<String, String>,
) -> bool {
    selector
        .iter()
        .all(|(key, value)| labels.get(key) == Some(value))
}

fn kubernetes_runtime_pod_labels(
    manifest: &ResourceEnvelope<DeploymentSpec>,
) -> BTreeMap<String, String> {
    let mut labels = manifest.metadata.labels.clone();
    labels.extend(manifest.spec.template.labels.clone());
    labels.insert(
        "app.kubernetes.io/name".to_string(),
        manifest.metadata.name.clone(),
    );
    labels.insert(
        "jarvisctl.io/runtime-driver".to_string(),
        "codex-app".to_string(),
    );
    labels.insert(
        "jarvisctl.io/runtime-namespace".to_string(),
        manifest.metadata.name.clone(),
    );
    labels.insert(
        "jarvisctl.io/control-namespace".to_string(),
        manifest.namespace_key().to_string(),
    );
    labels
}

fn namespace_defaults_from_sources(
    control_namespace: &str,
    sources: &[ResourceManifest],
) -> NamespaceSpec {
    sources
        .iter()
        .find_map(|manifest| match manifest {
            ResourceManifest::Namespace(namespace)
                if namespace.metadata.name == control_namespace =>
            {
                Some(namespace.spec.clone())
            }
            _ => None,
        })
        .unwrap_or_default()
}

fn kubernetes_runtime_env_from(template: &DeploymentTemplateSpec) -> Vec<serde_json::Value> {
    let mut env_from = Vec::new();
    for reference in &template.config_maps {
        let mut entry = json!({
            "configMapRef": {
                "name": env_binding_name(reference),
                "optional": env_binding_optional(reference),
            }
        });
        if let Some(prefix) = env_binding_prefix(reference) {
            entry["prefix"] = json!(prefix);
        }
        env_from.push(entry);
    }
    for reference in &template.secrets {
        let mut entry = json!({
            "secretRef": {
                "name": env_binding_name(reference),
                "optional": env_binding_optional(reference),
            }
        });
        if let Some(prefix) = env_binding_prefix(reference) {
            entry["prefix"] = json!(prefix);
        }
        env_from.push(entry);
    }
    env_from
}

fn kubernetes_runtime_host_path_volumes(
    template: &DeploymentTemplateSpec,
    runtime: &KubernetesRuntimeSpec,
    sources: &[ResourceManifest],
    control_namespace: &str,
) -> anyhow::Result<(Vec<serde_json::Value>, Vec<serde_json::Value>)> {
    let mut mounts = Vec::new();
    if let (Some(workspace_host_path), Some(workspace_mount_path)) = (
        runtime.workspace_host_path.as_deref(),
        runtime.workspace_mount_path.as_deref(),
    ) {
        mounts.push(KubernetesHostPathMount {
            host_path: workspace_host_path.to_string(),
            mount_path: workspace_mount_path.to_string(),
            read_only: false,
        });
    }
    mounts.extend(runtime.host_path_mounts.clone());
    mounts.extend(resolved_kubernetes_volume_bindings(
        &template.volumes,
        control_namespace,
        sources,
    )?);

    let mut volumes = Vec::new();
    let mut volume_mounts = Vec::new();
    for (index, mount) in mounts.iter().enumerate() {
        let volume_name = format!("host-path-{}", index + 1);
        volumes.push(json!({
            "name": volume_name,
            "hostPath": {
                "path": mount.host_path,
                "type": "DirectoryOrCreate",
            }
        }));
        volume_mounts.push(json!({
            "name": volume_name,
            "mountPath": mount.mount_path,
            "readOnly": mount.read_only,
        }));
    }
    Ok((volumes, volume_mounts))
}

fn resolved_kubernetes_volume_bindings(
    references: &[VolumeBindingRef],
    control_namespace: &str,
    sources: &[ResourceManifest],
) -> anyhow::Result<Vec<KubernetesHostPathMount>> {
    let mut mounts = Vec::new();
    for reference in references {
        let name = volume_binding_name(reference);
        let manifest = sources.iter().find_map(|manifest| match manifest {
            ResourceManifest::Volume(volume)
                if volume.namespace_key() == control_namespace && volume.metadata.name == name =>
            {
                Some(volume)
            }
            _ => None,
        });
        let Some(volume) = manifest else {
            if volume_binding_optional(reference) {
                continue;
            }
            bail!(
                "missing Volume '{}/{}' while compiling Kubernetes runtime Deployment",
                control_namespace,
                name
            );
        };
        let selected_paths = if volume_binding_paths(reference).is_empty() {
            volume.spec.paths.clone()
        } else {
            volume
                .spec
                .paths
                .iter()
                .filter(|path| volume_binding_paths(reference).contains(path))
                .cloned()
                .collect::<Vec<_>>()
        };
        for path in selected_paths {
            mounts.push(KubernetesHostPathMount {
                host_path: path.clone(),
                mount_path: path,
                read_only: false,
            });
        }
    }
    Ok(mounts)
}
