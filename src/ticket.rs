use anyhow::{Context, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TicketFrontmatter {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub autostart: Option<bool>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub repo_path: Option<String>,
    #[serde(default)]
    pub codex_sandbox_mode: Option<String>,
    #[serde(default)]
    pub codex_approval_policy: Option<String>,
    #[serde(default)]
    pub codex_profile: Option<String>,
    #[serde(default)]
    pub codex_model: Option<String>,
    #[serde(default)]
    pub codex_reasoning_effort: Option<String>,
    #[serde(default)]
    pub codex_completion_status: Option<String>,
    #[serde(default)]
    pub codex_completion_column: Option<String>,
    #[serde(default, alias = "codex_finish_tmux")]
    pub codex_finish_mode: Option<String>,
    #[serde(default)]
    pub codex_search: Option<bool>,
    #[serde(default)]
    pub codex_add_dirs: Vec<String>,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub updated: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TicketNote {
    pub path: PathBuf,
    pub frontmatter: TicketFrontmatter,
    pub title: String,
    pub sections: BTreeMap<String, String>,
}

impl TicketNote {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read ticket note '{}'", path.display()))?;
        let (frontmatter_raw, body) = split_frontmatter(&raw)?;
        let frontmatter: TicketFrontmatter =
            serde_yaml::from_str(&frontmatter_raw).context("failed to parse YAML frontmatter")?;
        let title = frontmatter
            .title
            .clone()
            .or_else(|| extract_title(&body))
            .unwrap_or_else(|| {
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().to_string())
                    .unwrap_or_else(|| "untitled-ticket".to_string())
            });

        Ok(Self {
            path: path.to_path_buf(),
            frontmatter,
            title,
            sections: parse_sections(&body),
        })
    }

    pub fn validate_codex_minimum(&self) -> anyhow::Result<()> {
        if let Some(kind) = &self.frontmatter.kind {
            ensure!(
                kind.eq_ignore_ascii_case("ticket"),
                "ticket note '{}' has unsupported type '{}'",
                self.path.display(),
                kind
            );
        }

        let repo_path =
            self.frontmatter.repo_path.as_deref().ok_or_else(|| {
                anyhow!("ticket note '{}' is missing repo_path", self.path.display())
            })?;
        ensure!(
            Path::new(repo_path).exists(),
            "repo_path '{}' does not exist",
            repo_path
        );

        self.codex_cli_args()?;
        self.finish_session_policy()?;

        Ok(())
    }

    pub fn repo_path(&self) -> Option<&str> {
        self.frontmatter.repo_path.as_deref()
    }

    pub fn effective_id(&self) -> String {
        self.frontmatter
            .id
            .clone()
            .unwrap_or_else(|| slugify(&self.title))
    }

    pub fn section(&self, name: &str) -> Option<&str> {
        self.sections.get(name).map(String::as_str)
    }

    pub fn render_codex_prompt(&self) -> String {
        let mut lines = vec![
            format!(
                "Take '{}' at '{}' as the execution contract.",
                self.title,
                self.path.display()
            ),
            String::new(),
        ];

        if let Some(repo_path) = self.repo_path() {
            lines.push(format!("Repo: {}", repo_path));
        }
        if let Some(project) = self.frontmatter.project.as_deref() {
            lines.push(format!("Project: {}", project));
        }
        if let Some(priority) = self.frontmatter.priority.as_deref() {
            lines.push(format!("Priority: {}", priority));
        }
        if let Some(owner) = self.frontmatter.owner.as_deref() {
            lines.push(format!("Owner: {}", owner));
        }
        if let Some(status) = self.frontmatter.status.as_deref() {
            lines.push(format!("Status: {}", status));
        }
        if let Some(runtime) = self.codex_runtime_summary() {
            lines.push(format!("Codex runtime: {}", runtime));
        }

        for section_name in [
            "Request",
            "Definition Of Done",
            "Context",
            "Constraints",
            "Execution Handoff",
        ] {
            if let Some(section) = self.section(section_name) {
                let cleaned = section.trim();
                if cleaned.is_empty() {
                    continue;
                }
                lines.push(String::new());
                lines.push(format!("{}:", section_name));
                lines.push(cleaned.to_string());
            }
        }

        lines.push(String::new());
        lines.push("Instructions:".to_string());
        lines.push("- Treat the ticket note as the source of truth.".to_string());
        lines.push("- Update the ticket progress and outcome as you work.".to_string());
        lines.push("- Validate your changes before you finish.".to_string());

        lines.join("\n")
    }

    pub fn readiness_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        if !matches!(
            self.frontmatter.owner.as_deref(),
            Some(owner) if owner.eq_ignore_ascii_case("codex")
        ) {
            warnings.push(
                "ticket owner is not 'codex'; future board automation should only dispatch codex-owned tickets"
                    .to_string(),
            );
        }

        warnings
    }

    pub fn codex_cli_args(&self) -> anyhow::Result<Vec<String>> {
        let mut args = Vec::new();

        if let Some(profile) = self.frontmatter.codex_profile.as_deref() {
            ensure!(
                !profile.trim().is_empty(),
                "ticket '{}' has an empty codex_profile",
                self.path.display()
            );
            args.push("--profile".to_string());
            args.push(profile.trim().to_string());
        }

        if let Some(model) = self.frontmatter.codex_model.as_deref() {
            ensure!(
                !model.trim().is_empty(),
                "ticket '{}' has an empty codex_model",
                self.path.display()
            );
            args.push("--model".to_string());
            args.push(model.trim().to_string());
        }

        if let Some(reasoning_effort) = self.frontmatter.codex_reasoning_effort.as_deref() {
            let normalized = reasoning_effort.trim();
            ensure!(
                matches!(
                    normalized,
                    "none" | "minimal" | "low" | "medium" | "high" | "xhigh"
                ),
                "ticket '{}' has unsupported codex_reasoning_effort '{}'",
                self.path.display(),
                reasoning_effort
            );
            args.push("--config".to_string());
            args.push(format!("model_reasoning_effort=\"{}\"", normalized));
        }

        if let Some(sandbox_mode) = self.frontmatter.codex_sandbox_mode.as_deref() {
            let normalized = sandbox_mode.trim();
            ensure!(
                matches!(
                    normalized,
                    "read-only" | "workspace-write" | "danger-full-access"
                ),
                "ticket '{}' has unsupported codex_sandbox_mode '{}'",
                self.path.display(),
                sandbox_mode
            );
            args.push("--sandbox".to_string());
            args.push(normalized.to_string());
        }

        if let Some(approval_policy) = self.frontmatter.codex_approval_policy.as_deref() {
            let normalized = approval_policy.trim();
            ensure!(
                matches!(
                    normalized,
                    "untrusted" | "on-failure" | "on-request" | "never"
                ),
                "ticket '{}' has unsupported codex_approval_policy '{}'",
                self.path.display(),
                approval_policy
            );
            args.push("--ask-for-approval".to_string());
            args.push(normalized.to_string());
        }

        if self.frontmatter.codex_search == Some(true) {
            args.push("--search".to_string());
        }

        for add_dir in &self.frontmatter.codex_add_dirs {
            let trimmed = add_dir.trim();
            ensure!(
                !trimmed.is_empty(),
                "ticket '{}' contains an empty codex_add_dirs entry",
                self.path.display()
            );
            args.push("--add-dir".to_string());
            args.push(trimmed.to_string());
        }

        Ok(args)
    }

    pub fn codex_runtime_summary(&self) -> Option<String> {
        let mut parts = Vec::new();

        if let Some(profile) = self.frontmatter.codex_profile.as_deref() {
            parts.push(format!("profile={}", profile));
        }
        if let Some(model) = self.frontmatter.codex_model.as_deref() {
            parts.push(format!("model={}", model));
        }
        if let Some(reasoning_effort) = self.frontmatter.codex_reasoning_effort.as_deref() {
            parts.push(format!("reasoning={}", reasoning_effort));
        }
        parts.push(format!(
            "finish={}",
            self.finish_session_policy().unwrap_or("close")
        ));
        if let Some(sandbox_mode) = self.frontmatter.codex_sandbox_mode.as_deref() {
            parts.push(format!("sandbox={}", sandbox_mode));
        }
        if let Some(approval_policy) = self.frontmatter.codex_approval_policy.as_deref() {
            parts.push(format!("approval={}", approval_policy));
        }
        if self.frontmatter.codex_search == Some(true) {
            parts.push("search=enabled".to_string());
        }
        if !self.frontmatter.codex_add_dirs.is_empty() {
            parts.push(format!(
                "add_dirs={}",
                self.frontmatter.codex_add_dirs.join(",")
            ));
        }

        if parts.is_empty() {
            None
        } else {
            Some(parts.join(", "))
        }
    }

    pub fn completion_status(&self) -> String {
        self.frontmatter
            .codex_completion_status
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("review")
            .to_string()
    }

    pub fn completion_column(&self) -> String {
        self.frontmatter
            .codex_completion_column
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("Review")
            .to_string()
    }

    pub fn finish_session_policy(&self) -> anyhow::Result<&str> {
        let policy = self
            .frontmatter
            .codex_finish_mode
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("close");
        ensure!(
            matches!(policy, "close" | "keep"),
            "ticket '{}' has unsupported codex_finish_mode '{}'",
            self.path.display(),
            policy
        );
        Ok(policy)
    }
}

fn split_frontmatter(raw: &str) -> anyhow::Result<(String, String)> {
    let mut lines = raw.lines();
    if lines.next().map(str::trim) != Some("---") {
        bail!("ticket note must start with YAML frontmatter");
    }

    let mut frontmatter = Vec::new();
    let mut body = Vec::new();
    let mut in_frontmatter = true;

    for line in lines {
        if in_frontmatter && line.trim() == "---" {
            in_frontmatter = false;
            continue;
        }
        if in_frontmatter {
            frontmatter.push(line);
        } else {
            body.push(line);
        }
    }

    if in_frontmatter {
        bail!("ticket note has unterminated YAML frontmatter");
    }

    Ok((frontmatter.join("\n"), body.join("\n")))
}

fn extract_title(body: &str) -> Option<String> {
    body.lines()
        .find_map(|line| line.strip_prefix("# ").map(str::trim))
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_sections(body: &str) -> BTreeMap<String, String> {
    let mut sections = BTreeMap::new();
    let mut current_name: Option<String> = None;
    let mut current_lines: Vec<String> = Vec::new();

    for line in body.lines() {
        if let Some(name) = line.strip_prefix("## ") {
            flush_section(&mut sections, &mut current_name, &mut current_lines);
            current_name = Some(name.trim().to_string());
            continue;
        }

        if current_name.is_some() {
            current_lines.push(line.to_string());
        }
    }

    flush_section(&mut sections, &mut current_name, &mut current_lines);
    sections
}

fn flush_section(
    sections: &mut BTreeMap<String, String>,
    current_name: &mut Option<String>,
    current_lines: &mut Vec<String>,
) {
    if let Some(name) = current_name.take() {
        sections.insert(name, current_lines.join("\n").trim().to_string());
        current_lines.clear();
    }
}

pub fn slugify(input: &str) -> String {
    let mut slug = String::with_capacity(input.len());
    let mut last_was_dash = false;

    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }

    slug.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::TicketNote;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after UNIX_EPOCH")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nonce}"))
    }

    #[test]
    fn load_parses_markdown_ticket_sections_and_prompt() {
        let root = unique_temp_dir("jarvisctl-ticket-load");
        let repo_path = root.join("repo");
        fs::create_dir_all(&repo_path).unwrap();
        let ticket_path = root.join("ticket.md");
        fs::write(
            &ticket_path,
            format!(
                r#"---
title: Markdown Launch
type: ticket
status: ready_for_codex
owner: codex
autostart: true
project: Projects/jarvisctl/Project.md
repo_path: {}
codex_finish_mode: close
---

# Markdown Launch

## Request
- Launch from a real Markdown note.

## Definition Of Done
- Parse the note body.
- Keep the prompt aligned with the ticket.

## Execution Handoff
- Use the note as the execution contract.
"#,
                repo_path.display()
            ),
        )
        .unwrap();

        let ticket = TicketNote::load(&ticket_path).unwrap();
        assert_eq!(ticket.title, "Markdown Launch");
        assert_eq!(
            ticket.section("Request"),
            Some("- Launch from a real Markdown note.")
        );
        assert!(
            ticket
                .section("Definition Of Done")
                .unwrap()
                .contains("Parse the note body.")
        );
        assert!(ticket.render_codex_prompt().contains("Execution Handoff:"));
        assert_eq!(ticket.completion_column(), "Review");
        assert_eq!(ticket.finish_session_policy().unwrap(), "close");
        ticket.validate_codex_minimum().unwrap();

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_uses_markdown_heading_when_frontmatter_title_is_missing() {
        let root = unique_temp_dir("jarvisctl-ticket-heading");
        let repo_path = root.join("repo");
        fs::create_dir_all(&repo_path).unwrap();
        let ticket_path = root.join("ticket.md");
        fs::write(
            &ticket_path,
            format!(
                r#"---
type: ticket
owner: codex
repo_path: {}
---

# Heading Derived Title

## Request
- Verify title fallback.
"#,
                repo_path.display()
            ),
        )
        .unwrap();

        let ticket = TicketNote::load(&ticket_path).unwrap();
        assert_eq!(ticket.title, "Heading Derived Title");
        assert_eq!(ticket.effective_id(), "heading-derived-title");
        assert!(
            ticket
                .render_codex_prompt()
                .contains("Heading Derived Title")
        );
        let _ = fs::remove_dir_all(root);
    }
}
