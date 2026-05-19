use super::*;

pub fn render_get_output(
    kind_arg: ControlPlaneResourceKindArg,
    namespace: Option<&str>,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    let _ = reconcile_control_plane()?;
    if kind_arg == ControlPlaneResourceKindArg::Worker {
        let summaries = list_worker_summaries(namespace)?;
        return match output {
            ControlPlaneOutput::Json => serde_json::to_string_pretty(&summaries)
                .context("failed to encode worker summaries"),
            ControlPlaneOutput::Yaml => {
                serde_yaml::to_string(&summaries).context("failed to encode worker summaries")
            }
            ControlPlaneOutput::Table => Ok(render_summary_table(&summaries)),
        };
    }
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
    if kind_arg == ControlPlaneResourceKindArg::Worker {
        let envelope = worker_describe_envelope(name, namespace)?;
        return match output {
            ControlPlaneOutput::Json => {
                serde_json::to_string_pretty(&envelope).context("failed to encode worker payload")
            }
            ControlPlaneOutput::Yaml | ControlPlaneOutput::Table => {
                serde_yaml::to_string(&envelope).context("failed to encode worker payload")
            }
        };
    }
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

pub(crate) fn load_rollout_status(
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

pub(crate) fn list_resource_summaries(
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

pub(crate) fn list_worker_summaries(
    namespace: Option<&str>,
) -> anyhow::Result<Vec<ResourceSummary>> {
    let mut summaries = Vec::new();
    for manifest in load_manifests_by_kind(ResourceKind::Worker, namespace)? {
        summaries.push(resource_summary(&manifest)?);
    }
    for manifest in load_manifests_by_kind(ResourceKind::Service, namespace)? {
        let ResourceManifest::Service(service) = manifest else {
            continue;
        };
        if effective_service_target_kind(&service.spec) != ServiceTargetKind::Runtime {
            continue;
        }
        let status = service_status(&service)?;
        summaries.push(ResourceSummary {
            kind: "Worker".to_string(),
            namespace: service.metadata.namespace.clone(),
            name: service.metadata.name.clone(),
            status: "codex (runtime-offload)".to_string(),
            detail: if status.endpoints.is_empty() {
                "no resolved endpoints".to_string()
            } else {
                format!(
                    "{} endpoint(s): {}",
                    status.endpoints.len(),
                    status.endpoints.join(", ")
                )
            },
        });
    }
    summaries.sort_by(|left, right| {
        left.namespace
            .cmp(&right.namespace)
            .then_with(|| left.name.cmp(&right.name))
    });
    if env::var_os("JARVIS_WORKER_INDEX_LOCAL_ONLY").is_none() {
        let timeout_seconds = load_or_create_orchestration_policy()
            .map(|policy| policy.remote_index_timeout_seconds)
            .unwrap_or(5);
        summaries.extend(collect_remote_worker_summaries(timeout_seconds)?);
        summaries.sort_by(|left, right| {
            left.namespace
                .cmp(&right.namespace)
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.detail.cmp(&right.detail))
        });
    }
    Ok(summaries)
}

fn collect_remote_worker_summaries(timeout_seconds: u64) -> anyhow::Result<Vec<ResourceSummary>> {
    let mut summaries = Vec::new();
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
                &format!("{}s", timeout_seconds.max(1)),
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                &target,
                "JARVIS_WORKER_INDEX_LOCAL_ONLY=1 jarvisctl get workers --output json",
            ])
            .output();
        let Ok(output) = output else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let Ok(mut remote) = serde_json::from_slice::<Vec<ResourceSummary>>(&output.stdout) else {
            continue;
        };
        for summary in &mut remote {
            summary.detail = if summary.detail.trim().is_empty() {
                format!("node {}", node.metadata.name)
            } else {
                format!("{} · node {}", summary.detail, node.metadata.name)
            };
        }
        summaries.extend(remote);
    }
    Ok(summaries)
}

pub(crate) fn worker_describe_envelope(
    name: &str,
    namespace: Option<&str>,
) -> anyhow::Result<WorkerDescribeEnvelope> {
    let namespace = normalize_namespaced_resource_namespace(namespace);
    if let Ok(ResourceManifest::Worker(worker)) =
        load_manifest(ResourceKind::Worker, name, Some(&namespace))
    {
        return Ok(WorkerDescribeEnvelope {
            manifest: serde_json::to_value(&worker).context("failed to encode Worker manifest")?,
            status: worker_status(&worker),
        });
    }
    let manifest = load_manifest(ResourceKind::Service, name, Some(&namespace))?;
    let ResourceManifest::Service(service) = manifest else {
        bail!(
            "resource '{}/{}' is not a Service-backed worker lane",
            namespace,
            name
        );
    };
    ensure!(
        effective_service_target_kind(&service.spec) == ServiceTargetKind::Runtime,
        "service '{}/{}' is not a runtime worker lane",
        namespace,
        name
    );
    let status = service_status(&service)?;
    let loaded = !status.endpoints.is_empty();
    let admission_code = if loaded { "ready" } else { "no_endpoints" }.to_string();
    let admission = if loaded { "ready" } else { "blocked" }.to_string();
    let admission_reason = if loaded {
        format!("{} resolved runtime endpoint(s)", status.endpoints.len())
    } else {
        "service has no ready runtime endpoints".to_string()
    };
    Ok(WorkerDescribeEnvelope {
        manifest: json!({
            "apiVersion": API_VERSION,
            "kind": "Worker",
            "metadata": {
                "name": service.metadata.name,
                "namespace": service.metadata.namespace,
                "labels": service.metadata.labels,
                "annotations": service.metadata.annotations,
            },
            "spec": {
                "provider": "codex",
                "model": "codex",
                "role": "runtime-offload",
                "outputMode": "text",
            },
        }),
        status: WorkerStatus {
            endpoint: format!("service://{}/{}", namespace, name),
            loaded,
            locality: "cluster".to_string(),
            model: "codex".to_string(),
            output_mode: "text".to_string(),
            provider: "codex".to_string(),
            role: "runtime-offload".to_string(),
            capabilities: vec!["conversation".to_string(), "offload".to_string()],
            classes: vec!["runtime".to_string(), "codex".to_string()],
            pool: Some(namespace.clone()),
            max_concurrent: status.endpoints.len().max(1),
            active_runs: 0,
            pending_runs: 0,
            available_slots: status.endpoints.len(),
            admission,
            admission_code,
            admission_reason,
            service_name: name.to_string(),
            service_namespace: namespace,
            endpoints: status.endpoints,
            allowed_intents: status.allowed_intents,
        },
    })
}

pub fn render_worker_validation_output(output: ControlPlaneOutput) -> anyhow::Result<String> {
    let report = validate_worker_lanes()?;
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(&report).context("failed to encode worker validation")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(&report).context("failed to encode worker validation")
        }
        ControlPlaneOutput::Table => Ok(format!(
            "STATUS\tWORKERS\tREADY\tDETAIL\n{}\t{}\t{}\t{}",
            report.status, report.workers, report.ready_workers, report.detail
        )),
    }
}

pub(crate) fn resource_summary(manifest: &ResourceManifest) -> anyhow::Result<ResourceSummary> {
    match manifest {
        ResourceManifest::Node(node) => Ok(ResourceSummary {
            kind: "Node".to_string(),
            namespace: None,
            name: node.metadata.name.clone(),
            status: if node.spec.cordoned {
                "cordoned".to_string()
            } else {
                "schedulable".to_string()
            },
            detail: node_summary_detail(node),
        }),
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
        ResourceManifest::Service(service) => {
            let status = service_status(service)?;
            Ok(ResourceSummary {
                kind: "Service".to_string(),
                namespace: service.metadata.namespace.clone(),
                name: service.metadata.name.clone(),
                status: format!(
                    "{} {} endpoints",
                    status.target_kind,
                    status.endpoints.len()
                ),
                detail: if status.endpoints.is_empty() {
                    "no resolved endpoints".to_string()
                } else {
                    status.endpoints.join(", ")
                },
            })
        }
        ResourceManifest::Worker(worker) => {
            let loaded =
                !worker.spec.provider.trim().is_empty() && !worker.spec.model.trim().is_empty();
            Ok(ResourceSummary {
                kind: "Worker".to_string(),
                namespace: worker.metadata.namespace.clone(),
                name: worker.metadata.name.clone(),
                status: format!(
                    "{} ({})",
                    worker.spec.model,
                    if worker.spec.role.trim().is_empty() {
                        "worker"
                    } else {
                        worker.spec.role.as_str()
                    }
                ),
                detail: if loaded {
                    format!(
                        "{} · {} · pool {}",
                        worker.spec.provider,
                        worker.spec.locality.as_deref().unwrap_or("local"),
                        worker.spec.pool.as_deref().unwrap_or("default")
                    )
                } else {
                    "worker missing provider or model".to_string()
                },
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
            detail: if config_map.spec.access_policy.is_empty() {
                config_map
                    .spec
                    .data
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                format!(
                    "{} keys; scoped to {}",
                    config_map.spec.data.len(),
                    access_policy_summary(&config_map.spec.access_policy)
                )
            },
        }),
        ResourceManifest::Secret(secret) => Ok(ResourceSummary {
            kind: "Secret".to_string(),
            namespace: secret.metadata.namespace.clone(),
            name: secret.metadata.name.clone(),
            status: format!("{} keys", secret.spec.string_data.len()),
            detail: if secret.spec.access_policy.is_empty() {
                secret
                    .spec
                    .string_data
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                format!(
                    "{} keys; scoped to {}",
                    secret.spec.string_data.len(),
                    access_policy_summary(&secret.spec.access_policy)
                )
            },
        }),
        ResourceManifest::Volume(volume) => Ok(ResourceSummary {
            kind: "Volume".to_string(),
            namespace: volume.metadata.namespace.clone(),
            name: volume.metadata.name.clone(),
            status: format!("{} paths", volume.spec.paths.len()),
            detail: if volume.spec.access_policy.is_empty() {
                volume.spec.paths.join(", ")
            } else {
                format!(
                    "{} paths; scoped to {}",
                    volume.spec.paths.len(),
                    access_policy_summary(&volume.spec.access_policy)
                )
            },
        }),
    }
}

pub(crate) fn namespace_status(control_namespace: &str) -> anyhow::Result<NamespaceStatus> {
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

pub(crate) fn deployment_status(
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
    let events = deployment_events(manifest, target_replica_set, &conditions);
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
        config_maps: env_binding_statuses(&manifest.spec.template.config_maps),
        secrets: env_binding_statuses(&manifest.spec.template.secrets),
        volumes: volume_binding_statuses(&manifest.spec.template.volumes),
        replica_sets,
        sessions: active_sessions,
        conditions,
        events,
    })
}

pub(crate) fn deployment_rollout_history(
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

pub(crate) fn deployment_progress_deadline_exceeded(
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

pub(crate) fn deployment_conditions(
    manifest: &ResourceEnvelope<DeploymentSpec>,
    target_replica_set: Option<&ResourceEnvelope<ReplicaSetSpec>>,
    replica_sets: &[ReplicaSetStatus],
    updated_replicas: usize,
    ready_replicas: usize,
    available: bool,
    failed: bool,
) -> Vec<StatusCondition> {
    let now = now_epoch_ms();
    let current_replica_set_name = target_replica_set
        .map(|replica_set| replica_set.metadata.name.as_str())
        .unwrap_or("unknown");
    let mut conditions = Vec::new();
    conditions.push(StatusCondition {
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
        StatusCondition {
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
        StatusCondition {
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
        StatusCondition {
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
        StatusCondition {
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

pub(crate) fn status_event(
    event_type: impl Into<String>,
    reason: impl Into<String>,
    message: impl Into<String>,
    epoch_ms: u128,
    related: Option<String>,
) -> StatusEvent {
    StatusEvent {
        event_type: event_type.into(),
        reason: reason.into(),
        message: message.into(),
        epoch_ms,
        related,
    }
}

pub(crate) fn sort_status_events_desc(events: &mut Vec<StatusEvent>) {
    events.sort_by(|left, right| {
        right
            .epoch_ms
            .cmp(&left.epoch_ms)
            .then_with(|| left.event_type.cmp(&right.event_type))
    });
}

pub(crate) fn deployment_events(
    manifest: &ResourceEnvelope<DeploymentSpec>,
    target_replica_set: Option<&ResourceEnvelope<ReplicaSetSpec>>,
    conditions: &[StatusCondition],
) -> Vec<StatusEvent> {
    let mut events = conditions
        .iter()
        .map(|condition| {
            status_event(
                format!(
                    "deployment_{}",
                    condition.condition_type.to_ascii_lowercase()
                ),
                condition.reason.clone(),
                condition.message.clone(),
                condition.last_transition_epoch_ms,
                Some(manifest.metadata.name.clone()),
            )
        })
        .collect::<Vec<_>>();

    if let Some(replica_set) = target_replica_set {
        let created_at_epoch_ms = replica_set
            .metadata
            .annotations
            .get("jarvisctl.io/created-at-epoch-ms")
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or_else(now_epoch_ms);
        events.push(status_event(
            "deployment_rollout",
            "TargetReplicaSet",
            format!(
                "Deployment targets ReplicaSet '{}' at revision {}",
                replica_set.metadata.name, replica_set.spec.revision
            ),
            created_at_epoch_ms,
            Some(replica_set.metadata.name.clone()),
        ));
    }

    sort_status_events_desc(&mut events);
    events.truncate(16);
    events
}

pub(crate) fn replica_set_status(
    manifest: &ResourceEnvelope<ReplicaSetSpec>,
) -> anyhow::Result<ReplicaSetStatus> {
    let sessions = collect_runtime_sessions()?;
    replica_set_status_with_sessions(manifest, &sessions)
}

pub(crate) fn replica_set_status_with_sessions(
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
        config_maps: env_binding_statuses(&manifest.spec.template.config_maps),
        secrets: env_binding_statuses(&manifest.spec.template.secrets),
        volumes: volume_binding_statuses(&manifest.spec.template.volumes),
        sessions: desired_namespaces,
        active: manifest.spec.replicas > 0,
    })
}

pub(crate) fn render_rollout_status_table(
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

pub(crate) fn deployment_rollout_complete(status: &DeploymentStatus) -> bool {
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

pub(crate) fn deployment_rollout_failure_message(status: &DeploymentStatus) -> Option<String> {
    status
        .conditions
        .iter()
        .find(|condition| {
            condition.condition_type == "Progressing"
                && condition.reason == "ProgressDeadlineExceeded"
        })
        .map(|condition| condition.message.clone())
}

pub(crate) fn render_rollout_history_table(history: &[DeploymentRolloutHistoryEntry]) -> String {
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

pub(crate) fn service_status(
    manifest: &ResourceEnvelope<ServiceSpec>,
) -> anyhow::Result<ServiceStatus> {
    let mut sessions = collect_runtime_sessions()?;
    sessions.retain(|session| service_matches_session(manifest, session));
    sessions.sort_by(|left, right| left.namespace.cmp(&right.namespace));
    let mut endpoints = sessions
        .into_iter()
        .map(|session| session.namespace)
        .collect::<Vec<_>>();
    endpoints.sort();
    Ok(ServiceStatus {
        target_kind: match effective_service_target_kind(&manifest.spec) {
            ServiceTargetKind::Runtime => "runtime",
            ServiceTargetKind::Worker => "worker",
        }
        .to_string(),
        endpoints,
        strategy: manifest.spec.strategy.clone(),
        allowed_intents: manifest.spec.allowed_intents.clone(),
        access_policy: resource_access_policy_status(&manifest.spec.access_policy),
    })
}

pub(crate) fn worker_status(manifest: &ResourceEnvelope<WorkerSpec>) -> WorkerStatus {
    let provider = manifest.spec.provider.trim().to_string();
    let model = manifest.spec.model.trim().to_string();
    let loaded = !provider.is_empty() && !model.is_empty();
    let admission_code = if loaded { "ready" } else { "invalid" }.to_string();
    let admission = if loaded { "ready" } else { "blocked" }.to_string();
    let admission_reason = if loaded {
        "worker manifest has provider and model".to_string()
    } else {
        "worker manifest must define provider and model".to_string()
    };
    WorkerStatus {
        endpoint: format!(
            "worker://{}/{}",
            manifest.metadata.namespace.as_deref().unwrap_or("default"),
            manifest.metadata.name
        ),
        loaded,
        locality: manifest
            .spec
            .locality
            .clone()
            .unwrap_or_else(|| "local".to_string()),
        model: if model.is_empty() {
            "unknown".to_string()
        } else {
            model
        },
        output_mode: manifest
            .spec
            .output_mode
            .clone()
            .unwrap_or_else(|| "text".to_string()),
        provider: if provider.is_empty() {
            "unknown".to_string()
        } else {
            provider
        },
        role: if manifest.spec.role.trim().is_empty() {
            "worker".to_string()
        } else {
            manifest.spec.role.clone()
        },
        capabilities: manifest.spec.capabilities.clone(),
        classes: manifest.spec.classes.clone(),
        pool: manifest.spec.pool.clone(),
        max_concurrent: manifest.spec.max_concurrent.unwrap_or(1),
        active_runs: 0,
        pending_runs: 0,
        available_slots: manifest.spec.max_concurrent.unwrap_or(1),
        admission,
        admission_code,
        admission_reason,
        service_name: String::new(),
        service_namespace: manifest
            .metadata
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string()),
        endpoints: vec![manifest.metadata.name.clone()],
        allowed_intents: Vec::new(),
    }
}

pub(crate) fn network_policy_status(
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

pub(crate) fn describe_status(manifest: &ResourceManifest) -> anyhow::Result<serde_json::Value> {
    match manifest {
        ResourceManifest::Node(node) => Ok(serde_json::to_value(node_static_status(node))?),
        ResourceManifest::Namespace(namespace) => Ok(serde_json::to_value(namespace_status(
            &namespace.metadata.name,
        )?)?),
        ResourceManifest::Deployment(deployment) => {
            Ok(serde_json::to_value(deployment_status(deployment)?)?)
        }
        ResourceManifest::ReplicaSet(replica_set) => {
            Ok(serde_json::to_value(replica_set_status(replica_set)?)?)
        }
        ResourceManifest::Service(service) => Ok(serde_json::to_value(service_status(service)?)?),
        ResourceManifest::Worker(worker) => Ok(serde_json::to_value(worker_status(worker))?),
        ResourceManifest::NetworkPolicy(network_policy) => Ok(serde_json::to_value(
            network_policy_status(network_policy)?,
        )?),
        ResourceManifest::ConfigMap(config_map) => Ok(serde_json::to_value(ConfigMapStatus {
            entries: config_map.spec.data.len(),
            keys: config_map.spec.data.keys().cloned().collect::<Vec<_>>(),
            access_policy: resource_access_policy_status(&config_map.spec.access_policy),
        })?),
        ResourceManifest::Secret(secret) => Ok(serde_json::to_value(SecretStatus {
            keys: secret.spec.string_data.keys().cloned().collect::<Vec<_>>(),
            access_policy: resource_access_policy_status(&secret.spec.access_policy),
        })?),
        ResourceManifest::Volume(volume) => Ok(serde_json::to_value(VolumeStatus {
            paths: volume.spec.paths.clone(),
            access_policy: resource_access_policy_status(&volume.spec.access_policy),
        })?),
    }
}

fn node_summary_detail(node: &ResourceEnvelope<NodeSpec>) -> String {
    let mut parts = Vec::new();
    if !node.spec.roles.is_empty() {
        parts.push(format!("roles={}", node.spec.roles.join(",")));
    }
    if let Some(address) = node.spec.address.as_deref() {
        parts.push(format!("addr={address}"));
    }
    if let Some(ssh_host) = node.spec.ssh_host.as_deref() {
        parts.push(format!("ssh={ssh_host}"));
    }
    if let Some(max_sessions) = node.spec.max_sessions {
        parts.push(format!("max={max_sessions}"));
    }
    parts.join(" ")
}

fn node_static_status(node: &ResourceEnvelope<NodeSpec>) -> NodeStatus {
    NodeStatus {
        available: false,
        schedulable: !node.spec.cordoned && node.spec.taints.is_empty(),
        roles: node.spec.roles.clone(),
        address: node.spec.address.clone(),
        ssh_target: node_ssh_target(&node.spec),
        architecture: node.spec.capabilities.get("arch").cloned(),
        operating_system: node.spec.capabilities.get("os").cloned(),
        codex: node.spec.capabilities.get("codex").cloned(),
        codex_auth: node.spec.capabilities.get("codex_auth").cloned(),
        jarvisctl: node.spec.capabilities.get("jarvisctl").cloned(),
        message: "static manifest only; run `jarvisctl node ping` for a live probe".to_string(),
    }
}

pub(crate) fn render_summary_table(summaries: &[ResourceSummary]) -> String {
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
