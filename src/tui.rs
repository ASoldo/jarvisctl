use std::io::stdout;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use crossterm::{
    cursor,
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};

use crate::codex::enrich_native_sessions;
use crate::native::{NativeSessionMetadata, RuntimeContextMetadata, collect_native_sessions};
use crate::{SessionBackend, delete_session, exec_agent, interrupt_agent};

const BG: Color = Color::Reset;
const PL_A: Color = Color::Rgb(17, 94, 89);
const PL_B: Color = Color::Rgb(30, 64, 175);
const PL_C: Color = Color::Rgb(55, 48, 163);
const PL_D: Color = Color::Rgb(82, 24, 124);
const BORDER: Color = Color::Rgb(70, 67, 72);
const ROW_HIGHLIGHT: Color = Color::Rgb(48, 45, 50);
const TEXT: Color = Color::Rgb(214, 211, 216);
const SUBTLE_TEXT: Color = Color::Rgb(149, 145, 153);

pub fn view_agent(name: &str, output: Arc<Mutex<Vec<String>>>) -> anyhow::Result<()> {
    let mut stdout = stdout();
    enable_raw_mode()?;
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        cursor::Hide
    )?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    loop {
        terminal.draw(|f| {
            let area = Layout::default()
                .direction(Direction::Vertical)
                .margin(1)
                .constraints([Constraint::Min(1)].as_ref())
                .split(f.area());

            let max_lines = area[0].height.saturating_sub(2) as usize;

            let log_content = {
                let out = output.lock().unwrap();
                let joined = out.join("\n");
                let stripped = strip_ansi_escapes::strip(joined.as_bytes());
                let clean = String::from_utf8_lossy(&stripped);
                let lines: Vec<&str> = clean.lines().collect();

                let start = lines.len().saturating_sub(max_lines);
                lines[start..].join("\n")
            };

            let paragraph = Paragraph::new(log_content)
                .block(
                    Block::default()
                        .title(name)
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(BORDER)),
                )
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(TEXT));

            f.render_widget(paragraph, area[0]);
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    _ => {}
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        cursor::Show
    )?;

    Ok(())
}

#[derive(Clone, Debug)]
struct DashboardRow {
    namespace: String,
    agent: String,
    pid: u32,
    running: bool,
    working_directory: Option<String>,
    shell_command: String,
    context: Option<RuntimeContextMetadata>,
}

#[derive(Debug)]
struct DashboardApp {
    backend: SessionBackend,
    rows: Vec<DashboardRow>,
    selected: usize,
    status: String,
    refresh_every: Duration,
    last_refresh: Instant,
}

impl DashboardApp {
    fn new(backend: SessionBackend, refresh_every: Duration) -> anyhow::Result<Self> {
        let mut app = Self {
            backend,
            rows: Vec::new(),
            selected: 0,
            status: String::from("ready"),
            refresh_every,
            last_refresh: Instant::now(),
        };
        app.refresh()?;
        Ok(app)
    }

    fn refresh(&mut self) -> anyhow::Result<()> {
        self.rows = load_dashboard_rows(self.backend)?;
        if self.rows.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.rows.len() {
            self.selected = self.rows.len() - 1;
        }
        self.last_refresh = Instant::now();
        Ok(())
    }

    fn selected_row(&self) -> Option<&DashboardRow> {
        self.rows.get(self.selected)
    }

    fn next(&mut self) {
        if !self.rows.is_empty() {
            self.selected = (self.selected + 1) % self.rows.len();
        }
    }

    fn previous(&mut self) {
        if !self.rows.is_empty() {
            self.selected = if self.selected == 0 {
                self.rows.len() - 1
            } else {
                self.selected - 1
            };
        }
    }
}

pub fn run_dashboard(backend: SessionBackend, refresh_ms: u64) -> anyhow::Result<()> {
    let mut stdout = stdout();
    enable_raw_mode()?;
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        cursor::Hide
    )?;

    let backend_renderer = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend_renderer)?;
    terminal.clear()?;

    let mut app = DashboardApp::new(backend, Duration::from_millis(refresh_ms.max(200)))?;

    let result = (|| -> anyhow::Result<()> {
        loop {
            if app.last_refresh.elapsed() >= app.refresh_every {
                app.refresh()?;
            }

            terminal.draw(|frame| render_dashboard(frame, &app))?;

            if !event::poll(Duration::from_millis(100))? {
                continue;
            }

            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Down | KeyCode::Char('j') => app.next(),
                KeyCode::Up | KeyCode::Char('k') => app.previous(),
                KeyCode::Char('r') => {
                    app.refresh()?;
                    app.status = "refreshed".to_string();
                }
                KeyCode::Char('i') => {
                    if let Some(row) = app.selected_row().cloned() {
                        interrupt_agent(app.backend, &row.namespace, &row.agent)
                            .map_err(anyhow::Error::from)?;
                        app.status = format!("sent interrupt to {}:{}", row.namespace, row.agent);
                    }
                }
                KeyCode::Char('x') => {
                    if let Some(row) = app.selected_row().cloned() {
                        suspend_terminal(&mut terminal)?;
                        let action = delete_session(app.backend, &row.namespace);
                        resume_terminal(&mut terminal)?;
                        action.map_err(anyhow::Error::from)?;
                        app.refresh()?;
                        app.status = format!("closed {}", row.namespace);
                    }
                }
                KeyCode::Enter => {
                    if let Some(row) = app.selected_row().cloned() {
                        suspend_terminal(&mut terminal)?;
                        let action = exec_agent(app.backend, &row.namespace, &row.agent);
                        resume_terminal(&mut terminal)?;
                        action.map_err(anyhow::Error::from)?;
                        app.refresh()?;
                        app.status = format!("returned from {}:{}", row.namespace, row.agent);
                    }
                }
                KeyCode::Char('t') => {
                    if let Some(row) = app.selected_row().cloned() {
                        if let Some(transcript_path) = row
                            .context
                            .as_ref()
                            .and_then(|context| context.transcript_path.as_deref())
                        {
                            suspend_terminal(&mut terminal)?;
                            let action = open_transcript_viewer(transcript_path);
                            resume_terminal(&mut terminal)?;
                            action.map_err(anyhow::Error::from)?;
                            app.status = format!("viewed transcript for {}", row.namespace);
                        } else {
                            app.status = format!("no transcript recorded for {}", row.namespace);
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(())
    })();

    let cleanup_result = leave_terminal(&mut terminal);
    result?;
    cleanup_result?;
    Ok(())
}

fn load_dashboard_rows(backend: SessionBackend) -> anyhow::Result<Vec<DashboardRow>> {
    let _ = backend;
    let mut sessions = collect_native_sessions()?;
    enrich_native_sessions(&mut sessions)?;
    Ok(flatten_native_rows(&sessions))
}

fn flatten_native_rows(sessions: &[NativeSessionMetadata]) -> Vec<DashboardRow> {
    let mut rows = Vec::new();
    for session in sessions {
        for agent in &session.agents {
            rows.push(DashboardRow {
                namespace: session.namespace.clone(),
                agent: agent.name.clone(),
                pid: agent.pid,
                running: agent.running,
                working_directory: session.working_directory.clone(),
                shell_command: session.shell_command.clone(),
                context: session.context.clone(),
            });
        }
    }
    rows.sort_by(|left, right| {
        left.namespace
            .cmp(&right.namespace)
            .then(left.agent.cmp(&right.agent))
    });
    rows
}

fn render_dashboard(frame: &mut Frame, app: &DashboardApp) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_dashboard_header(frame, layout[0], app);
    render_dashboard_body(frame, layout[1], app);
    render_dashboard_footer(frame, layout[2], app);
}

fn render_dashboard_header(frame: &mut Frame, area: ratatui::layout::Rect, app: &DashboardApp) {
    let active = app
        .selected_row()
        .map(|row| format!("{}:{}", row.namespace, row.agent))
        .unwrap_or_else(|| "-".to_string());

    let mut spans = Vec::new();
    push_powerline_segment(&mut spans, " runtime ", Color::Black, Color::Cyan, PL_A);
    push_powerline_segment(&mut spans, " native ", Color::White, PL_A, PL_B);
    push_powerline_segment(
        &mut spans,
        format!(" sessions {} ", app.rows.len()),
        Color::White,
        PL_B,
        PL_C,
    );
    push_powerline_segment(
        &mut spans,
        format!(" focus {} ", active),
        Color::White,
        PL_C,
        BG,
    );
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BG)),
        area,
    );
}

fn render_dashboard_body(frame: &mut Frame, area: ratatui::layout::Rect, app: &DashboardApp) {
    let sections = if area.width >= 120 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
            .split(area)
    };

    if app.rows.is_empty() {
        let empty = Paragraph::new("No active runtime sessions.")
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Sessions")
                    .border_style(Style::default().fg(BORDER)),
            )
            .style(Style::default().fg(SUBTLE_TEXT));
        frame.render_widget(empty, sections[0]);
        frame.render_widget(
            Paragraph::new(
                "Select a namespace to inspect ticket, session, and transcript details.",
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Detail")
                    .border_style(Style::default().fg(BORDER)),
            )
            .style(Style::default().fg(SUBTLE_TEXT))
            .wrap(Wrap { trim: false }),
            sections[1],
        );
        return;
    }

    let rows = app.rows.iter().map(|row| {
        let state = if row.running { "running" } else { "idle" };
        let state_style = if row.running {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Yellow)
        };
        let namespace_cell = if let Some(title) = row
            .context
            .as_ref()
            .and_then(|ctx| ctx.task_title.as_deref())
        {
            Cell::from(format!(
                "{}\n{}",
                row.namespace,
                truncate_for_cell(title, 38)
            ))
        } else {
            Cell::from(row.namespace.clone())
        };
        let row_height = if row
            .context
            .as_ref()
            .and_then(|ctx| ctx.task_title.as_deref())
            .is_some()
        {
            2
        } else {
            1
        };
        Row::new(vec![
            namespace_cell,
            Cell::from(row.agent.clone()),
            Cell::from(row.pid.to_string()),
            Cell::from(Span::styled(state, state_style)),
        ])
        .height(row_height)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(42),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(vec!["Namespace", "Agent", "PID", "State"]).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("Sessions")
            .border_style(Style::default().fg(BORDER)),
    )
    .row_highlight_style(
        Style::default()
            .bg(ROW_HIGHLIGHT)
            .fg(TEXT)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▌ ");

    let mut state = TableState::default();
    state.select(Some(app.selected));
    frame.render_stateful_widget(table, sections[0], &mut state);
    render_dashboard_detail(frame, sections[1], app.selected_row());
}

fn render_dashboard_footer(frame: &mut Frame, area: ratatui::layout::Rect, app: &DashboardApp) {
    let selected = app
        .selected_row()
        .map(|row| format!("{}:{}", row.namespace, row.agent))
        .unwrap_or_else(|| "-".to_string());

    let mut spans = Vec::new();
    push_powerline_segment(
        &mut spans,
        " enter attach ",
        Color::Black,
        Color::Cyan,
        PL_A,
    );
    push_powerline_segment(&mut spans, " i interrupt ", Color::White, PL_A, PL_B);
    push_powerline_segment(&mut spans, " x close ", Color::White, PL_B, PL_D);
    push_powerline_segment(&mut spans, " t transcript ", Color::White, PL_D, PL_C);
    push_powerline_segment(&mut spans, " r refresh ", Color::White, PL_C, PL_D);
    push_powerline_segment(
        &mut spans,
        format!(" target {} ", selected),
        Color::White,
        PL_D,
        PL_B,
    );
    push_powerline_segment(
        &mut spans,
        format!(" {} ", app.status),
        Color::White,
        PL_B,
        BG,
    );
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BG)),
        area,
    );
}

fn push_powerline_segment<'a>(
    spans: &mut Vec<Span<'a>>,
    text: impl Into<String>,
    fg: Color,
    bg: Color,
    next_bg: Color,
) {
    spans.push(Span::styled(text.into(), Style::default().fg(fg).bg(bg)));
    spans.push(Span::styled("", Style::default().fg(bg).bg(next_bg)));
}

fn suspend_terminal(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> anyhow::Result<()> {
    disable_raw_mode().context("failed to disable raw mode for dashboard suspend")?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        cursor::Show
    )?;
    Ok(())
}

fn resume_terminal(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> anyhow::Result<()> {
    enable_raw_mode().context("failed to enable raw mode for dashboard resume")?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture,
        cursor::Hide
    )?;
    terminal.clear()?;
    Ok(())
}

fn leave_terminal(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> anyhow::Result<()> {
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        cursor::Show
    )?;
    Ok(())
}

fn render_dashboard_detail(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    selected: Option<&DashboardRow>,
) {
    let Some(row) = selected else {
        frame.render_widget(
            Paragraph::new("No runtime selected.")
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Detail")
                        .border_style(Style::default().fg(BORDER)),
                )
                .style(Style::default().fg(SUBTLE_TEXT)),
            area,
        );
        return;
    };

    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("Namespace: ", Style::default().fg(SUBTLE_TEXT)),
        Span::styled(&row.namespace, Style::default().fg(TEXT)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Agent: ", Style::default().fg(SUBTLE_TEXT)),
        Span::styled(&row.agent, Style::default().fg(TEXT)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("PID: ", Style::default().fg(SUBTLE_TEXT)),
        Span::styled(row.pid.to_string(), Style::default().fg(TEXT)),
    ]));

    if let Some(context) = row.context.as_ref() {
        if let Some(workload) = context.workload.as_deref() {
            lines.push(Line::from(vec![
                Span::styled("Workload: ", Style::default().fg(SUBTLE_TEXT)),
                Span::styled(workload, Style::default().fg(TEXT)),
            ]));
        }
        if let Some(task_title) = context.task_title.as_deref() {
            lines.push(Line::from(vec![
                Span::styled("Task: ", Style::default().fg(SUBTLE_TEXT)),
                Span::styled(task_title, Style::default().fg(TEXT)),
            ]));
        }
        if let Some(task_note) = context.task_note.as_deref() {
            lines.push(Line::from(vec![
                Span::styled("Ticket: ", Style::default().fg(SUBTLE_TEXT)),
                Span::styled(short_path(task_note), Style::default().fg(TEXT)),
            ]));
        }
        if let Some(session_id) = context.codex_session_id.as_deref() {
            lines.push(Line::from(vec![
                Span::styled("Session: ", Style::default().fg(SUBTLE_TEXT)),
                Span::styled(session_id, Style::default().fg(TEXT)),
            ]));
        }
        if let Some(transcript_path) = context.transcript_path.as_deref() {
            lines.push(Line::from(vec![
                Span::styled("Transcript: ", Style::default().fg(SUBTLE_TEXT)),
                Span::styled(short_path(transcript_path), Style::default().fg(TEXT)),
            ]));
        }
    }

    if let Some(working_directory) = row.working_directory.as_deref() {
        lines.push(Line::from(vec![
            Span::styled("Repo: ", Style::default().fg(SUBTLE_TEXT)),
            Span::styled(short_path(working_directory), Style::default().fg(TEXT)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("Command: ", Style::default().fg(SUBTLE_TEXT)),
        Span::styled(&row.shell_command, Style::default().fg(TEXT)),
    ]));

    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Detail")
                    .border_style(Style::default().fg(BORDER)),
            )
            .style(Style::default().fg(TEXT))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn open_transcript_viewer(path: &str) -> anyhow::Result<()> {
    let quoted = shell_words::join([path]);
    let command = format!(
        "if command -v less >/dev/null 2>&1; then less -R +G {quoted}; else tail -n 200 {quoted}; fi"
    );
    let status = Command::new("bash").arg("-lc").arg(command).status()?;
    if status.success() {
        return Ok(());
    }
    Err(anyhow::anyhow!(
        "transcript viewer exited with status {}",
        status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string())
    ))
}

fn truncate_for_cell(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut truncated = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

fn short_path(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(path)
        .to_string()
}
