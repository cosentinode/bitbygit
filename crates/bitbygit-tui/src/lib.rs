use std::error::Error;
use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::KeyModifiers;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use bitbygit_git::{ChangeKind, Git, GitError, GitOutput, StatusEntry, StatusEntryType};
use bitbygit_store::{AuditEntry, LocalStore, StorePaths};

const MAX_PROMPT_LEN: usize = 512;

pub fn run() -> Result<(), Box<dyn Error>> {
    let mut terminal = TerminalSession::enter()?;
    let mut app = App::new();
    app.load_current_dir();

    loop {
        terminal.draw(|frame| {
            let areas = Viewport::split(frame.area());
            app.ensure_visible_focus(&areas);
            app.clamp_file_scroll_for(areas.status);
            render(&app, frame, &areas);
            app.last_viewport = areas;
        })?;

        if app.should_quit {
            break;
        }

        if event::poll(Duration::from_millis(100))? {
            app.handle_event(event::read()?);
        }
    }

    Ok(())
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> Result<Self, Box<dyn Error>> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        let setup = execute!(
            stdout,
            EnterAlternateScreen,
            event::EnableMouseCapture,
            crossterm::cursor::Hide
        )
        .and_then(|()| Terminal::new(CrosstermBackend::new(stdout)));

        let terminal = match setup {
            Ok(terminal) => terminal,
            Err(error) => {
                let _raw = disable_raw_mode();
                let _cleanup = execute!(
                    io::stdout(),
                    crossterm::cursor::Show,
                    event::DisableMouseCapture,
                    LeaveAlternateScreen
                );
                return Err(Box::new(error));
            }
        };
        Ok(Self { terminal })
    }

    fn draw<F>(&mut self, render: F) -> Result<(), io::Error>
    where
        F: FnOnce(&mut ratatui::Frame<'_>),
    {
        self.terminal.draw(render).map(|_completed| ())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _raw = disable_raw_mode();
        let _leave = execute!(
            self.terminal.backend_mut(),
            crossterm::cursor::Show,
            event::DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _cursor = self.terminal.show_cursor();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    focus: Focus,
    prompt: String,
    repos: Vec<String>,
    selected_repo: usize,
    files: Vec<FileRow>,
    selected_file: usize,
    file_scroll: usize,
    details: String,
    pending_confirmation: Option<PendingAction>,
    should_quit: bool,
    last_viewport: Viewport,
}

impl App {
    pub fn new() -> Self {
        Self {
            focus: Focus::Repos,
            prompt: String::new(),
            repos: vec![
                "bitbygit".to_owned(),
                "add repository".to_owned(),
                "recent repositories".to_owned(),
            ],
            selected_repo: 0,
            files: Vec::new(),
            selected_file: 0,
            file_scroll: 0,
            details: "No repository status loaded yet.".to_owned(),
            pending_confirmation: None,
            should_quit: false,
            last_viewport: Viewport::default(),
        }
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn load_current_dir(&mut self) {
        self.refresh_status();
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if self.pending_confirmation.is_some() {
            match key.code {
                KeyCode::Char('y') => self.confirm_pending(),
                KeyCode::Char('n') | KeyCode::Esc => self.cancel_pending(),
                _ => self.cancel_pending(),
            }
            return;
        }

        match key.code {
            KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Char('q') if self.focus != Focus::Prompt => self.should_quit = true,
            KeyCode::Tab => self.focus = self.next_visible_focus(),
            KeyCode::BackTab => self.focus = self.previous_visible_focus(),
            KeyCode::Up => self.move_selection_up(),
            KeyCode::Down => self.move_selection_down(),
            KeyCode::Char('a') if self.focus == Focus::Status => {
                self.pending_confirmation = Some(PendingAction::StageAll);
                self.details = "Stage all changes? Press y to confirm or n to cancel.".to_owned();
            }
            KeyCode::Char('A') if self.focus == Focus::Status => {
                self.pending_confirmation = Some(PendingAction::UnstageAll);
                self.details = "Unstage all changes? Press y to confirm or n to cancel.".to_owned();
            }
            KeyCode::Char('s') if self.focus == Focus::Status => self.stage_selected_file(),
            KeyCode::Char('u') if self.focus == Focus::Status => self.unstage_selected_file(),
            KeyCode::Char(value)
                if self.focus == Focus::Prompt
                    && prompt_accepts_modifiers(key.modifiers)
                    && self.prompt.len() + value.len_utf8() <= MAX_PROMPT_LEN =>
            {
                self.prompt.push(value);
            }
            KeyCode::Backspace if self.focus == Focus::Prompt => {
                let _removed = self.prompt.pop();
            }
            _ => {}
        }
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key)
                if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat =>
            {
                self.handle_key(key);
            }
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                if let Some(focus) = self.last_viewport.focus_at(mouse.column, mouse.row) {
                    self.focus = focus;
                    if focus == Focus::Repos {
                        self.select_repo_at_position(mouse.column, mouse.row);
                    }
                }
            }
            _ => {}
        }
    }

    fn ensure_visible_focus(&mut self, viewport: &Viewport) {
        if self.focus == Focus::Queue && viewport.queue.area() == 0 {
            self.focus = Focus::Prompt;
        }
    }

    fn next_visible_focus(&self) -> Focus {
        let order = self.visible_focus_order();
        cycle_focus(&order, self.focus, 1)
    }

    fn previous_visible_focus(&self) -> Focus {
        let order = self.visible_focus_order();
        cycle_focus(&order, self.focus, -1)
    }

    fn visible_focus_order(&self) -> Vec<Focus> {
        let mut order = vec![Focus::Repos, Focus::Status, Focus::Details];
        if self.last_viewport.queue.area() > 0 {
            order.push(Focus::Queue);
        }
        order.push(Focus::Prompt);
        order
    }

    fn select_repo_at_position(&mut self, column: u16, row: u16) {
        let first_row = self.last_viewport.repos.y.saturating_add(1);
        let last_row = self
            .last_viewport
            .repos
            .y
            .saturating_add(self.last_viewport.repos.height.saturating_sub(1));
        let first_column = self.last_viewport.repos.x.saturating_add(1);
        let last_column = self
            .last_viewport
            .repos
            .x
            .saturating_add(self.last_viewport.repos.width.saturating_sub(1));
        if row < first_row || row >= last_row || column < first_column || column >= last_column {
            return;
        }
        let index = row.saturating_sub(first_row) as usize;
        if index < self.repos.len() {
            self.selected_repo = index;
        }
    }

    fn move_selection_up(&mut self) {
        match self.focus {
            Focus::Repos if self.selected_repo > 0 => self.selected_repo -= 1,
            Focus::Status if self.selected_file > 0 => {
                self.selected_file -= 1;
                self.clamp_file_scroll();
                self.refresh_diff();
            }
            _ => {}
        }
    }

    fn move_selection_down(&mut self) {
        match self.focus {
            Focus::Repos if self.selected_repo + 1 < self.repos.len() => self.selected_repo += 1,
            Focus::Status if self.selected_file + 1 < self.files.len() => {
                self.selected_file += 1;
                self.clamp_file_scroll();
                self.refresh_diff();
            }
            _ => {}
        }
    }

    fn refresh_status(&mut self) {
        match Git::new(current_dir()).status() {
            Ok(status) => {
                self.files = status
                    .entries
                    .iter()
                    .flat_map(FileRow::from_entry)
                    .collect();
                self.files.sort_by(|left, right| {
                    left.section
                        .cmp(&right.section)
                        .then(left.path.cmp(&right.path))
                });
                if self.selected_file >= self.files.len() {
                    self.selected_file = self.files.len().saturating_sub(1);
                }
                self.clamp_file_scroll();
                self.refresh_diff();
            }
            Err(error) => {
                self.files.clear();
                self.selected_file = 0;
                self.details = format!("Unable to read repository status: {error}");
            }
        }
    }

    fn refresh_diff(&mut self) {
        let Some(file) = self.files.get(self.selected_file) else {
            self.details = "No changed file selected.".to_owned();
            return;
        };
        match Git::new(current_dir())
            .diff_paths(&file.pathspecs, file.section == FileSection::Staged)
        {
            Ok(diff) if diff.is_empty() && file.section == FileSection::Untracked => {
                self.details =
                    "Untracked file; stage it to include it in the next commit.".to_owned();
            }
            Ok(diff) if diff.is_empty() => {
                self.details = "No textual diff for selected file.".to_owned();
            }
            Ok(diff) => self.details = diff,
            Err(error) => self.details = format!("Unable to render diff: {error}"),
        }
    }

    fn stage_selected_file(&mut self) {
        let Some(file) = self.files.get(self.selected_file).cloned() else {
            return;
        };
        if !file.can_stage() {
            self.details = "Selected row has no unstaged changes to stage.".to_owned();
            return;
        }
        let message = run_audited_git_operation("stage", "stage_path", || {
            Git::new(current_dir()).stage_paths(&file.pathspecs)
        });
        self.refresh_status();
        self.details = message;
    }

    fn unstage_selected_file(&mut self) {
        let Some(file) = self.files.get(self.selected_file).cloned() else {
            return;
        };
        if !file.can_unstage() {
            self.details = "Selected row has no staged changes to unstage.".to_owned();
            return;
        }
        let message = run_audited_git_operation("unstage", "unstage_path", || {
            Git::new(current_dir()).unstage_paths(&file.pathspecs)
        });
        self.refresh_status();
        self.details = message;
    }

    fn confirm_pending(&mut self) {
        let Some(action) = self.pending_confirmation.take() else {
            return;
        };
        let message = run_audited_git_operation(action.label(), action.operation(), || {
            let git = Git::new(current_dir());
            match action {
                PendingAction::StageAll => git.stage_all(),
                PendingAction::UnstageAll => git.unstage_all(),
            }
        });
        self.refresh_status();
        self.details = message;
    }

    fn cancel_pending(&mut self) {
        self.pending_confirmation = None;
        self.details = "Operation cancelled.".to_owned();
    }

    fn clamp_file_scroll(&mut self) {
        self.clamp_file_scroll_for(self.last_viewport.status);
    }

    fn clamp_file_scroll_for(&mut self, area: Rect) {
        let visible_len = status_visible_len(area);
        if self.selected_file < self.file_scroll {
            self.file_scroll = self.selected_file;
        }
        let window_end = self.file_scroll.saturating_add(visible_len);
        if self.selected_file >= window_end {
            self.file_scroll = self.selected_file.saturating_sub(visible_len - 1);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingAction {
    StageAll,
    UnstageAll,
}

impl PendingAction {
    fn label(self) -> &'static str {
        match self {
            Self::StageAll => "stage all",
            Self::UnstageAll => "unstage all",
        }
    }

    fn operation(self) -> &'static str {
        match self {
            Self::StageAll => "stage_all",
            Self::UnstageAll => "unstage_all",
        }
    }
}

fn prompt_accepts_modifiers(modifiers: KeyModifiers) -> bool {
    modifiers.is_empty() || modifiers == KeyModifiers::SHIFT
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Repos,
    Status,
    Details,
    Queue,
    Prompt,
}

fn cycle_focus(order: &[Focus], current: Focus, offset: isize) -> Focus {
    let Some(index) = order.iter().position(|focus| *focus == current) else {
        return order.first().copied().unwrap_or(Focus::Repos);
    };
    let len = order.len() as isize;
    let next = (index as isize + offset).rem_euclid(len) as usize;
    order[next]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Viewport {
    repos: Rect,
    status: Rect,
    details: Rect,
    queue: Rect,
    prompt: Rect,
}

impl Viewport {
    pub fn split(area: Rect) -> Self {
        if area.width < 50 || area.height < 16 {
            return Self::compact(area);
        }

        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(8),
                Constraint::Length(5),
                Constraint::Length(3),
            ])
            .split(area);
        let top = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(24),
                Constraint::Percentage(34),
                Constraint::Percentage(42),
            ])
            .split(vertical[0]);

        Self {
            repos: top[0],
            status: top[1],
            details: top[2],
            queue: vertical[1],
            prompt: vertical[2],
        }
    }

    fn compact(area: Rect) -> Self {
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(35),
                Constraint::Percentage(35),
                Constraint::Min(3),
                Constraint::Length(3),
            ])
            .split(area);
        Self {
            repos: vertical[0],
            status: vertical[1],
            details: vertical[2],
            queue: Rect::default(),
            prompt: vertical[3],
        }
    }

    fn focus_at(&self, column: u16, row: u16) -> Option<Focus> {
        [
            (Focus::Repos, self.repos),
            (Focus::Status, self.status),
            (Focus::Details, self.details),
            (Focus::Queue, self.queue),
            (Focus::Prompt, self.prompt),
        ]
        .into_iter()
        .find_map(|(focus, rect)| rect_contains(rect, column, row).then_some(focus))
    }
}

fn render(app: &App, frame: &mut ratatui::Frame<'_>, areas: &Viewport) {
    frame.render_widget(Clear, frame.area());
    frame.render_widget(repo_list(app), areas.repos);
    frame.render_widget(status_panel(app, areas.status), areas.status);
    frame.render_widget(details_panel(app), areas.details);
    if areas.queue.area() > 0 {
        frame.render_widget(queue_panel(app), areas.queue);
    }
    frame.render_widget(prompt_panel(app), areas.prompt);
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileRow {
    path: std::path::PathBuf,
    pathspecs: Vec<std::path::PathBuf>,
    label: String,
    section: FileSection,
}

impl FileRow {
    fn from_entry(entry: &StatusEntry) -> Vec<Self> {
        if entry.entry_type == StatusEntryType::Conflict {
            return vec![Self::new(entry, FileSection::Conflict)];
        }
        if entry.entry_type == StatusEntryType::Untracked {
            return vec![Self::new(entry, FileSection::Untracked)];
        }
        if entry.entry_type == StatusEntryType::Ignored {
            return vec![Self::new(entry, FileSection::Ignored)];
        }

        let mut rows = Vec::new();
        if entry.index != ChangeKind::Unmodified {
            rows.push(Self::new(entry, FileSection::Staged));
        }
        if entry.worktree != ChangeKind::Unmodified {
            rows.push(Self::new(entry, FileSection::Unstaged));
        }
        rows
    }

    fn new(entry: &StatusEntry, section: FileSection) -> Self {
        let label = format!(
            "{} {} {}",
            section.marker(),
            change_label(entry, section),
            path_label(entry)
        );
        Self {
            path: entry.path.clone(),
            pathspecs: pathspecs_for_entry(entry),
            label,
            section,
        }
    }

    fn can_stage(&self) -> bool {
        matches!(
            self.section,
            FileSection::Unstaged | FileSection::Untracked | FileSection::Conflict
        )
    }

    fn can_unstage(&self) -> bool {
        self.section == FileSection::Staged
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FileSection {
    Conflict,
    Staged,
    Unstaged,
    Untracked,
    Ignored,
}

impl FileSection {
    fn marker(self) -> &'static str {
        match self {
            Self::Conflict => "UU",
            Self::Staged => "S ",
            Self::Unstaged => "M ",
            Self::Untracked => "??",
            Self::Ignored => "!!",
        }
    }
}

fn change_label(entry: &StatusEntry, section: FileSection) -> &'static str {
    match section {
        FileSection::Conflict => "U",
        FileSection::Staged => change_kind_label(entry.index),
        FileSection::Unstaged => change_kind_label(entry.worktree),
        FileSection::Untracked => "?",
        FileSection::Ignored => "!",
    }
}

fn change_kind_label(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Unmodified => ".",
        ChangeKind::Modified => "M",
        ChangeKind::Added => "A",
        ChangeKind::Deleted => "D",
        ChangeKind::Renamed => "R",
        ChangeKind::Copied => "C",
        ChangeKind::Unmerged => "U",
        ChangeKind::Untracked => "?",
        ChangeKind::Ignored => "!",
        ChangeKind::Unknown(_code) => "?",
    }
}

fn path_label(entry: &StatusEntry) -> String {
    match &entry.original_path {
        Some(original_path) => format!(
            "{} -> {}",
            original_path.to_string_lossy(),
            entry.path.to_string_lossy()
        ),
        None => entry.path.to_string_lossy().into_owned(),
    }
}

fn pathspecs_for_entry(entry: &StatusEntry) -> Vec<std::path::PathBuf> {
    match (&entry.entry_type, &entry.original_path) {
        (StatusEntryType::Renamed, Some(original_path)) => {
            vec![original_path.clone(), entry.path.clone()]
        }
        _ => vec![entry.path.clone()],
    }
}

fn repo_list(app: &App) -> List<'_> {
    let items = app
        .repos
        .iter()
        .enumerate()
        .map(|(index, repo)| {
            let marker = if index == app.selected_repo {
                "> "
            } else {
                "  "
            };
            ListItem::new(Line::from(format!("{marker}{repo}")))
        })
        .collect::<Vec<_>>();

    List::new(items).block(panel_block("Repos", app.focus == Focus::Repos))
}

fn status_panel(app: &App, area: Rect) -> Paragraph<'_> {
    let visible_len = status_visible_len(area);
    let lines = if app.files.is_empty() {
        vec![Line::from("working tree clean or unavailable")]
    } else {
        app.files
            .iter()
            .enumerate()
            .skip(app.file_scroll)
            .take(visible_len)
            .map(|(index, file)| {
                let marker = if index == app.selected_file {
                    "> "
                } else {
                    "  "
                };
                Line::from(format!("{marker}{}", file.label))
            })
            .collect::<Vec<_>>()
    };
    Paragraph::new(lines)
        .block(panel_block("Status", app.focus == Focus::Status))
        .wrap(Wrap { trim: true })
}

fn status_visible_len(area: Rect) -> usize {
    area.height.saturating_sub(2).max(1) as usize
}

fn details_panel(app: &App) -> Paragraph<'_> {
    Paragraph::new(app.details.clone())
        .block(panel_block("Details", app.focus == Focus::Details))
        .wrap(Wrap { trim: true })
}

fn queue_panel(app: &App) -> Paragraph<'_> {
    let text = vec![
        Line::from("No queued operations."),
        Line::from("Tab cycles focus. q exits."),
    ];
    Paragraph::new(text)
        .block(panel_block("Queue", app.focus == Focus::Queue))
        .wrap(Wrap { trim: true })
}

fn prompt_panel(app: &App) -> Paragraph<'_> {
    let prompt = if app.prompt.is_empty() {
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::styled(
                "type a guarded command later; q exits now",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::raw(app.prompt.clone()),
        ])
    };

    Paragraph::new(prompt).block(panel_block("Prompt", app.focus == Focus::Prompt))
}

fn panel_block(title: &'static str, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(style)
}

fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    rect.area() > 0
        && column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn current_dir() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_error| std::path::PathBuf::from("."))
}

fn operation_message(action: &str, result: Result<(), String>) -> String {
    match result {
        Ok(()) => format!("{action} succeeded"),
        Err(error) => format!("{action} failed: {error}"),
    }
}

fn run_audited_git_operation(
    action: &str,
    operation: &str,
    run: impl FnOnce() -> Result<GitOutput, GitError>,
) -> String {
    let audit = match begin_audit_operation(operation) {
        Ok(audit) => audit,
        Err(error) => return operation_message(action, Err(error)),
    };
    let result = run();
    operation_message(action, audit.finish(&result))
}

fn begin_audit_operation(operation: &str) -> Result<PendingAudit, String> {
    let paths = StorePaths::from_environment()
        .map_err(|error| format!("audit failed before operation: {error}"))?;
    begin_audit_operation_with_paths(operation, paths)
}

fn begin_audit_operation_with_paths(
    operation: &str,
    paths: StorePaths,
) -> Result<PendingAudit, String> {
    let store = LocalStore::open(paths)
        .map_err(|error| format!("audit failed before operation: {error}"))?;
    let entry = AuditEntry::new(None, operation, "started", "pending")
        .map_err(|error| format!("audit failed before operation: {error}"))?;
    store
        .append_audit(entry)
        .map_err(|error| format!("audit failed before operation: {error}"))?;
    Ok(PendingAudit {
        operation: operation.to_owned(),
        store,
    })
}

struct PendingAudit {
    operation: String,
    store: LocalStore,
}

impl PendingAudit {
    fn finish(self, result: &Result<GitOutput, GitError>) -> Result<(), String> {
        let result_label = if result.is_ok() { "ok" } else { "error" };
        let message = operation_result_message(result);
        let entry = AuditEntry::new(None, self.operation, result_label, message.clone())
            .map_err(|error| format!("{message}; audit failed: {error}"))?;
        self.store
            .append_audit(entry)
            .map_err(|error| format!("{message}; audit failed: {error}"))?;
        result
            .as_ref()
            .map(|_output| ())
            .map_err(ToString::to_string)
    }
}

fn operation_result_message(result: &Result<GitOutput, GitError>) -> String {
    match result {
        Ok(_output) => "completed".to_owned(),
        Err(error) => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyModifiers, MouseEvent};
    use ratatui::backend::TestBackend;

    #[test]
    fn tab_cycles_visible_focus() {
        let mut app = App::new();
        app.last_viewport = Viewport::split(Rect::new(0, 0, 120, 40));
        assert_eq!(app.focus(), Focus::Repos);

        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus(), Focus::Status);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus(), Focus::Details);
    }

    #[test]
    fn compact_focus_cycle_skips_hidden_queue() {
        let mut app = App::new();
        app.last_viewport = Viewport::split(Rect::new(0, 0, 40, 12));

        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));

        assert_eq!(app.focus(), Focus::Prompt);
    }

    #[test]
    fn compact_resize_moves_hidden_queue_focus_to_prompt() {
        let mut app = App::new();
        let compact = Viewport::split(Rect::new(0, 0, 40, 12));
        app.focus = Focus::Queue;

        app.ensure_visible_focus(&compact);

        assert_eq!(app.focus(), Focus::Prompt);
    }

    #[test]
    fn prompt_accepts_text_only_when_focused() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('x')));
        assert!(app.prompt.is_empty());

        app.focus = Focus::Prompt;
        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.prompt, "x");
    }

    #[test]
    fn escape_requests_clean_exit() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Esc));

        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_c_requests_clean_exit() {
        let mut app = App::new();
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(app.should_quit);
    }

    #[test]
    fn q_is_text_when_prompt_is_focused() {
        let mut app = App::new();
        app.focus = Focus::Prompt;
        app.handle_key(key(KeyCode::Char('q')));

        assert_eq!(app.prompt, "q");
        assert!(!app.should_quit);
    }

    #[test]
    fn key_release_events_do_not_change_prompt() {
        let mut app = App::new();
        app.focus = Focus::Prompt;
        app.handle_event(Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::empty(),
            KeyEventKind::Release,
        )));

        assert!(app.prompt.is_empty());
    }

    #[test]
    fn key_repeat_events_are_handled() {
        let mut app = App::new();
        app.focus = Focus::Prompt;
        app.handle_event(Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::empty(),
            KeyEventKind::Repeat,
        )));

        assert_eq!(app.prompt, "x");
    }

    #[test]
    fn modified_prompt_chords_are_ignored() {
        let mut app = App::new();
        app.focus = Focus::Prompt;
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));

        assert!(app.prompt.is_empty());
    }

    #[test]
    fn prompt_input_is_bounded() {
        let mut app = App::new();
        app.focus = Focus::Prompt;
        for _ in 0..MAX_PROMPT_LEN + 10 {
            app.handle_key(key(KeyCode::Char('x')));
        }

        assert_eq!(app.prompt.len(), MAX_PROMPT_LEN);
    }

    #[test]
    fn prompt_cap_accounts_for_multibyte_chars() {
        let mut app = App::new();
        app.focus = Focus::Prompt;
        for _ in 0..MAX_PROMPT_LEN - 1 {
            app.handle_key(key(KeyCode::Char('x')));
        }
        app.handle_key(key(KeyCode::Char('\u{00e9}')));

        assert_eq!(app.prompt.len(), MAX_PROMPT_LEN - 1);
    }

    #[test]
    fn viewport_uses_desktop_panels_when_roomy() {
        let viewport = Viewport::split(Rect::new(0, 0, 120, 40));

        assert!(viewport.repos.width > 0);
        assert!(viewport.status.width > 0);
        assert!(viewport.details.width > 0);
        assert!(viewport.queue.height > 0);
        assert!(viewport.prompt.height > 0);
    }

    #[test]
    fn viewport_compacts_for_small_terminals() {
        let viewport = Viewport::split(Rect::new(0, 0, 40, 12));

        assert_eq!(viewport.queue.area(), 0);
        assert!(viewport.prompt.height > 0);
    }

    #[test]
    fn partial_file_creates_staged_and_unstaged_rows() {
        let entry = StatusEntry {
            path: std::path::PathBuf::from("file.txt"),
            original_path: None,
            index: ChangeKind::Modified,
            worktree: ChangeKind::Modified,
            entry_type: StatusEntryType::Ordinary,
        };

        let rows = FileRow::from_entry(&entry);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].section, FileSection::Staged);
        assert_eq!(rows[1].section, FileSection::Unstaged);
        assert!(!rows[0].can_stage());
        assert!(rows[0].can_unstage());
        assert!(rows[1].can_stage());
        assert!(!rows[1].can_unstage());
    }

    #[test]
    fn rename_and_delete_rows_show_change_details() {
        let renamed = StatusEntry {
            path: std::path::PathBuf::from("new.txt"),
            original_path: Some(std::path::PathBuf::from("old.txt")),
            index: ChangeKind::Renamed,
            worktree: ChangeKind::Unmodified,
            entry_type: StatusEntryType::Renamed,
        };
        let deleted = StatusEntry {
            path: std::path::PathBuf::from("deleted.txt"),
            original_path: None,
            index: ChangeKind::Unmodified,
            worktree: ChangeKind::Deleted,
            entry_type: StatusEntryType::Ordinary,
        };

        let renamed_rows = FileRow::from_entry(&renamed);
        let deleted_rows = FileRow::from_entry(&deleted);

        assert!(renamed_rows[0].label.contains("R old.txt -> new.txt"));
        assert_eq!(renamed_rows[0].pathspecs.len(), 2);
        assert_eq!(
            renamed_rows[0].pathspecs[0],
            std::path::PathBuf::from("old.txt")
        );
        assert_eq!(
            renamed_rows[0].pathspecs[1],
            std::path::PathBuf::from("new.txt")
        );
        assert!(deleted_rows[0].label.contains("D deleted.txt"));
    }

    #[test]
    fn status_window_follows_selected_file() {
        let mut app = App::new();
        app.focus = Focus::Status;
        app.last_viewport.status = Rect::new(0, 0, 30, 5);
        let visible_len = status_visible_len(app.last_viewport.status);
        app.files = (0..visible_len + 5)
            .map(|index| FileRow {
                path: std::path::PathBuf::from(format!("file-{index}.txt")),
                pathspecs: vec![std::path::PathBuf::from(format!("file-{index}.txt"))],
                label: format!("M file-{index}.txt"),
                section: FileSection::Unstaged,
            })
            .collect();

        for _ in 0..visible_len + 4 {
            app.handle_key(key(KeyCode::Down));
        }

        assert_eq!(app.selected_file, visible_len + 4);
        assert_eq!(app.file_scroll, 5);
        assert!(app.selected_file < app.file_scroll + visible_len);
    }

    #[test]
    fn status_window_reclamps_after_resize() {
        let mut app = App::new();
        app.focus = Focus::Status;
        app.last_viewport.status = Rect::new(0, 0, 30, 10);
        app.files = (0..10)
            .map(|index| FileRow {
                path: std::path::PathBuf::from(format!("file-{index}.txt")),
                pathspecs: vec![std::path::PathBuf::from(format!("file-{index}.txt"))],
                label: format!("M file-{index}.txt"),
                section: FileSection::Unstaged,
            })
            .collect();
        app.selected_file = 7;
        app.clamp_file_scroll();

        app.clamp_file_scroll_for(Rect::new(0, 0, 30, 3));

        assert_eq!(app.file_scroll, 7);
    }

    #[test]
    fn stage_all_requires_confirmation() {
        let mut app = App::new();
        app.focus = Focus::Status;

        app.handle_key(key(KeyCode::Char('a')));

        assert_eq!(app.pending_confirmation, Some(PendingAction::StageAll));
        assert!(app.details.contains("Stage all changes?"));

        app.handle_key(key(KeyCode::Char('n')));

        assert_eq!(app.pending_confirmation, None);
        assert_eq!(app.details, "Operation cancelled.");
    }

    #[test]
    fn unrelated_key_cancels_pending_confirmation() {
        let mut app = App::new();
        app.focus = Focus::Status;

        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Down));

        assert_eq!(app.pending_confirmation, None);
        assert_eq!(app.details, "Operation cancelled.");
    }

    #[test]
    fn stage_operations_are_audited() -> Result<(), Box<dyn Error>> {
        let paths = isolated_store_paths("stage-audit")?;
        let output = std::process::Command::new("git")
            .arg("--version")
            .output()?;
        let result: Result<GitOutput, GitError> = Ok(GitOutput {
            status: output.status,
            stdout: String::from_utf8(output.stdout)?,
            stderr: String::from_utf8(output.stderr)?,
        });

        for operation in ["stage_path", "unstage_path", "stage_all", "unstage_all"] {
            let audit = begin_audit_operation_with_paths(operation, paths.clone())?;
            audit.finish(&result)?;
        }

        let entries = LocalStore::open(paths)?.list_audit_entries()?;
        assert_eq!(entries.len(), 8);
        assert_eq!(entries[0].operation, "stage_path");
        assert_eq!(entries[0].result, "started");
        assert_eq!(entries[1].operation, "stage_path");
        assert_eq!(entries[1].result, "ok");
        assert_eq!(entries[2].operation, "unstage_path");
        assert_eq!(entries[3].operation, "unstage_path");
        assert_eq!(entries[4].operation, "stage_all");
        assert_eq!(entries[5].operation, "stage_all");
        assert_eq!(entries[6].operation, "unstage_all");
        assert_eq!(entries[7].operation, "unstage_all");
        assert!(
            entries
                .iter()
                .step_by(2)
                .all(|entry| entry.result == "started")
        );
        assert!(
            entries
                .iter()
                .skip(1)
                .step_by(2)
                .all(|entry| entry.result == "ok")
        );
        assert!(
            entries
                .iter()
                .skip(1)
                .step_by(2)
                .all(|entry| entry.message == "completed")
        );
        Ok(())
    }

    #[test]
    fn failed_stage_operations_are_audited() -> Result<(), Box<dyn Error>> {
        let paths = isolated_store_paths("failed-stage-audit")?;
        let result = Git::new("/definitely/not/a/bitbygit/repo").stage_all();

        let audit = begin_audit_operation_with_paths("stage_all", paths.clone())?;
        let audit_result = audit.finish(&result);

        assert!(result.is_err());
        assert!(audit_result.is_err());
        let entries = LocalStore::open(paths)?.list_audit_entries()?;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].operation, "stage_all");
        assert_eq!(entries[0].result, "started");
        assert_eq!(entries[1].operation, "stage_all");
        assert_eq!(entries[1].result, "error");
        assert_ne!(entries[1].message, "completed");
        Ok(())
    }

    #[test]
    fn renders_desktop_viewport() -> Result<(), Box<dyn Error>> {
        render_with_test_backend(120, 40)
    }

    #[test]
    fn renders_compact_viewport() -> Result<(), Box<dyn Error>> {
        render_with_test_backend(40, 12)
    }

    #[test]
    fn renders_tiny_viewport() -> Result<(), Box<dyn Error>> {
        render_with_test_backend(20, 8)
    }

    #[test]
    fn mouse_click_sets_focus_from_last_viewport() {
        let mut app = App::new();
        app.last_viewport = Viewport::split(Rect::new(0, 0, 100, 30));
        app.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.last_viewport.prompt.x + 1,
            row: app.last_viewport.prompt.y + 1,
            modifiers: KeyModifiers::empty(),
        }));

        assert_eq!(app.focus(), Focus::Prompt);
    }

    #[test]
    fn mouse_click_selects_repo_row() {
        let mut app = App::new();
        app.last_viewport = Viewport::split(Rect::new(0, 0, 100, 30));
        app.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.last_viewport.repos.x + 1,
            row: app.last_viewport.repos.y + 2,
            modifiers: KeyModifiers::empty(),
        }));

        assert_eq!(app.focus(), Focus::Repos);
        assert_eq!(app.selected_repo, 1);
    }

    #[test]
    fn mouse_click_on_repo_border_does_not_select_repo() {
        let mut app = App::new();
        app.selected_repo = 2;
        app.last_viewport = Viewport::split(Rect::new(0, 0, 100, 30));
        app.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.last_viewport.repos.x,
            row: app.last_viewport.repos.y,
            modifiers: KeyModifiers::empty(),
        }));

        assert_eq!(app.selected_repo, 2);
    }

    #[test]
    fn mouse_click_on_repo_side_border_does_not_select_repo() {
        let mut app = App::new();
        app.selected_repo = 2;
        app.last_viewport = Viewport::split(Rect::new(0, 0, 100, 30));
        app.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.last_viewport.repos.x,
            row: app.last_viewport.repos.y + 2,
            modifiers: KeyModifiers::empty(),
        }));

        assert_eq!(app.selected_repo, 2);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn render_with_test_backend(width: u16, height: u16) -> Result<(), Box<dyn Error>> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend)?;
        let mut app = App::new();

        terminal.draw(|frame| {
            let viewport = Viewport::split(frame.area());
            app.ensure_visible_focus(&viewport);
            render(&app, frame, &viewport);
        })?;

        Ok(())
    }

    fn isolated_store_paths(name: &str) -> Result<StorePaths, Box<dyn Error>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("bitbygit-tui-{name}-{}-{now}", std::process::id()));
        Ok(StorePaths::from_roots(
            root.join("config"),
            root.join("data"),
        ))
    }
}
