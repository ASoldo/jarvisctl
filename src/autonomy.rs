use crate::capability::{AutonomyReconcileReport, reconcile_autonomy};
use crate::control_plane::ControlPlaneOutput;
use crate::mission::MissionRecord;
use crate::operator_request::{OperatorRequestCreateOptions, create_operator_request};
use crate::proposal::ProposalRecord;
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

const SERVICE_NAME: &str = "jarvisctl-autonomy.service";
const TIMER_NAME: &str = "jarvisctl-autonomy.timer";

#[derive(Debug, Clone)]
pub struct AutonomyDaemonOptions {
    pub interval_seconds: u64,
    pub notify: bool,
    pub once: bool,
}

#[derive(Debug, Clone)]
pub struct AutonomyServiceInstallOptions {
    pub interval_seconds: u64,
    pub notify: bool,
    pub enable: bool,
    pub start: bool,
    pub request_linger: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomyDaemonIteration {
    pub iteration: u64,
    pub report: AutonomyReconcileReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomyServiceStatus {
    pub service_unit: String,
    pub timer_unit: String,
    pub service_unit_path: String,
    pub timer_unit_path: String,
    pub service_active: String,
    pub service_enabled: String,
    pub timer_active: String,
    pub timer_enabled: String,
    pub linger: String,
    pub linger_request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomyServiceInstallReport {
    pub installed: bool,
    pub enabled: bool,
    pub started: bool,
    pub service_unit_path: String,
    pub timer_unit_path: String,
    pub linger: String,
    pub linger_request_id: Option<String>,
    pub commands: Vec<String>,
}

pub fn run_autonomy_daemon<F>(
    options: AutonomyDaemonOptions,
    mut reconcile: F,
) -> anyhow::Result<()>
where
    F: FnMut(bool, bool) -> anyhow::Result<AutonomyReconcileReport>,
{
    let interval = options.interval_seconds.max(30);
    let mut iteration = 0;
    loop {
        iteration += 1;
        let report = reconcile(options.notify, false)?;
        println!(
            "{}",
            serde_json::to_string(&AutonomyDaemonIteration { iteration, report })
                .context("failed to encode autonomy daemon iteration")?
        );
        if options.once {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(interval));
    }
}

pub fn reconcile_from_records(
    missions: &[MissionRecord],
    proposals: &[ProposalRecord],
    notify: bool,
    dry_run: bool,
) -> anyhow::Result<AutonomyReconcileReport> {
    reconcile_autonomy(missions, proposals, notify, dry_run)
}

pub fn install_autonomy_user_service(
    options: AutonomyServiceInstallOptions,
) -> anyhow::Result<AutonomyServiceInstallReport> {
    let dir = systemd_user_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("failed to create '{}'", dir.display()))?;
    let service_path = dir.join(SERVICE_NAME);
    let timer_path = dir.join(TIMER_NAME);
    let interval = options.interval_seconds.max(60);
    let notify_flag = if options.notify { " --notify" } else { "" };
    let service = format!(
        "[Unit]\nDescription=jarvisctl autonomy reconciler\nAfter=default.target\n\n[Service]\nType=oneshot\nExecStart=/bin/sh -lc 'exec \"$HOME/.cargo/bin/jarvisctl\" autonomy reconcile{notify_flag}'\nEnvironment=RUST_LOG=warn\nNice=15\nIOSchedulingClass=best-effort\nIOSchedulingPriority=7\n\n[Install]\nWantedBy=default.target\n"
    );
    let timer = format!(
        "[Unit]\nDescription=Run jarvisctl autonomy reconciler periodically\n\n[Timer]\nOnBootSec=2min\nOnUnitActiveSec={interval}s\nAccuracySec=30s\nPersistent=true\nUnit={SERVICE_NAME}\n\n[Install]\nWantedBy=timers.target\n"
    );
    fs::write(&service_path, service)
        .with_context(|| format!("failed to write '{}'", service_path.display()))?;
    fs::write(&timer_path, timer)
        .with_context(|| format!("failed to write '{}'", timer_path.display()))?;

    let mut commands = vec![
        format!("systemctl --user daemon-reload"),
        format!("systemctl --user enable {}", TIMER_NAME),
    ];
    run_systemctl_user(&["daemon-reload"], false)?;
    let mut enabled = false;
    let mut started = false;
    if options.enable || options.start {
        let mut args = vec!["enable"];
        if options.start {
            args.push("--now");
        }
        args.push(TIMER_NAME);
        run_systemctl_user(&args, false)?;
        enabled = true;
        started = options.start;
        if options.start {
            commands[1] = format!("systemctl --user enable --now {}", TIMER_NAME);
        }
    }

    let mut linger_request_id = None;
    let linger = user_linger_state();
    if options.request_linger && linger != "yes" {
        let user = env::var("USER").unwrap_or_else(|_| "rootster".to_string());
        let request = create_operator_request(OperatorRequestCreateOptions {
            title: format!("Enable linger for {user}"),
            kind: "sudo".to_string(),
            severity: "high".to_string(),
            reason: "Allow jarvisctl autonomy user timers to keep running after the operator logs out.".to_string(),
            risk: Some("Enabling linger lets this user's systemd services continue without an interactive login session.".to_string()),
            requested_by: Some("jarvisctl-autonomy-install".to_string()),
            namespace: None,
            request_id: None,
            method: Some("sudo".to_string()),
            command: Some(format!("sudo loginctl enable-linger {user}")),
            params: None,
            ttl_seconds: Some(12 * 60 * 60),
        })?;
        linger_request_id = Some(request.id);
    }

    Ok(AutonomyServiceInstallReport {
        installed: true,
        enabled,
        started,
        service_unit_path: service_path.display().to_string(),
        timer_unit_path: timer_path.display().to_string(),
        linger,
        linger_request_id,
        commands,
    })
}

pub fn uninstall_autonomy_user_service() -> anyhow::Result<AutonomyServiceStatus> {
    let dir = systemd_user_dir()?;
    let service_path = dir.join(SERVICE_NAME);
    let timer_path = dir.join(TIMER_NAME);
    let _ = run_systemctl_user(&["disable", "--now", TIMER_NAME], true);
    let _ = run_systemctl_user(&["daemon-reload"], true);
    if service_path.exists() {
        fs::remove_file(&service_path)
            .with_context(|| format!("failed to remove '{}'", service_path.display()))?;
    }
    if timer_path.exists() {
        fs::remove_file(&timer_path)
            .with_context(|| format!("failed to remove '{}'", timer_path.display()))?;
    }
    let _ = run_systemctl_user(&["daemon-reload"], true);
    autonomy_service_status()
}

pub fn autonomy_service_status() -> anyhow::Result<AutonomyServiceStatus> {
    let dir = systemd_user_dir()?;
    let service_path = dir.join(SERVICE_NAME);
    let timer_path = dir.join(TIMER_NAME);
    Ok(AutonomyServiceStatus {
        service_unit: SERVICE_NAME.to_string(),
        timer_unit: TIMER_NAME.to_string(),
        service_unit_path: service_path.display().to_string(),
        timer_unit_path: timer_path.display().to_string(),
        service_active: systemctl_user_value(&["is-active", SERVICE_NAME]),
        service_enabled: systemctl_user_value(&["is-enabled", SERVICE_NAME]),
        timer_active: systemctl_user_value(&["is-active", TIMER_NAME]),
        timer_enabled: systemctl_user_value(&["is-enabled", TIMER_NAME]),
        linger: user_linger_state(),
        linger_request_id: None,
    })
}

pub fn render_autonomy_service_status(
    status: &AutonomyServiceStatus,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(status).context("failed to encode autonomy service status")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(status).context("failed to encode autonomy service status")
        }
        ControlPlaneOutput::Table => Ok(format!(
            "SERVICE\tSERVICE_ACTIVE\tTIMER\tTIMER_ACTIVE\tTIMER_ENABLED\tLINGER\n{}\t{}\t{}\t{}\t{}\t{}",
            status.service_unit,
            status.service_active,
            status.timer_unit,
            status.timer_active,
            status.timer_enabled,
            status.linger
        )),
    }
}

pub fn render_autonomy_service_install(
    report: &AutonomyServiceInstallReport,
    output: ControlPlaneOutput,
) -> anyhow::Result<String> {
    match output {
        ControlPlaneOutput::Json => {
            serde_json::to_string_pretty(report).context("failed to encode autonomy install report")
        }
        ControlPlaneOutput::Yaml => {
            serde_yaml::to_string(report).context("failed to encode autonomy install report")
        }
        ControlPlaneOutput::Table => Ok(format!(
            "INSTALLED\tENABLED\tSTARTED\tTIMER\tLINGER\tREQUEST\n{}\t{}\t{}\t{}\t{}\t{}",
            report.installed,
            report.enabled,
            report.started,
            report.timer_unit_path,
            report.linger,
            report.linger_request_id.as_deref().unwrap_or("-")
        )),
    }
}

fn systemd_user_dir() -> anyhow::Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/systemd/user"))
}

fn run_systemctl_user(args: &[&str], allow_failure: bool) -> anyhow::Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("failed to run systemctl --user")?;
    if !status.success() && !allow_failure {
        bail!("systemctl --user {} exited with {status}", args.join(" "));
    }
    Ok(())
}

fn systemctl_user_value(args: &[&str]) -> String {
    match Command::new("systemctl").arg("--user").args(args).output() {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if stderr.is_empty() {
                format!("inactive:{}", output.status)
            } else {
                stderr
            }
        }
        Err(error) => format!("unavailable:{error}"),
    }
}

fn user_linger_state() -> String {
    let user = env::var("USER").unwrap_or_else(|_| "rootster".to_string());
    match Command::new("loginctl")
        .args(["show-user", &user, "-p", "Linger", "--value"])
        .output()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if stderr.is_empty() {
                format!("unknown:{}", output.status)
            } else {
                stderr
            }
        }
        Err(error) => format!("unavailable:{error}"),
    }
}
