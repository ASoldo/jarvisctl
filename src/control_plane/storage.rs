use super::*;

pub(crate) fn save_manifest(manifest: &ResourceManifest) -> anyhow::Result<()> {
    let path = manifest_path(manifest.kind(), manifest.name(), manifest.namespace())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    let raw = serde_yaml::to_string(manifest).context("failed to encode manifest")?;
    atomic_write_string(&path, &raw)
}

pub(crate) fn atomic_write_string(path: &Path, raw: &str) -> anyhow::Result<()> {
    atomic_write_bytes(path, raw.as_bytes())
}

pub(crate) fn atomic_write_bytes(path: &Path, raw: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path '{}' has no parent", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("path '{}' has no file name", path.display()))?;
    let timestamp = Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let temp_candidates = [
        parent.join(format!(
            ".{}.tmp.{}.{}",
            file_name,
            std::process::id(),
            timestamp
        )),
        parent.join(format!(
            "{}.tmp.{}.{}",
            file_name,
            std::process::id(),
            timestamp
        )),
    ];
    let mut last_error = None;

    for temp_path in &temp_candidates {
        match fs::write(temp_path, raw) {
            Ok(()) => {
                return fs::rename(temp_path, path).with_context(|| {
                    format!(
                        "failed to rename '{}' to '{}'",
                        temp_path.display(),
                        path.display()
                    )
                });
            }
            Err(error) => {
                last_error = Some(anyhow!(
                    "failed to write '{}': {}",
                    temp_path.display(),
                    error
                ));
            }
        }
    }

    fs::write(path, raw).with_context(|| {
        if let Some(error) = &last_error {
            format!(
                "failed to write '{}' after temp-file fallbacks ({error})",
                path.display()
            )
        } else {
            format!("failed to write '{}'", path.display())
        }
    })
}

pub(crate) fn load_all_manifests(namespace: Option<&str>) -> anyhow::Result<Vec<ResourceManifest>> {
    let mut manifests = Vec::new();
    for kind in [
        ResourceKind::Node,
        ResourceKind::Namespace,
        ResourceKind::Deployment,
        ResourceKind::ReplicaSet,
        ResourceKind::Service,
        ResourceKind::Worker,
        ResourceKind::NetworkPolicy,
        ResourceKind::ConfigMap,
        ResourceKind::Secret,
        ResourceKind::Volume,
    ] {
        manifests.extend(load_manifests_by_kind(kind, namespace)?);
    }
    Ok(manifests)
}

pub(crate) fn load_manifests_by_kind(
    kind: ResourceKind,
    namespace: Option<&str>,
) -> anyhow::Result<Vec<ResourceManifest>> {
    let root = control_plane_root()?;
    let mut manifests = Vec::new();
    match kind {
        ResourceKind::Node | ResourceKind::Namespace => {
            let dir = root.join("namespaces");
            let dir = if kind == ResourceKind::Node {
                root.join("nodes")
            } else {
                dir
            };
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

pub(crate) fn load_manifest(
    kind: ResourceKind,
    name: &str,
    namespace: Option<&str>,
) -> anyhow::Result<ResourceManifest> {
    let path = manifest_path(kind, name, namespace)?;
    load_manifest_from_path(&path)
}

pub(crate) fn load_manifest_from_path(path: &Path) -> anyhow::Result<ResourceManifest> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    parse_manifest_documents(&raw)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("manifest '{}' is empty", path.display()))
}

pub(crate) fn manifest_path(
    kind: ResourceKind,
    name: &str,
    namespace: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let root = control_plane_root()?;
    let filename = format!("{}.yaml", slugify(name));
    let path = match kind {
        ResourceKind::Node => root.join("nodes").join(filename),
        ResourceKind::Namespace => root.join("namespaces").join(filename),
        _ => root
            .join(kind.directory_name())
            .join(normalize_namespaced_resource_namespace(namespace))
            .join(filename),
    };
    Ok(path)
}

pub(crate) fn control_plane_root() -> anyhow::Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".jarvis").join("control-plane"))
}

pub(crate) fn service_route_state_path(
    control_namespace: &str,
    service_name: &str,
) -> anyhow::Result<PathBuf> {
    Ok(control_plane_root()?
        .join("state")
        .join("services")
        .join(control_namespace)
        .join(format!("{}.json", slugify(service_name))))
}

pub(crate) fn load_service_route_state(
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

pub(crate) fn save_service_route_state(
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
    atomic_write_string(&path, &raw)
}
