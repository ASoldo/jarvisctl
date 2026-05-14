use crate::codex::enrich_native_sessions;
use crate::codex_app::{
    CodexAppInputMode, attach_codex_app, cleanup_stale_session, codex_app_session_dir_exists,
    codex_app_session_metadata, collect_codex_app_sessions, delete_codex_app_session,
    interrupt_codex_app, tell_codex_app, tell_codex_app_with_mode,
};
use crate::native::{
    NativeSessionMetadata, attach_native, collect_native_sessions, delete_native_session,
    interrupt_native, native_session_metadata, tell_native,
};
use anyhow::{Result, anyhow};
use std::thread;
use std::time::Duration;
use sysinfo::{Pid, System};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RuntimeSessionState {
    Missing,
    ActiveWork,
    Idle,
}

pub fn collect_runtime_sessions() -> Result<Vec<NativeSessionMetadata>> {
    let mut sessions = collect_native_sessions()?;
    sessions.extend(collect_codex_app_sessions()?);
    enrich_native_sessions(&mut sessions)?;
    sessions.sort_by(|left, right| left.namespace.cmp(&right.namespace));
    Ok(sessions)
}

pub fn session_metadata_for_namespace(namespace: &str) -> Result<NativeSessionMetadata> {
    match maybe_session_metadata_for_namespace(namespace)? {
        Some(session) => Ok(session),
        None => Err(anyhow!("runtime session '{}' does not exist", namespace)),
    }
}

pub fn probe_runtime_session_state(namespace: &str) -> Result<RuntimeSessionState> {
    let Some(metadata) = maybe_session_metadata_for_namespace(namespace)? else {
        return Ok(RuntimeSessionState::Missing);
    };

    if metadata.backend == "codex-app" {
        let turn_active = metadata
            .context
            .as_ref()
            .and_then(|context| context.turn_status.as_deref())
            == Some("inProgress");
        return Ok(if turn_active {
            RuntimeSessionState::ActiveWork
        } else {
            RuntimeSessionState::Idle
        });
    }

    let mut system = System::new_all();
    system.refresh_all();
    let has_active_codex = metadata.agents.iter().any(|agent| {
        agent.running
            && process_tree_has_codex(
                &system,
                usize::try_from(agent.pid)
                    .ok()
                    .map(Pid::from)
                    .unwrap_or_else(|| Pid::from(0)),
            )
    });
    if has_active_codex {
        return Ok(RuntimeSessionState::ActiveWork);
    }

    if metadata.agents.iter().any(|agent| agent.running) {
        return Ok(RuntimeSessionState::Idle);
    }

    Ok(RuntimeSessionState::Idle)
}

pub fn attach_runtime_session(namespace: &str, agent: &str) -> Result<()> {
    let session = session_metadata_for_namespace(namespace)?;
    match session.backend.as_str() {
        "codex-app" => attach_codex_app(namespace),
        _ => attach_native(namespace, agent),
    }
}

pub fn tell_runtime_session(
    namespace: &str,
    agent: &str,
    contents: &str,
    press_enter: bool,
    mode: CodexAppInputMode,
) -> Result<()> {
    let session = session_metadata_for_namespace(namespace)?;
    match session.backend.as_str() {
        "codex-app" => {
            if !press_enter {
                return Err(anyhow!(
                    "--no-enter is not supported for codex app sessions"
                ));
            }
            if agent != "agent0" {
                return Err(anyhow!(
                    "codex app sessions expose a single logical agent named agent0"
                ));
            }
            tell_codex_app_with_mode(namespace, contents, mode)
        }
        _ => tell_native(namespace, agent, contents, press_enter),
    }
}

pub fn interrupt_runtime_session(namespace: &str, agent: &str) -> Result<()> {
    let session = session_metadata_for_namespace(namespace)?;
    match session.backend.as_str() {
        "codex-app" => {
            if agent != "agent0" {
                return Err(anyhow!(
                    "codex app sessions expose a single logical agent named agent0"
                ));
            }
            interrupt_codex_app(namespace)
        }
        _ => interrupt_native(namespace, agent),
    }
}

pub fn delete_runtime_session(namespace: &str) -> Result<()> {
    let session = match session_metadata_for_namespace(namespace) {
        Ok(session) => session,
        Err(error) => {
            if codex_app_session_dir_exists(namespace)? {
                cleanup_stale_session(namespace)?;
                return Ok(());
            }
            return Err(error);
        }
    };
    match session.backend.as_str() {
        "codex-app" => delete_codex_app_session(namespace),
        _ => delete_native_session(namespace),
    }
}

pub fn delete_runtime_session_if_exists(namespace: &str) -> Result<()> {
    match maybe_session_metadata_for_namespace(namespace) {
        Ok(Some(_)) => delete_runtime_session(namespace),
        Ok(None) => {
            if codex_app_session_dir_exists(namespace)? {
                cleanup_stale_session(namespace)?;
            }
            Ok(())
        }
        Err(error) => {
            if codex_app_session_dir_exists(namespace)? {
                cleanup_stale_session(namespace)?;
                return Ok(());
            }
            Err(error)
        }
    }
}

pub fn cancel_runtime_session(namespace: &str, agent: &str) -> Result<()> {
    if let Some(metadata) = maybe_session_metadata_for_namespace(namespace)? {
        if metadata.backend == "codex-app" {
            if metadata
                .context
                .as_ref()
                .and_then(|context| context.turn_status.as_deref())
                == Some("inProgress")
            {
                let _ = tell_codex_app(
                    namespace,
                    "Stop the current turn and wait for operator guidance.",
                );
                thread::sleep(Duration::from_millis(200));
                let _ = interrupt_codex_app(namespace);
                thread::sleep(Duration::from_millis(100));
            }
            return delete_runtime_session(namespace);
        }
    }

    if probe_runtime_session_state(namespace)? == RuntimeSessionState::ActiveWork {
        let _ = tell_native(namespace, agent, "/clear", true);
        thread::sleep(Duration::from_millis(200));
        let _ = interrupt_native(namespace, agent);
        thread::sleep(Duration::from_millis(100));
    }

    delete_runtime_session_if_exists(namespace)
}

fn maybe_session_metadata_for_namespace(namespace: &str) -> Result<Option<NativeSessionMetadata>> {
    if let Some(session) = codex_app_session_metadata(namespace)? {
        return Ok(Some(session));
    }
    if let Some(session) = native_session_metadata(namespace)? {
        return Ok(Some(session));
    }
    Ok(None)
}

fn process_tree_has_codex(system: &System, root_pid: Pid) -> bool {
    let mut pending = vec![root_pid];
    let mut visited = std::collections::BTreeSet::new();

    while let Some(pid) = pending.pop() {
        if !visited.insert(pid) {
            continue;
        }

        let Some(process) = system.process(pid) else {
            continue;
        };
        if process.name().to_string_lossy() == "codex" {
            return true;
        }

        for child in system.processes().values() {
            if child.parent() == Some(pid) {
                pending.push(child.pid());
            }
        }
    }

    false
}
