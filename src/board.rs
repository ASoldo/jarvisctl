use anyhow::{Context, anyhow, bail};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct BoardFile {
    pub path: PathBuf,
    pub preamble: Vec<String>,
    pub sections: Vec<BoardSection>,
}

#[derive(Debug, Clone)]
pub struct BoardSection {
    pub title: String,
    pub heading_line: String,
    pub lines: Vec<BoardLine>,
}

#[derive(Debug, Clone)]
pub enum BoardLine {
    Raw(String),
    Card(BoardCard),
}

#[derive(Debug, Clone)]
pub struct BoardCard {
    pub raw: String,
    pub link: String,
}

impl BoardFile {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read board '{}'", path.display()))?;
        let mut preamble = Vec::new();
        let mut sections = Vec::new();
        let mut current_section: Option<BoardSection> = None;

        for line in raw.lines() {
            if line.starts_with("## ") {
                if let Some(section) = current_section.take() {
                    sections.push(section);
                }
                let title = line.trim_start_matches("## ").trim().to_string();
                current_section = Some(BoardSection {
                    title,
                    heading_line: line.to_string(),
                    lines: Vec::new(),
                });
                continue;
            }

            match current_section.as_mut() {
                Some(section) => section.lines.push(parse_board_line(line)),
                None => preamble.push(line.to_string()),
            }
        }

        if let Some(section) = current_section.take() {
            sections.push(section);
        }

        if sections.is_empty() {
            bail!("board '{}' has no sections", path.display());
        }

        Ok(Self {
            path: path.to_path_buf(),
            preamble,
            sections,
        })
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let rendered = self.render();
        fs::write(&self.path, rendered)
            .with_context(|| format!("failed to write board '{}'", self.path.display()))
    }

    pub fn render(&self) -> String {
        let mut out = Vec::new();
        out.extend(self.preamble.iter().cloned());

        for section in &self.sections {
            out.push(section.heading_line.clone());
            out.extend(section.lines.iter().map(render_board_line));
        }

        let mut rendered = out.join("\n");
        rendered.push('\n');
        rendered
    }

    pub fn card_positions(&self) -> Vec<(String, String)> {
        let mut positions = Vec::new();
        for section in &self.sections {
            for line in &section.lines {
                if let BoardLine::Card(card) = line {
                    positions.push((card.link.clone(), section.title.clone()));
                }
            }
        }
        positions
    }

    pub fn move_card(&mut self, link: &str, destination_title: &str) -> anyhow::Result<bool> {
        let mut moved_card: Option<BoardCard> = None;

        for section in &mut self.sections {
            let mut index = 0;
            while index < section.lines.len() {
                let should_remove = matches!(
                    section.lines.get(index),
                    Some(BoardLine::Card(card)) if card.link == link
                );
                if should_remove {
                    let line = section.lines.remove(index);
                    if let BoardLine::Card(card) = line {
                        moved_card = Some(card);
                    }
                    break;
                } else {
                    index += 1;
                }
            }
            if moved_card.is_some() {
                break;
            }
        }

        let Some(card) = moved_card else {
            return Ok(false);
        };

        let destination_index = self
            .sections
            .iter()
            .position(|section| {
                normalize_column(&section.title) == normalize_column(destination_title)
            })
            .unwrap_or_else(|| {
                self.sections.push(BoardSection {
                    title: destination_title.to_string(),
                    heading_line: format!("## {}", destination_title),
                    lines: vec![BoardLine::Raw(String::new())],
                });
                self.sections.len() - 1
            });

        let section = self
            .sections
            .get_mut(destination_index)
            .ok_or_else(|| anyhow!("destination section '{}' is missing", destination_title))?;
        section.lines.push(BoardLine::Card(card));
        Ok(true)
    }
}

pub fn normalize_column(title: &str) -> String {
    title.trim().to_ascii_lowercase()
}

pub fn discover_default_boards(vault_path: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut boards = Vec::new();

    let dispatch_board = vault_path.join("Ops").join("Codex Dispatch Board.md");
    if dispatch_board.exists() {
        boards.push(dispatch_board);
    }

    let projects_dir = vault_path.join("Projects");
    if projects_dir.exists() {
        for entry in fs::read_dir(&projects_dir)
            .with_context(|| format!("failed to read '{}'", projects_dir.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let candidate = entry.path().join("Board.md");
            if candidate.exists() {
                boards.push(candidate);
            }
        }
    }

    boards.sort();
    boards.dedup();
    Ok(boards)
}

pub fn resolve_wiki_link(vault_path: &Path, link: &str) -> PathBuf {
    let mut clean = link.trim();
    if let Some((before_alias, _)) = clean.split_once('|') {
        clean = before_alias;
    }

    let mut resolved = vault_path.join(clean);
    if resolved.extension().is_none() {
        resolved.set_extension("md");
    }
    resolved
}

fn parse_board_line(line: &str) -> BoardLine {
    if let Some(link) = extract_ticket_link(line) {
        return BoardLine::Card(BoardCard {
            raw: line.to_string(),
            link,
        });
    }
    BoardLine::Raw(line.to_string())
}

fn render_board_line(line: &BoardLine) -> String {
    match line {
        BoardLine::Raw(raw) => raw.clone(),
        BoardLine::Card(card) => card.raw.clone(),
    }
}

fn extract_ticket_link(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if !(trimmed.starts_with("- [ ] ") || trimmed.starts_with("- [x] ")) {
        return None;
    }

    let start = trimmed.find("[[")?;
    let end = trimmed[start + 2..].find("]]")?;
    let link = &trimmed[start + 2..start + 2 + end];
    Some(link.to_string())
}
