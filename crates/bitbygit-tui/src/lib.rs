use std::error::Error;
use std::io::{self, Stdout};
use std::path::PathBuf;
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

use bitbygit_core::{
    ConfirmationRequirement, OperationKind, OperationPlan, OperationRequest, OperationStep,
    RiskLevel,
    prompt_parser::{ParsedPrompt, parse_prompt},
};
use bitbygit_gh::{CreatePullRequest, GhError, GitHub};
use bitbygit_git::{
    BranchInfo, BranchKind, BranchState, BranchTarget, ChangeKind, Git, GitError, GitOutput, Head,
    HeadTarget, StatusEntry, StatusEntryType,
};
use bitbygit_store::{AuditEntry, LocalStore, RepoId, StorePaths};

const MAX_PROMPT_LEN: usize = 512;
const MAX_AUDIT_MESSAGE_LEN: usize = 512;
const AUDIT_SECRET_MARKERS: &[&str] = &[
    "authorization",
    "credential",
    "github_pat_",
    "gho_",
    "ghp_",
    "glpat-",
    "oauth",
    "passwd",
    "password",
    "secret",
    "token",
];

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
    branch: Option<BranchState>,
    selected_file: usize,
    file_scroll: usize,
    details: String,
    operation_queue: OperationQueue,
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
            branch: None,
            selected_file: 0,
            file_scroll: 0,
            details: "No repository status loaded yet.".to_owned(),
            operation_queue: OperationQueue::default(),
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
        if let Some(requirement) = self
            .operation_queue
            .pending()
            .map(|operation| operation.plan.confirmation.requirement)
        {
            match key.code {
                KeyCode::Char('Y')
                    if requirement == ConfirmationRequirement::ExplicitConfirmation =>
                {
                    self.confirm_pending();
                }
                KeyCode::Char('y')
                    if requirement == ConfirmationRequirement::ExplicitConfirmation =>
                {
                    self.details =
                        "Explicit confirmation required: press uppercase Y to confirm or n to cancel."
                            .to_owned();
                }
                KeyCode::Char('y') if requirement != ConfirmationRequirement::Blocked => {
                    self.confirm_pending();
                }
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
                self.submit_operation_request(OperationRequest::StageAll);
            }
            KeyCode::Char('A') if self.focus == Focus::Status => {
                self.submit_operation_request(OperationRequest::UnstageAll);
            }
            KeyCode::Char('s') if self.focus == Focus::Status => self.stage_selected_file(),
            KeyCode::Char('u') if self.focus == Focus::Status => self.unstage_selected_file(),
            KeyCode::Enter if self.focus == Focus::Prompt => self.submit_prompt(),
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
            Event::Mouse(mouse)
                if self.operation_queue.has_pending()
                    && mouse.kind == MouseEventKind::Down(MouseButton::Left) =>
            {
                self.cancel_pending();
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
                self.branch = Some(status.branch);
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
                self.branch = None;
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
        let pathspecs = file.pathspecs.clone();
        self.submit_prepared_operation(OperationPlanner::current().plan_stage_pathspecs(pathspecs));
    }

    fn unstage_selected_file(&mut self) {
        let Some(file) = self.files.get(self.selected_file).cloned() else {
            return;
        };
        if !file.can_unstage() {
            self.details = "Selected row has no staged changes to unstage.".to_owned();
            return;
        }
        let pathspecs = file.pathspecs.clone();
        self.submit_prepared_operation(
            OperationPlanner::current().plan_unstage_pathspecs(pathspecs),
        );
    }

    fn submit_operation_request(&mut self, request: OperationRequest) {
        match OperationPlanner::current().plan_request(request) {
            Ok(operation) => self.submit_prepared_operation(operation),
            Err(error) => self.details = error,
        }
    }

    fn submit_prepared_operation(&mut self, operation: PreparedOperation) {
        self.submit_operation(operation.plan, operation.context);
    }

    fn submit_operation(&mut self, plan: OperationPlan, context: ExecutionContext) {
        if plan.confirmation.requirement == ConfirmationRequirement::NormalSelection {
            self.execute_operation(plan, context);
        } else {
            self.queue_operation(plan, context);
        }
    }

    fn queue_operation(&mut self, plan: OperationPlan, context: ExecutionContext) {
        self.details = plan.preview_text();
        self.operation_queue
            .enqueue(QueuedOperation::new(plan, context));
    }

    fn execute_operation(&mut self, plan: OperationPlan, context: ExecutionContext) {
        let message = execute_typed_plan(&plan, context);
        if should_refresh_status_after(&plan) {
            self.refresh_status();
        }
        self.details = message;
    }

    fn confirm_pending(&mut self) {
        let Some(operation) = self.operation_queue.take_pending() else {
            return;
        };
        if let Some(sequence) = operation.sequence {
            self.execute_prompt_sequence(sequence);
        } else {
            self.execute_operation(operation.plan, operation.context);
        }
    }

    fn execute_prompt_sequence(&mut self, sequence: QueuedPromptSequence) {
        let result = PromptSequenceExecutor::current().execute(sequence);
        if result.should_refresh_status() {
            self.refresh_status();
        }
        self.details = result.message();
    }

    fn submit_prompt(&mut self) {
        let parsed = match parse_prompt(&self.prompt) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.details = error.to_string();
                return;
            }
        };
        match parsed {
            ParsedPrompt::Single(request) => {
                match OperationPlanner::current().plan_request(request) {
                    Ok(operation) => {
                        self.prompt.clear();
                        self.submit_prepared_operation(operation);
                    }
                    Err(error) => self.details = error,
                }
            }
            ParsedPrompt::Sequence(requests) => {
                match OperationPlanner::current().plan_prompt_sequence(requests) {
                    Ok(sequence) => {
                        self.prompt.clear();
                        self.details = sequence.plan.preview_text();
                        self.operation_queue.enqueue(QueuedOperation::new_sequence(
                            sequence.plan,
                            sequence.sequence,
                        ));
                    }
                    Err(error) => self.details = error,
                }
            }
        }
    }

    fn cancel_pending(&mut self) {
        self.operation_queue.clear();
        self.details = "Operation cancelled.".to_owned();
    }

    fn clamp_file_scroll(&mut self) {
        self.clamp_file_scroll_for(self.last_viewport.status);
    }

    fn clamp_file_scroll_for(&mut self, area: Rect) {
        let visible_len = status_file_visible_len(area);
        if visible_len == 0 {
            self.file_scroll = self.selected_file;
            return;
        }
        if self.selected_file < self.file_scroll {
            self.file_scroll = self.selected_file;
        }
        let window_end = self.file_scroll.saturating_add(visible_len);
        if self.selected_file >= window_end {
            self.file_scroll = self.selected_file.saturating_sub(visible_len - 1);
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct OperationQueue {
    pending: Option<QueuedOperation>,
}

impl OperationQueue {
    fn pending(&self) -> Option<&QueuedOperation> {
        self.pending.as_ref()
    }

    fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn enqueue(&mut self, operation: QueuedOperation) {
        self.pending = Some(operation);
    }

    fn take_pending(&mut self) -> Option<QueuedOperation> {
        self.pending.take()
    }

    fn clear(&mut self) {
        self.pending = None;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueuedOperation {
    plan: OperationPlan,
    context: ExecutionContext,
    sequence: Option<QueuedPromptSequence>,
}

impl QueuedOperation {
    fn new(plan: OperationPlan, context: ExecutionContext) -> Self {
        Self {
            plan,
            context,
            sequence: None,
        }
    }

    fn new_sequence(plan: OperationPlan, sequence: QueuedPromptSequence) -> Self {
        Self {
            plan,
            context: ExecutionContext::default(),
            sequence: Some(sequence),
        }
    }

    #[cfg(test)]
    fn with_payload(plan: OperationPlan, payload: PendingPayload) -> Self {
        Self::new(plan, ExecutionContext::from_payload(payload))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedOperation {
    plan: OperationPlan,
    context: ExecutionContext,
}

impl PreparedOperation {
    fn new(plan: OperationPlan, context: ExecutionContext) -> Self {
        Self { plan, context }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedPromptSequence {
    plan: OperationPlan,
    sequence: QueuedPromptSequence,
}

impl PreparedPromptSequence {
    fn new(plan: OperationPlan, sequence: QueuedPromptSequence) -> Self {
        Self { plan, sequence }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueuedPromptSequence {
    first: PreparedOperation,
    remaining_requests: Vec<OperationRequest>,
}

impl QueuedPromptSequence {
    fn new(first: PreparedOperation, remaining_requests: Vec<OperationRequest>) -> Self {
        Self {
            first,
            remaining_requests,
        }
    }

    fn total_steps(&self) -> usize {
        1 + self.remaining_requests.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingPayload {
    StagePaths {
        paths: Vec<PathBuf>,
    },
    UnstagePaths {
        paths: Vec<PathBuf>,
    },
    StageAll,
    UnstageAll,
    Push {
        local_branch: String,
        target: HeadTarget,
        remote: String,
        upstream_branch: String,
        upstream: String,
        expected_remote_oid: Option<String>,
        remote_urls: Vec<String>,
    },
    PushSetUpstream {
        remote: String,
        branch: String,
        target: HeadTarget,
        expected_remote_oid: Option<String>,
        remote_urls: Vec<String>,
    },
    Pull {
        local_branch: String,
        target: HeadTarget,
        upstream: String,
        remote: String,
        upstream_branch: String,
        upstream_oid: Option<String>,
    },
    PullRebase {
        local_branch: String,
        target: HeadTarget,
        upstream: String,
        remote: String,
        upstream_branch: String,
        upstream_oid: Option<String>,
    },
    Commit {
        staged_items: Vec<String>,
        staged_tree: String,
        target: HeadTarget,
    },
    Checkout {
        branch: BranchTarget,
        target: HeadTarget,
    },
    CreateBranch {
        branch: String,
        base: Option<BranchTarget>,
        target: HeadTarget,
    },
    Merge {
        branch: BranchTarget,
        target: HeadTarget,
    },
    Rebase {
        base: BranchTarget,
        target: HeadTarget,
    },
    OpenPullRequest {
        branch: String,
        upstream: String,
        remote: String,
        remote_urls: Vec<String>,
        target: HeadTarget,
        base: String,
        title: String,
        repository: String,
        github_executable: Option<PathBuf>,
    },
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
            pathspecs: pathspecs_for_entry(entry, section),
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

fn pathspecs_for_entry(entry: &StatusEntry, section: FileSection) -> Vec<std::path::PathBuf> {
    match (&entry.entry_type, &entry.original_path, section) {
        (StatusEntryType::Renamed, Some(original_path), FileSection::Staged) => {
            vec![original_path.clone(), entry.path.clone()]
        }
        _ => vec![entry.path.clone()],
    }
}

fn staged_plan_items(entries: &[StatusEntry]) -> Vec<String> {
    let mut items = entries
        .iter()
        .flat_map(FileRow::from_entry)
        .filter(|row| row.section == FileSection::Staged)
        .map(|row| row.label)
        .collect::<Vec<_>>();
    items.sort();
    items
}

fn file_path_labels(paths: &[std::path::PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn stage_paths_plan(paths: Vec<String>) -> OperationPlan {
    let summary = match paths.as_slice() {
        [path] => format!("stage {path}"),
        _ => format!("stage {} path(s)", paths.len()),
    };
    OperationPlan::new(
        OperationRequest::StagePaths { paths },
        "Stage plan",
        vec![OperationStep::new(
            OperationKind::StagePaths,
            RiskLevel::Low,
            summary,
        )],
        "",
    )
}

fn unstage_paths_plan(paths: Vec<String>) -> OperationPlan {
    let summary = match paths.as_slice() {
        [path] => format!("unstage {path}"),
        _ => format!("unstage {} path(s)", paths.len()),
    };
    OperationPlan::new(
        OperationRequest::UnstagePaths { paths },
        "Unstage plan",
        vec![OperationStep::new(
            OperationKind::UnstagePaths,
            RiskLevel::Low,
            summary,
        )],
        "",
    )
}

fn stage_all_plan() -> OperationPlan {
    OperationPlan::new(
        OperationRequest::StageAll,
        "Stage all plan",
        vec![OperationStep::new(
            OperationKind::StageAll,
            RiskLevel::Medium,
            "stage all working tree changes",
        )],
        "Press y to stage all changes or n to cancel.",
    )
}

fn unstage_all_plan() -> OperationPlan {
    OperationPlan::new(
        OperationRequest::UnstageAll,
        "Unstage all plan",
        vec![OperationStep::new(
            OperationKind::UnstageAll,
            RiskLevel::Medium,
            "unstage all staged changes",
        )],
        "Press y to unstage all changes or n to cancel.",
    )
}

fn fetch_plan() -> OperationPlan {
    OperationPlan::new(
        OperationRequest::Fetch,
        "Fetch plan",
        vec![OperationStep::new(
            OperationKind::Fetch,
            RiskLevel::Low,
            "fetch default remote",
        )],
        "",
    )
}

fn branches_plan() -> OperationPlan {
    OperationPlan::new(
        OperationRequest::Branches,
        "Branches plan",
        vec![OperationStep::new(
            OperationKind::Branches,
            RiskLevel::Low,
            "list local and remote branches",
        )],
        "",
    )
}

fn commit_plan(message: &str, staged_count: usize) -> OperationPlan {
    OperationPlan::new(
        OperationRequest::Commit {
            message: message.to_owned(),
        },
        "Commit plan",
        vec![
            OperationStep::new(
                OperationKind::Commit,
                RiskLevel::Medium,
                format!("commit {staged_count} staged file(s)"),
            )
            .with_detail(format!("message: {message}")),
        ],
        "Press y to commit or n to cancel.",
    )
}

fn push_plan(branch: &str, upstream: &str, ahead: u32) -> OperationPlan {
    OperationPlan::new(
        OperationRequest::Push,
        "Push plan",
        vec![
            OperationStep::new(
                OperationKind::PushCurrentBranch,
                RiskLevel::Medium,
                format!("push {branch} to {upstream}"),
            )
            .with_detail(format!("ahead: {ahead} commit(s)")),
        ],
        "Press y to push or n to cancel.",
    )
}

fn push_set_upstream_plan(branch: &str, remote: &str) -> OperationPlan {
    OperationPlan::new(
        OperationRequest::Push,
        "Push plan",
        vec![
            OperationStep::new(
                OperationKind::PushSetUpstream,
                RiskLevel::Medium,
                format!("push {branch} to {remote}"),
            )
            .with_detail(format!("set upstream to {remote}/{branch}")),
        ],
        "Press y to push or n to cancel.",
    )
}

fn pull_plan(rebase: bool, upstream: &str, behind: u32) -> OperationPlan {
    if rebase {
        OperationPlan::new(
            OperationRequest::Pull { rebase: true },
            "Pull rebase plan",
            vec![
                OperationStep::new(
                    OperationKind::PullRebase,
                    RiskLevel::High,
                    format!("rebase current branch onto fetched {upstream}"),
                )
                .with_detail("block if the fetched upstream changes before confirmation")
                .with_detail(format!("locally behind: {behind} commit(s)")),
            ],
            "Explicit confirmation required: press uppercase Y to rebase or n to cancel.",
        )
    } else {
        OperationPlan::new(
            OperationRequest::Pull { rebase: false },
            "Pull plan",
            vec![
                OperationStep::new(
                    OperationKind::PullFastForward,
                    RiskLevel::Medium,
                    format!("fast-forward from fetched {upstream}"),
                )
                .with_detail("block if the fetched upstream changes before confirmation")
                .with_detail(format!("locally behind: {behind} commit(s)")),
            ],
            "Press y to pull or n to cancel.",
        )
    }
}

fn checkout_plan(branch: &BranchTarget) -> OperationPlan {
    OperationPlan::new(
        OperationRequest::Checkout {
            branch: branch.name.clone(),
        },
        "Checkout plan",
        vec![
            OperationStep::new(
                OperationKind::CheckoutBranch,
                RiskLevel::Medium,
                format!("switch to {} at {}", branch.name, short_oid(&branch.oid)),
            )
            .with_detail("block if the working tree or branch target changes"),
        ],
        "Press y to checkout or n to cancel.",
    )
}

fn create_branch_plan(
    branch: &str,
    base: Option<&BranchTarget>,
    base_label: &str,
) -> OperationPlan {
    OperationPlan::new(
        OperationRequest::CreateBranch {
            branch: branch.to_owned(),
            base: base.map(|base| base.name.clone()),
        },
        "Create branch plan",
        vec![
            OperationStep::new(
                OperationKind::CreateBranch,
                RiskLevel::Medium,
                format!("create and switch to {branch}"),
            )
            .with_detail(format!("base: {base_label}"))
            .with_detail("block if the working tree, current target, or base changes"),
        ],
        "Press y to create branch or n to cancel.",
    )
}

fn merge_plan(current: &str, branch: &BranchTarget) -> OperationPlan {
    OperationPlan::new(
        OperationRequest::Merge {
            branch: branch.name.clone(),
        },
        "Merge plan",
        vec![
            OperationStep::new(
                OperationKind::MergeFastForward,
                RiskLevel::Medium,
                format!(
                    "fast-forward {current} to {} at {}",
                    branch.name,
                    short_oid(&branch.oid)
                ),
            )
            .with_detail("merge commits and conflicts are out of scope for this guarded action"),
        ],
        "Press y to merge or n to cancel.",
    )
}

fn rebase_plan(current: &str, base: &BranchTarget) -> OperationPlan {
    OperationPlan::new(
        OperationRequest::Rebase {
            base: base.name.clone(),
        },
        "Rebase plan",
        vec![
            OperationStep::new(
                OperationKind::Rebase,
                RiskLevel::High,
                format!(
                    "rebase {current} onto {} at {}",
                    base.name,
                    short_oid(&base.oid)
                ),
            )
            .with_detail("block if the working tree, current target, or base changes")
            .with_detail("Git may stop for conflicts that require manual resolution"),
        ],
        "Explicit confirmation required: press uppercase Y to rebase or n to cancel.",
    )
}

fn open_pull_request_plan(
    request: OperationRequest,
    remote: &str,
    head: &str,
    base: &str,
    title: &str,
    repository: &str,
    existing_url: Option<&str>,
) -> OperationPlan {
    let target = existing_url.map(ToOwned::to_owned).unwrap_or_else(|| {
        format!("https://github.com/{repository}/compare/{base}...{head}?expand=1")
    });
    let summary = if existing_url.is_some() {
        format!("surface existing pull request for {head}")
    } else {
        format!("open pull request from {head} to {base}")
    };
    OperationPlan::new(
        request,
        "Open pull request plan",
        vec![
            OperationStep::new(OperationKind::OpenPullRequest, RiskLevel::Medium, summary)
                .with_detail("provider: GitHub")
                .with_detail(format!("remote: {remote}"))
                .with_detail(format!("head: {head}"))
                .with_detail(format!("base: {base}"))
                .with_detail(format!("title: {title}"))
                .with_detail(format!("target: {target}"))
                .with_detail(
                    "revalidate branch, upstream, remote, and pull request state before execution",
                ),
        ],
        "Press y to open or surface the pull request or n to cancel.",
    )
}

fn prompt_sequence_plan(
    requests: &[OperationRequest],
    first: &PreparedOperation,
) -> Result<OperationPlan, String> {
    let Some(first_step) = first.plan.first_step() else {
        return Err("Prompt sequence blocked: first step produced no visible plan.".to_owned());
    };
    let mut steps = vec![prompt_sequence_first_step(1, first_step)];
    for (index, request) in requests.iter().enumerate().skip(1) {
        steps.push(prompt_sequence_deferred_step(index + 1, request)?);
    }
    let confirmation_prompt = prompt_sequence_confirmation_prompt(&steps, requests.len());
    Ok(OperationPlan::new(
        OperationRequest::PromptSequence {
            requests: requests.to_vec(),
        },
        "Prompt sequence plan",
        steps,
        confirmation_prompt,
    ))
}

fn prompt_sequence_first_step(index: usize, step: &OperationStep) -> OperationStep {
    let mut preview = OperationStep::new(
        step.kind,
        sequence_step_risk(step.risk_level),
        format!("{index}. {}", step.summary),
    );
    for detail in &step.details {
        preview = preview.with_detail(detail.clone());
    }
    preview
}

fn prompt_sequence_deferred_step(
    index: usize,
    request: &OperationRequest,
) -> Result<OperationStep, String> {
    let preview = prompt_sequence_request_preview(request)?;
    let mut step = OperationStep::new(
        preview.kind,
        sequence_step_risk(preview.risk_level),
        format!("{index}. {}", preview.summary),
    );
    for detail in preview.details {
        step = step.with_detail(detail);
    }
    Ok(step.with_detail(format!(
        "planned after step {} succeeds",
        index.saturating_sub(1)
    )))
}

fn prompt_sequence_confirmation_prompt(steps: &[OperationStep], step_count: usize) -> String {
    let risk_level = steps
        .iter()
        .map(|step| step.risk_level)
        .max()
        .unwrap_or(RiskLevel::Medium);
    if risk_level >= RiskLevel::High {
        format!(
            "Explicit confirmation required: press uppercase Y to run {step_count} prompt steps or n to cancel."
        )
    } else {
        format!("Press y to run {step_count} prompt steps or n to cancel.")
    }
}

fn sequence_step_risk(risk_level: RiskLevel) -> RiskLevel {
    risk_level.max(RiskLevel::Medium)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptSequenceStepPreview {
    kind: OperationKind,
    risk_level: RiskLevel,
    summary: String,
    details: Vec<String>,
}

impl PromptSequenceStepPreview {
    fn new(kind: OperationKind, risk_level: RiskLevel, summary: impl Into<String>) -> Self {
        Self {
            kind,
            risk_level,
            summary: summary.into(),
            details: Vec::new(),
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.details.push(detail.into());
        self
    }
}

fn prompt_sequence_request_preview(
    request: &OperationRequest,
) -> Result<PromptSequenceStepPreview, String> {
    match request {
        OperationRequest::Fetch => Ok(PromptSequenceStepPreview::new(
            OperationKind::Fetch,
            RiskLevel::Low,
            "fetch default remote",
        )),
        OperationRequest::Branches => Ok(PromptSequenceStepPreview::new(
            OperationKind::Branches,
            RiskLevel::Low,
            "list local and remote branches",
        )),
        OperationRequest::Commit { message } => Ok(PromptSequenceStepPreview::new(
            OperationKind::Commit,
            RiskLevel::Medium,
            "commit staged changes",
        )
        .with_detail(format!("message: {message}"))),
        OperationRequest::Push => Ok(PromptSequenceStepPreview::new(
            OperationKind::PushCurrentBranch,
            RiskLevel::Medium,
            "push current branch",
        )),
        OperationRequest::Pull { rebase: true } => Ok(PromptSequenceStepPreview::new(
            OperationKind::PullRebase,
            RiskLevel::High,
            "rebase current branch onto upstream",
        )),
        OperationRequest::Pull { rebase: false } => Ok(PromptSequenceStepPreview::new(
            OperationKind::PullFastForward,
            RiskLevel::Medium,
            "fast-forward from upstream",
        )),
        OperationRequest::Checkout { branch } => Ok(PromptSequenceStepPreview::new(
            OperationKind::CheckoutBranch,
            RiskLevel::Medium,
            format!("checkout {branch}"),
        )),
        OperationRequest::CreateBranch { branch, base } => {
            let preview = PromptSequenceStepPreview::new(
                OperationKind::CreateBranch,
                RiskLevel::Medium,
                format!("create and switch to {branch}"),
            );
            Ok(match base {
                Some(base) => preview.with_detail(format!("base: {base}")),
                None => preview,
            })
        }
        OperationRequest::Merge { branch } => Ok(PromptSequenceStepPreview::new(
            OperationKind::MergeFastForward,
            RiskLevel::Medium,
            format!("fast-forward current branch to {branch}"),
        )),
        OperationRequest::Rebase { base } => Ok(PromptSequenceStepPreview::new(
            OperationKind::Rebase,
            RiskLevel::High,
            format!("rebase current branch onto {base}"),
        )),
        OperationRequest::OpenPullRequest { .. } => {
            Err("open pull request is only available as a single prompt".to_owned())
        }
        OperationRequest::RefreshStatus
        | OperationRequest::ViewDiff { .. }
        | OperationRequest::StagePaths { .. }
        | OperationRequest::UnstagePaths { .. }
        | OperationRequest::StageAll
        | OperationRequest::UnstageAll
        | OperationRequest::PromptSequence { .. } => {
            Err("unsupported request in prompt sequence".to_owned())
        }
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
    let visible_len = status_file_visible_len(area);
    let mut lines = vec![Line::from(branch_summary(app.branch.as_ref()))];
    if app.files.is_empty() {
        lines.push(Line::from("working tree clean or unavailable"));
    } else {
        lines.extend(
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
                }),
        );
    }

    Paragraph::new(lines)
        .block(panel_block("Status", app.focus == Focus::Status))
        .wrap(Wrap { trim: true })
}

fn branch_summary(branch: Option<&BranchState>) -> String {
    let Some(branch) = branch else {
        return "branch unavailable".to_owned();
    };
    let head = match &branch.head {
        Head::Branch(name) => name.as_str(),
        Head::Detached(oid) => oid.as_str(),
        Head::Unborn => "unborn",
    };
    let upstream = branch.upstream.as_deref().unwrap_or("no upstream");
    format!(
        "{head} -> {upstream} | ahead {} behind {}",
        branch.ahead, branch.behind
    )
}

fn branch_name(branch: &BranchState) -> Result<String, String> {
    match &branch.head {
        Head::Branch(name) if !branch.unborn => Ok(name.clone()),
        Head::Branch(_) | Head::Unborn => {
            Err("Operation blocked: unborn branch needs an initial commit first.".to_owned())
        }
        Head::Detached(_oid) => Err("Operation blocked: HEAD is detached.".to_owned()),
    }
}

fn short_oid(oid: &str) -> String {
    oid.chars().take(12).collect()
}

#[derive(Debug, Clone)]
struct OperationPlanner {
    repo_root: PathBuf,
    github_executable: Option<PathBuf>,
}

impl OperationPlanner {
    fn current() -> Self {
        Self {
            repo_root: current_dir(),
            github_executable: None,
        }
    }

    #[cfg(test)]
    fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
            github_executable: None,
        }
    }

    fn plan_request(&self, request: OperationRequest) -> Result<PreparedOperation, String> {
        match request {
            OperationRequest::RefreshStatus => {
                Err("Refresh status is handled directly by the UI.".to_owned())
            }
            OperationRequest::ViewDiff { .. } => {
                Err("View diff is handled directly by the UI.".to_owned())
            }
            OperationRequest::StagePaths { paths } => {
                Ok(self.plan_stage_pathspecs(paths.into_iter().map(PathBuf::from).collect()))
            }
            OperationRequest::UnstagePaths { paths } => {
                Ok(self.plan_unstage_pathspecs(paths.into_iter().map(PathBuf::from).collect()))
            }
            OperationRequest::StageAll => Ok(PreparedOperation::new(
                stage_all_plan(),
                ExecutionContext::from_payload(PendingPayload::StageAll),
            )),
            OperationRequest::UnstageAll => Ok(PreparedOperation::new(
                unstage_all_plan(),
                ExecutionContext::from_payload(PendingPayload::UnstageAll),
            )),
            OperationRequest::Commit { message } => self.plan_commit(message),
            OperationRequest::Fetch => Ok(PreparedOperation::new(
                fetch_plan(),
                ExecutionContext::default(),
            )),
            OperationRequest::Push => self.plan_push(),
            OperationRequest::Pull { rebase } => self.plan_pull(rebase),
            OperationRequest::Branches => Ok(PreparedOperation::new(
                branches_plan(),
                ExecutionContext::default(),
            )),
            OperationRequest::Checkout { branch } => self.plan_checkout(branch),
            OperationRequest::CreateBranch { branch, base } => {
                self.plan_create_branch(branch, base)
            }
            OperationRequest::Merge { branch } => self.plan_merge(branch),
            OperationRequest::Rebase { base } => self.plan_rebase(base),
            OperationRequest::OpenPullRequest { base } => self.plan_open_pull_request(base),
            OperationRequest::PromptSequence { .. } => {
                Err("Prompt sequences are handled by prompt submission.".to_owned())
            }
        }
    }

    fn plan_prompt_sequence(
        &self,
        requests: Vec<OperationRequest>,
    ) -> Result<PreparedPromptSequence, String> {
        if requests.len() < 2 {
            return Err("Prompt sequence requires at least two steps.".to_owned());
        }
        for (index, request) in requests.iter().enumerate() {
            prompt_sequence_request_preview(request).map_err(|error| {
                format!("Prompt sequence step {} is blocked: {error}", index + 1)
            })?;
        }
        let first_request = requests
            .first()
            .cloned()
            .ok_or_else(|| "Prompt sequence requires at least two steps.".to_owned())?;
        let first = self.plan_request(first_request)?;
        let plan = prompt_sequence_plan(&requests, &first)?;
        let remaining_requests = requests.into_iter().skip(1).collect();
        Ok(PreparedPromptSequence::new(
            plan,
            QueuedPromptSequence::new(first, remaining_requests),
        ))
    }

    fn plan_stage_pathspecs(&self, paths: Vec<PathBuf>) -> PreparedOperation {
        PreparedOperation::new(
            stage_paths_plan(file_path_labels(&paths)),
            ExecutionContext::from_payload(PendingPayload::StagePaths { paths }),
        )
    }

    fn plan_unstage_pathspecs(&self, paths: Vec<PathBuf>) -> PreparedOperation {
        PreparedOperation::new(
            unstage_paths_plan(file_path_labels(&paths)),
            ExecutionContext::from_payload(PendingPayload::UnstagePaths { paths }),
        )
    }

    fn plan_commit(&self, message: String) -> Result<PreparedOperation, String> {
        let git = self.git();
        let status = git
            .status()
            .map_err(|error| format!("Unable to prepare commit plan: {error}"))?;
        let staged_items = staged_plan_items(&status.entries);
        if staged_items.is_empty() {
            return Err("Commit blocked: there are no staged changes.".to_owned());
        }
        let staged_tree = git
            .staged_tree()
            .map_err(|error| format!("Unable to snapshot staged content: {error}"))?;
        let target = git
            .head_target()
            .map_err(|error| format!("Unable to snapshot commit target: {error}"))?;
        let staged_count = staged_items.len();

        Ok(PreparedOperation::new(
            commit_plan(&message, staged_count),
            ExecutionContext::from_payload(PendingPayload::Commit {
                staged_items,
                staged_tree,
                target,
            }),
        ))
    }

    fn plan_push(&self) -> Result<PreparedOperation, String> {
        let git = self.git();
        let status = git
            .status()
            .map_err(|error| format!("Unable to prepare push plan: {error}"))?;
        let branch = branch_name(&status.branch)?;
        if status.branch.behind > 0 {
            return Err("Push blocked: branch is behind its upstream. Pull or resolve divergence before pushing.".to_owned());
        }
        let head_target = git
            .head_target()
            .map_err(|error| format!("Unable to snapshot push target: {error}"))?;
        let ahead = status.branch.ahead;

        match status.branch.upstream {
            Some(upstream) => {
                let (remote, upstream_branch) = git
                    .upstream_push_target(&branch)
                    .map_err(|error| format!("Unable to prepare push target: {error}"))?
                    .ok_or_else(|| {
                        format!("Push blocked: unable to resolve upstream {upstream}.")
                    })?;
                let expected_upstream = format!("{remote}/{upstream_branch}");
                if expected_upstream != upstream {
                    return Err(format!(
                        "Push blocked: upstream config does not match {upstream}."
                    ));
                }
                let remote_urls = git
                    .remote_push_urls(&remote)
                    .map_err(|error| format!("Unable to snapshot push remote URLs: {error}"))?;
                let push_url = single_push_url(&remote, &remote_urls)?;
                let expected_remote_oid = git
                    .remote_url_head_oid(push_url, &upstream_branch)
                    .map_err(|error| format!("Unable to snapshot push remote: {error}"))?;
                Ok(PreparedOperation::new(
                    push_plan(&branch, &upstream, ahead),
                    ExecutionContext::from_payload(PendingPayload::Push {
                        local_branch: branch,
                        target: head_target,
                        remote,
                        upstream_branch,
                        upstream,
                        expected_remote_oid,
                        remote_urls,
                    }),
                ))
            }
            None => {
                let Some(remote) = self.default_remote_name() else {
                    return Err("Push blocked: no remotes are configured.".to_owned());
                };
                let remote_urls = git
                    .remote_push_urls(&remote)
                    .map_err(|error| format!("Unable to snapshot push remote URLs: {error}"))?;
                let push_url = single_push_url(&remote, &remote_urls)?;
                let expected_remote_oid = git
                    .remote_url_head_oid(push_url, &branch)
                    .map_err(|error| format!("Unable to snapshot push remote: {error}"))?;
                Ok(PreparedOperation::new(
                    push_set_upstream_plan(&branch, &remote),
                    ExecutionContext::from_payload(PendingPayload::PushSetUpstream {
                        remote,
                        branch,
                        target: head_target,
                        expected_remote_oid,
                        remote_urls,
                    }),
                ))
            }
        }
    }

    fn plan_pull(&self, rebase: bool) -> Result<PreparedOperation, String> {
        let git = self.git();
        let status = git
            .status()
            .map_err(|error| format!("Unable to prepare pull plan: {error}"))?;
        let Some(upstream) = status.branch.upstream.clone() else {
            return Err("Pull blocked: current branch has no upstream.".to_owned());
        };
        let local_branch = branch_name(&status.branch)?;
        let head_target = git
            .head_target()
            .map_err(|error| format!("Unable to snapshot pull target: {error}"))?;
        let (remote, upstream_branch) = git
            .upstream_push_target(&local_branch)
            .map_err(|error| format!("Unable to prepare pull target: {error}"))?
            .ok_or_else(|| format!("Pull blocked: unable to resolve upstream {upstream}."))?;
        let expected_upstream = format!("{remote}/{upstream_branch}");
        if expected_upstream != upstream {
            return Err(format!(
                "Pull blocked: upstream config does not match {upstream}."
            ));
        }
        git.fetch_remote_branch(&remote, &upstream_branch)
            .map_err(|error| format!("Unable to fetch pull target: {error}"))?;
        let status = git
            .status()
            .map_err(|error| format!("Unable to refresh pull plan after fetch: {error}"))?;
        if branch_name(&status.branch).as_deref() != Ok(local_branch.as_str()) {
            return Err("Pull blocked: current branch changed during fetch.".to_owned());
        }
        if status.branch.upstream.as_deref() != Some(upstream.as_str()) {
            return Err("Pull blocked: upstream changed during fetch.".to_owned());
        }
        if status.branch.ahead > 0 && status.branch.behind > 0 && !rebase {
            return Err("Pull blocked: branch has diverged. Use `pull --rebase` explicitly or resolve manually.".to_owned());
        }
        if rebase && !status.is_clean() {
            return Err("Pull rebase blocked: working tree must be clean.".to_owned());
        }
        let upstream_oid = git
            .remote_tracking_oid(&remote, &upstream_branch)
            .map_err(|error| format!("Unable to snapshot pull upstream: {error}"))?;
        let plan = pull_plan(rebase, &upstream, status.branch.behind);
        let payload = if rebase {
            PendingPayload::PullRebase {
                local_branch,
                target: head_target,
                upstream,
                remote,
                upstream_branch,
                upstream_oid,
            }
        } else {
            PendingPayload::Pull {
                local_branch,
                target: head_target,
                upstream,
                remote,
                upstream_branch,
                upstream_oid,
            }
        };
        Ok(PreparedOperation::new(
            plan,
            ExecutionContext::from_payload(payload),
        ))
    }

    fn plan_checkout(&self, branch: String) -> Result<PreparedOperation, String> {
        self.ensure_clean_branch_worktree("Checkout")?;
        let git = self.git();
        let target = git
            .head_target()
            .map_err(|error| format!("Unable to snapshot checkout target: {error}"))?;
        let branch_target = git
            .branch_target(&branch)
            .map_err(|error| format!("Unable to prepare checkout plan: {error}"))?
            .ok_or_else(|| format!("Checkout blocked: branch {branch} was not found."))?;
        git.ensure_remote_checkout_target_available(&branch_target)
            .map_err(|error| format!("Checkout blocked: {error}"))?;
        Ok(PreparedOperation::new(
            checkout_plan(&branch_target),
            ExecutionContext::from_payload(PendingPayload::Checkout {
                branch: branch_target,
                target,
            }),
        ))
    }

    fn plan_create_branch(
        &self,
        branch: String,
        base: Option<String>,
    ) -> Result<PreparedOperation, String> {
        self.ensure_clean_branch_worktree("Create branch")?;
        let git = self.git();
        let target = git
            .head_target()
            .map_err(|error| format!("Unable to snapshot branch target: {error}"))?;
        if git
            .branch_target(&branch)
            .map_err(|error| format!("Unable to prepare branch plan: {error}"))?
            .is_some()
        {
            return Err(format!("Create branch blocked: {branch} already exists."));
        }
        let base_target = match base {
            Some(base) => Some(
                git.branch_target(&base)
                    .map_err(|error| format!("Unable to prepare branch base: {error}"))?
                    .ok_or_else(|| format!("Create branch blocked: base {base} was not found."))?,
            ),
            None => None,
        };
        if base_target.is_none() && target.oid.is_none() {
            return Err(
                "Create branch blocked: current branch needs a commit before branching.".to_owned(),
            );
        }
        let base_label = base_target
            .as_ref()
            .map(|base| format!("{} at {}", base.name, short_oid(&base.oid)))
            .or_else(|| {
                target
                    .oid
                    .as_ref()
                    .map(|oid| format!("HEAD at {}", short_oid(oid)))
            })
            .unwrap_or_else(|| "unborn HEAD".to_owned());
        Ok(PreparedOperation::new(
            create_branch_plan(&branch, base_target.as_ref(), &base_label),
            ExecutionContext::from_payload(PendingPayload::CreateBranch {
                branch,
                base: base_target,
                target,
            }),
        ))
    }

    fn plan_merge(&self, branch: String) -> Result<PreparedOperation, String> {
        let current = self.current_branch_for_branch_operation("Merge")?;
        let git = self.git();
        let target = git
            .head_target()
            .map_err(|error| format!("Unable to snapshot merge target: {error}"))?;
        let branch_target = git
            .branch_target(&branch)
            .map_err(|error| format!("Unable to prepare merge plan: {error}"))?
            .ok_or_else(|| format!("Merge blocked: branch {branch} was not found."))?;
        if branch_target.name == current {
            return Err("Merge blocked: selected branch is already checked out.".to_owned());
        }
        Ok(PreparedOperation::new(
            merge_plan(&current, &branch_target),
            ExecutionContext::from_payload(PendingPayload::Merge {
                branch: branch_target,
                target,
            }),
        ))
    }

    fn plan_rebase(&self, base: String) -> Result<PreparedOperation, String> {
        let current = self.current_branch_for_branch_operation("Rebase")?;
        let git = self.git();
        let target = git
            .head_target()
            .map_err(|error| format!("Unable to snapshot rebase target: {error}"))?;
        let base_target = git
            .branch_target(&base)
            .map_err(|error| format!("Unable to prepare rebase plan: {error}"))?
            .ok_or_else(|| format!("Rebase blocked: base {base} was not found."))?;
        if base_target.name == current {
            return Err("Rebase blocked: selected base is already checked out.".to_owned());
        }
        Ok(PreparedOperation::new(
            rebase_plan(&current, &base_target),
            ExecutionContext::from_payload(PendingPayload::Rebase {
                base: base_target,
                target,
            }),
        ))
    }

    fn plan_open_pull_request(
        &self,
        requested_base: Option<String>,
    ) -> Result<PreparedOperation, String> {
        let git = self.git();
        let status = git
            .status()
            .map_err(|error| format!("Unable to prepare pull request plan: {error}"))?;
        let branch = branch_name(&status.branch)?;
        let Some(upstream) = status.branch.upstream.clone() else {
            return Err("Open pull request blocked: current branch has no upstream. Push it with `push` first.".to_owned());
        };
        if status.branch.ahead > 0 || status.branch.behind > 0 {
            return Err("Open pull request blocked: current branch is not fully pushed and up to date. Push or pull it first.".to_owned());
        }
        let (remote, upstream_branch) = git
            .upstream_push_target(&branch)
            .map_err(|error| format!("Unable to prepare pull request target: {error}"))?
            .ok_or_else(|| {
                format!("Open pull request blocked: unable to resolve upstream {upstream}.")
            })?;
        if upstream != format!("{remote}/{upstream_branch}") || upstream_branch != branch {
            return Err(
                "Open pull request blocked: current branch must track a same-named remote branch."
                    .to_owned(),
            );
        }
        let remote_urls = git
            .remote_push_urls(&remote)
            .map_err(|error| format!("Unable to prepare pull request remote: {error}"))?;
        let push_url = single_pull_request_push_url(&remote, &remote_urls)?;
        let target = git
            .head_target()
            .map_err(|error| format!("Unable to snapshot pull request branch: {error}"))?;
        let local_oid = target.oid.as_deref().ok_or_else(|| {
            "Open pull request blocked: current branch needs a commit before opening a pull request."
                .to_owned()
        })?;
        let remote_oid = git
            .remote_url_head_oid(push_url, &branch)
            .map_err(|error| format!("Unable to verify pushed branch: {error}"))?;
        if remote_oid.as_deref() != Some(local_oid) {
            return Err("Open pull request blocked: current branch is not pushed to its upstream. Push it with `push` first.".to_owned());
        }
        let github = self.github();
        let repository = github.repository().map_err(open_pull_request_gh_error)?;
        let base = requested_base.unwrap_or(repository.default_branch);
        if base == branch {
            return Err(
                "Open pull request blocked: base branch must differ from the current branch."
                    .to_owned(),
            );
        }
        let existing = github
            .existing_pull_requests(&branch)
            .map_err(open_pull_request_gh_error)?;
        let existing_url = existing
            .first()
            .map(|pull_request| pull_request.url.as_str());
        let title = branch.clone();
        let request = OperationRequest::OpenPullRequest {
            base: Some(base.clone()),
        };
        Ok(PreparedOperation::new(
            open_pull_request_plan(
                request,
                &remote,
                &branch,
                &base,
                &title,
                &repository.name_with_owner,
                existing_url,
            ),
            ExecutionContext::from_payload(PendingPayload::OpenPullRequest {
                branch,
                upstream,
                remote,
                remote_urls,
                target,
                base,
                title,
                repository: repository.name_with_owner,
                github_executable: self.github_executable.clone(),
            }),
        ))
    }

    fn ensure_clean_branch_worktree(&self, action: &str) -> Result<(), String> {
        self.git()
            .ensure_clean_worktree(&action.to_ascii_lowercase())
            .map_err(|error| format!("{action} blocked: {error}"))
    }

    fn current_branch_for_branch_operation(&self, action: &str) -> Result<String, String> {
        self.ensure_clean_branch_worktree(action)?;
        let status = self.git().status().map_err(|error| {
            format!("{action} blocked: unable to read repository status: {error}")
        })?;
        branch_name(&status.branch).map_err(|error| error.replace("Operation", action))
    }

    fn default_remote_name(&self) -> Option<String> {
        let remotes = self.git().remotes().ok()?;
        remotes
            .iter()
            .find(|remote| remote.name == "origin")
            .or_else(|| remotes.first())
            .map(|remote| remote.name.clone())
    }

    fn git(&self) -> Git {
        Git::new(self.repo_root.clone())
    }

    fn github(&self) -> GitHub {
        match &self.github_executable {
            Some(executable) => GitHub::with_executable(&self.repo_root, executable),
            None => GitHub::new(&self.repo_root),
        }
    }
}

fn open_pull_request_gh_error(error: GhError) -> String {
    format!("Open pull request blocked: {error}")
}

fn single_push_url<'a>(remote: &str, urls: &'a [String]) -> Result<&'a str, String> {
    match urls {
        [url] => Ok(url),
        [] => Err(format!(
            "Push blocked: unable to resolve push URL for remote {remote}."
        )),
        _ => Err(format!(
            "Push blocked: remote {remote} has multiple push URLs; push from bitbygit supports one destination at a time."
        )),
    }
}

fn single_pull_request_push_url<'a>(remote: &str, urls: &'a [String]) -> Result<&'a str, String> {
    match urls {
        [url] => Ok(url),
        [] => Err(format!(
            "Open pull request blocked: remote {remote} has no push URL."
        )),
        _ => Err(format!(
            "Open pull request blocked: remote {remote} has multiple push URLs."
        )),
    }
}

fn validate_push_plan(
    git: &Git,
    branch: &str,
    expected_upstream: Option<&str>,
    target: &HeadTarget,
    remote: &str,
    remote_urls: &[String],
) -> Result<(), String> {
    if git
        .head_target()
        .map_err(|error| format!("Unable to revalidate push target: {error}"))?
        != *target
    {
        return Err("Push blocked: branch target changed since the plan was shown.".to_owned());
    }
    let status = git
        .status()
        .map_err(|error| format!("Unable to revalidate push plan: {error}"))?;
    let current_branch = branch_name(&status.branch)?;
    if current_branch != branch {
        return Err("Push blocked: current branch changed since the plan was shown.".to_owned());
    }
    if status.branch.upstream.as_deref() != expected_upstream {
        return Err("Push blocked: upstream changed since the plan was shown.".to_owned());
    }
    let current_remote_urls = git
        .remote_push_urls(remote)
        .map_err(|error| format!("Unable to revalidate push remote URLs: {error}"))?;
    if current_remote_urls != remote_urls {
        return Err("Push blocked: remote URLs changed since the plan was shown.".to_owned());
    }
    single_push_url(remote, &current_remote_urls)?;
    if status.branch.behind > 0 {
        return Err("Push blocked: branch is now behind its upstream.".to_owned());
    }
    Ok(())
}

fn validate_pull_plan(
    git: &Git,
    rebase: bool,
    branch: &str,
    expected_upstream: &str,
    target: &HeadTarget,
) -> Result<(), String> {
    if git
        .head_target()
        .map_err(|error| format!("Unable to revalidate pull target: {error}"))?
        != *target
    {
        return Err("Pull blocked: branch target changed since the plan was shown.".to_owned());
    }
    let status = git
        .status()
        .map_err(|error| format!("Unable to revalidate pull plan: {error}"))?;
    if branch_name(&status.branch)? != branch {
        return Err("Pull blocked: current branch changed since the plan was shown.".to_owned());
    }
    if status.branch.upstream.as_deref() != Some(expected_upstream) {
        return Err("Pull blocked: upstream changed since the plan was shown.".to_owned());
    }
    if status.branch.ahead > 0 && status.branch.behind > 0 && !rebase {
        return Err("Pull blocked: branch has diverged. Use `pull --rebase` explicitly or resolve manually.".to_owned());
    }
    if rebase && !status.is_clean() {
        return Err("Pull rebase blocked: working tree must be clean.".to_owned());
    }
    Ok(())
}

fn status_visible_len(area: Rect) -> usize {
    area.height.saturating_sub(2).max(1) as usize
}

fn validate_open_pull_request_plan(
    git: &Git,
    branch: &str,
    expected_upstream: &str,
    remote: &str,
    remote_urls: &[String],
    target: &HeadTarget,
) -> Result<(), String> {
    if git
        .head_target()
        .map_err(|error| format!("Unable to revalidate pull request target: {error}"))?
        != *target
    {
        return Err(
            "Open pull request blocked: branch target changed since the plan was shown.".to_owned(),
        );
    }
    let status = git
        .status()
        .map_err(|error| format!("Unable to revalidate pull request plan: {error}"))?;
    if branch_name(&status.branch)? != branch {
        return Err(
            "Open pull request blocked: current branch changed since the plan was shown."
                .to_owned(),
        );
    }
    if status.branch.upstream.as_deref() != Some(expected_upstream) {
        return Err(
            "Open pull request blocked: upstream changed since the plan was shown.".to_owned(),
        );
    }
    if status.branch.ahead > 0 || status.branch.behind > 0 {
        return Err(
            "Open pull request blocked: current branch is no longer fully pushed and up to date."
                .to_owned(),
        );
    }
    let current_target = git
        .upstream_push_target(branch)
        .map_err(|error| format!("Unable to revalidate pull request upstream: {error}"))?;
    if current_target
        .as_ref()
        .map(|(name, branch)| format!("{name}/{branch}"))
        != Some(expected_upstream.to_owned())
    {
        return Err(
            "Open pull request blocked: upstream target changed since the plan was shown."
                .to_owned(),
        );
    }
    let current_remote_urls = git
        .remote_push_urls(remote)
        .map_err(|error| format!("Unable to revalidate pull request remote: {error}"))?;
    if current_remote_urls != remote_urls {
        return Err(
            "Open pull request blocked: remote URLs changed since the plan was shown.".to_owned(),
        );
    }
    let push_url = single_pull_request_push_url(remote, &current_remote_urls)?;
    let local_oid = target
        .oid
        .as_deref()
        .ok_or_else(|| "Open pull request blocked: planned branch has no commit.".to_owned())?;
    let remote_oid = git
        .remote_url_head_oid(push_url, branch)
        .map_err(|error| format!("Unable to revalidate pushed branch: {error}"))?;
    if remote_oid.as_deref() != Some(local_oid) {
        return Err(
            "Open pull request blocked: current branch is no longer pushed to its upstream."
                .to_owned(),
        );
    }
    Ok(())
}

fn status_file_visible_len(area: Rect) -> usize {
    status_visible_len(area).saturating_sub(1)
}

fn details_panel(app: &App) -> Paragraph<'_> {
    Paragraph::new(app.details.clone())
        .block(panel_block("Details", app.focus == Focus::Details))
        .wrap(Wrap { trim: true })
}

fn queue_panel(app: &App) -> Paragraph<'_> {
    let text = queue_panel_text(app.operation_queue.pending())
        .into_iter()
        .map(Line::from)
        .collect::<Vec<_>>();
    Paragraph::new(text)
        .block(panel_block("Queue", app.focus == Focus::Queue))
        .wrap(Wrap { trim: true })
}

fn queue_panel_text(pending: Option<&QueuedOperation>) -> Vec<String> {
    let Some(operation) = pending else {
        return vec![
            "No queued operations.".to_owned(),
            "Plans that need confirmation will appear here.".to_owned(),
        ];
    };
    let plan = &operation.plan;
    vec![
        format!(
            "Pending: {} | Risk: {}",
            plan.title,
            risk_label(plan.confirmation.risk_level)
        ),
        format!("Steps: {}", plan_steps_summary(plan)),
        format!("Confirm: {}", confirmation_copy(plan)),
    ]
}

fn plan_steps_summary(plan: &OperationPlan) -> String {
    plan.steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let details = if step.details.is_empty() {
                String::new()
            } else {
                format!(" ({})", step.details.join("; "))
            };
            format!("{}. {}{}", index + 1, step.summary, details)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn confirmation_copy(plan: &OperationPlan) -> String {
    if !plan.confirmation.prompt.is_empty() {
        return plan.confirmation.prompt.clone();
    }
    match plan.confirmation.requirement {
        ConfirmationRequirement::NormalSelection => "Runs after normal selection.".to_owned(),
        ConfirmationRequirement::VisiblePlan => "Press y to confirm or n to cancel.".to_owned(),
        ConfirmationRequirement::ExplicitConfirmation => {
            "Press uppercase Y to confirm or n to cancel.".to_owned()
        }
        ConfirmationRequirement::Blocked => "Blocked by policy.".to_owned(),
    }
}

fn risk_label(risk: RiskLevel) -> &'static str {
    match risk {
        RiskLevel::Low => "low",
        RiskLevel::Medium => "medium",
        RiskLevel::High => "high",
        RiskLevel::BlockedByDefault => "blocked by default",
    }
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

fn execute_typed_plan(plan: &OperationPlan, context: ExecutionContext) -> String {
    PlanExecutor::current().execute(plan, context).message()
}

fn should_refresh_status_after(plan: &OperationPlan) -> bool {
    !matches!(plan.request, OperationRequest::Branches)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ExecutionContext {
    payload: Option<PendingPayload>,
}

impl ExecutionContext {
    fn from_payload(payload: PendingPayload) -> Self {
        Self {
            payload: Some(payload),
        }
    }
}

#[derive(Debug, Clone)]
struct PlanExecutor {
    repo_root: PathBuf,
    audit: AuditDestination,
}

impl PlanExecutor {
    fn current() -> Self {
        Self {
            repo_root: current_dir(),
            audit: AuditDestination::Environment,
        }
    }

    #[cfg(test)]
    fn with_audit_paths(repo_root: impl Into<PathBuf>, paths: StorePaths) -> Self {
        Self {
            repo_root: repo_root.into(),
            audit: AuditDestination::Paths(paths),
        }
    }

    fn execute(&self, plan: &OperationPlan, context: ExecutionContext) -> PlanExecutionResult {
        if plan.steps.is_empty() {
            return PlanExecutionResult {
                planned_step_count: 0,
                step_results: Vec::new(),
                plan_error: Some("operation plan has no steps".to_owned()),
            };
        }

        let mut step_results = Vec::new();
        for step in &plan.steps {
            let step_result = self.execute_step(plan, step, &context);
            let should_continue = step_result.succeeded() || step.continue_on_failure;
            step_results.push(step_result);
            if !should_continue {
                break;
            }
        }

        PlanExecutionResult {
            planned_step_count: plan.steps.len(),
            step_results,
            plan_error: None,
        }
    }

    fn execute_step(
        &self,
        plan: &OperationPlan,
        step: &OperationStep,
        context: &ExecutionContext,
    ) -> StepExecutionResult {
        let repo_id = self.audit_repo_id();
        let audit = match self.audit.begin(repo_id, step.kind.audit_operation()) {
            Ok(audit) => audit,
            Err(error) => {
                return StepExecutionResult {
                    kind: step.kind,
                    summary: step.summary.clone(),
                    outcome: StepExecutionOutcome::AuditStartFailed { error },
                };
            }
        };
        let result = self.run_step(plan, step, context);
        let audit_result = audit.finish(&AuditTerminalResult::from_step_result(&result));

        StepExecutionResult {
            kind: step.kind,
            summary: step.summary.clone(),
            outcome: StepExecutionOutcome::Ran {
                result,
                audit_result,
            },
        }
    }

    fn run_step(
        &self,
        plan: &OperationPlan,
        step: &OperationStep,
        context: &ExecutionContext,
    ) -> StepRunResult {
        let git = self.git();
        match step.kind {
            OperationKind::Fetch => git_output(git.fetch_default_remote()),
            OperationKind::StagePaths => {
                let paths = stage_paths_for_step(plan, context)?;
                git_output(git.stage_paths(&paths))
            }
            OperationKind::UnstagePaths => {
                let paths = unstage_paths_for_step(plan, context)?;
                git_output(git.unstage_paths(&paths))
            }
            OperationKind::StageAll => git_output(git.stage_all()),
            OperationKind::UnstageAll => git_output(git.unstage_all()),
            OperationKind::Commit => self.run_commit_step(plan, context, &git),
            OperationKind::PushCurrentBranch => self.run_push_current_branch_step(context, &git),
            OperationKind::PushSetUpstream => self.run_push_set_upstream_step(context, &git),
            OperationKind::PullFastForward => self.run_pull_fast_forward_step(plan, context, &git),
            OperationKind::PullRebase => self.run_pull_rebase_step(plan, context, &git),
            OperationKind::Branches => git
                .branches()
                .map(ExecutionOutput::Branches)
                .map_err(StepExecutionError::Git),
            OperationKind::CheckoutBranch => self.run_checkout_step(plan, context, &git),
            OperationKind::CreateBranch => self.run_create_branch_step(plan, context, &git),
            OperationKind::MergeFastForward => self.run_merge_step(plan, context, &git),
            OperationKind::Rebase => self.run_rebase_step(plan, context, &git),
            OperationKind::OpenPullRequest => self.run_open_pull_request_step(plan, context, &git),
            OperationKind::RefreshStatus | OperationKind::ViewDiff => {
                Err(StepExecutionError::Unsupported(format!(
                    "{} step is not executable by the typed operation executor",
                    step.kind.action_label()
                )))
            }
        }
    }

    fn git(&self) -> Git {
        Git::new(self.repo_root.clone())
    }

    fn audit_repo_id(&self) -> Option<RepoId> {
        self.git()
            .repo_root()
            .ok()
            .map(|root| RepoId::from_path(&root))
    }

    fn run_commit_step(
        &self,
        plan: &OperationPlan,
        context: &ExecutionContext,
        git: &Git,
    ) -> StepRunResult {
        let OperationRequest::Commit { message } = &plan.request else {
            return Err(StepExecutionError::Unsupported(
                "commit step requires a typed commit request".to_owned(),
            ));
        };
        let PendingPayload::Commit {
            staged_items,
            staged_tree,
            target,
        } = typed_payload(context, OperationKind::Commit)?
        else {
            return Err(mismatched_context(OperationKind::Commit));
        };
        let current_status = git.status().map_err(|error| {
            StepExecutionError::Blocked(format!("Unable to validate commit plan: {error}"))
        })?;
        if staged_plan_items(&current_status.entries) != *staged_items {
            return Err(StepExecutionError::Blocked("Commit blocked: staged changes changed since the plan was shown. Re-run the commit prompt.".to_owned()));
        }
        let current_tree = git.staged_tree().map_err(|error| {
            StepExecutionError::Blocked(format!("Unable to validate staged content: {error}"))
        })?;
        let current_target = git.head_target().map_err(|error| {
            StepExecutionError::Blocked(format!("Unable to validate commit target: {error}"))
        })?;
        if current_tree != *staged_tree || current_target != *target {
            return Err(StepExecutionError::Blocked("Commit blocked: repository state changed since the plan was shown. Re-run the commit prompt.".to_owned()));
        }

        git_output(git.commit_staged_tree(message, staged_tree, target))
    }

    fn run_push_current_branch_step(&self, context: &ExecutionContext, git: &Git) -> StepRunResult {
        let PendingPayload::Push {
            local_branch,
            target,
            remote,
            upstream_branch,
            upstream,
            expected_remote_oid,
            remote_urls,
        } = typed_payload(context, OperationKind::PushCurrentBranch)?
        else {
            return Err(mismatched_context(OperationKind::PushCurrentBranch));
        };
        validate_push_plan(
            git,
            local_branch,
            Some(upstream),
            target,
            remote,
            remote_urls,
        )
        .map_err(StepExecutionError::Blocked)?;
        let source_oid = target.oid.as_deref().ok_or_else(|| {
            StepExecutionError::Git(GitError::Blocked {
                message: "push is blocked because the planned branch has no commit".to_owned(),
            })
        })?;
        git_output(git.push_current_branch(
            remote,
            upstream_branch,
            source_oid,
            expected_remote_oid.as_deref(),
        ))
    }

    fn run_push_set_upstream_step(&self, context: &ExecutionContext, git: &Git) -> StepRunResult {
        let PendingPayload::PushSetUpstream {
            remote,
            branch,
            target,
            expected_remote_oid,
            remote_urls,
        } = typed_payload(context, OperationKind::PushSetUpstream)?
        else {
            return Err(mismatched_context(OperationKind::PushSetUpstream));
        };
        validate_push_plan(git, branch, None, target, remote, remote_urls)
            .map_err(StepExecutionError::Blocked)?;
        let source_oid = target.oid.as_deref().ok_or_else(|| {
            StepExecutionError::Git(GitError::Blocked {
                message: "push is blocked because the planned branch has no commit".to_owned(),
            })
        })?;
        git_output(git.push_current_branch_set_upstream(
            remote,
            branch,
            source_oid,
            expected_remote_oid.as_deref(),
        ))
    }

    fn run_pull_fast_forward_step(
        &self,
        plan: &OperationPlan,
        context: &ExecutionContext,
        git: &Git,
    ) -> StepRunResult {
        require_pull_request(plan, false)?;
        let PendingPayload::Pull {
            local_branch,
            target,
            upstream,
            remote,
            upstream_branch,
            upstream_oid,
        } = typed_payload(context, OperationKind::PullFastForward)?
        else {
            return Err(mismatched_context(OperationKind::PullFastForward));
        };
        validate_pull_plan(git, false, local_branch, upstream, target)
            .map_err(StepExecutionError::Blocked)?;
        git_output(git.pull_ff_only_from(remote, upstream_branch, upstream_oid.as_deref(), target))
    }

    fn run_pull_rebase_step(
        &self,
        plan: &OperationPlan,
        context: &ExecutionContext,
        git: &Git,
    ) -> StepRunResult {
        require_pull_request(plan, true)?;
        let PendingPayload::PullRebase {
            local_branch,
            target,
            upstream,
            remote,
            upstream_branch,
            upstream_oid,
        } = typed_payload(context, OperationKind::PullRebase)?
        else {
            return Err(mismatched_context(OperationKind::PullRebase));
        };
        validate_pull_plan(git, true, local_branch, upstream, target)
            .map_err(StepExecutionError::Blocked)?;
        git_output(git.pull_rebase_from(remote, upstream_branch, upstream_oid.as_deref(), target))
    }

    fn run_checkout_step(
        &self,
        plan: &OperationPlan,
        context: &ExecutionContext,
        git: &Git,
    ) -> StepRunResult {
        let OperationRequest::Checkout { branch } = &plan.request else {
            return Err(StepExecutionError::Unsupported(
                "checkout step requires a typed checkout request".to_owned(),
            ));
        };
        let PendingPayload::Checkout {
            branch: target,
            target: head,
        } = typed_payload(context, OperationKind::CheckoutBranch)?
        else {
            return Err(mismatched_context(OperationKind::CheckoutBranch));
        };
        if target.name != *branch {
            return Err(StepExecutionError::Unsupported(
                "checkout step request does not match its typed execution context".to_owned(),
            ));
        }
        git_output(git.checkout_branch(target, head))
    }

    fn run_create_branch_step(
        &self,
        plan: &OperationPlan,
        context: &ExecutionContext,
        git: &Git,
    ) -> StepRunResult {
        let OperationRequest::CreateBranch {
            branch: requested_branch,
            base: requested_base,
        } = &plan.request
        else {
            return Err(StepExecutionError::Unsupported(
                "create branch step requires a typed create branch request".to_owned(),
            ));
        };
        let PendingPayload::CreateBranch {
            branch,
            base,
            target,
        } = typed_payload(context, OperationKind::CreateBranch)?
        else {
            return Err(mismatched_context(OperationKind::CreateBranch));
        };
        if branch != requested_branch
            || base.as_ref().map(|base| &base.name) != requested_base.as_ref()
        {
            return Err(StepExecutionError::Unsupported(
                "create branch step request does not match its typed execution context".to_owned(),
            ));
        }
        git_output(git.create_branch(branch, base.as_ref(), target))
    }

    fn run_merge_step(
        &self,
        plan: &OperationPlan,
        context: &ExecutionContext,
        git: &Git,
    ) -> StepRunResult {
        let OperationRequest::Merge { branch } = &plan.request else {
            return Err(StepExecutionError::Unsupported(
                "merge step requires a typed merge request".to_owned(),
            ));
        };
        let PendingPayload::Merge {
            branch: target,
            target: head,
        } = typed_payload(context, OperationKind::MergeFastForward)?
        else {
            return Err(mismatched_context(OperationKind::MergeFastForward));
        };
        if target.name != *branch {
            return Err(StepExecutionError::Unsupported(
                "merge step request does not match its typed execution context".to_owned(),
            ));
        }
        git_output(git.merge_ff_only(target, head))
    }

    fn run_rebase_step(
        &self,
        plan: &OperationPlan,
        context: &ExecutionContext,
        git: &Git,
    ) -> StepRunResult {
        let OperationRequest::Rebase { base } = &plan.request else {
            return Err(StepExecutionError::Unsupported(
                "rebase step requires a typed rebase request".to_owned(),
            ));
        };
        let PendingPayload::Rebase {
            base: target,
            target: head,
        } = typed_payload(context, OperationKind::Rebase)?
        else {
            return Err(mismatched_context(OperationKind::Rebase));
        };
        if target.name != *base {
            return Err(StepExecutionError::Unsupported(
                "rebase step request does not match its typed execution context".to_owned(),
            ));
        }
        git_output(git.rebase_onto(target, head))
    }

    fn run_open_pull_request_step(
        &self,
        plan: &OperationPlan,
        context: &ExecutionContext,
        git: &Git,
    ) -> StepRunResult {
        let OperationRequest::OpenPullRequest {
            base: Some(requested_base),
        } = &plan.request
        else {
            return Err(StepExecutionError::Unsupported(
                "open pull request step requires a typed pull request request".to_owned(),
            ));
        };
        let PendingPayload::OpenPullRequest {
            branch,
            upstream,
            remote,
            remote_urls,
            target,
            base,
            title,
            repository,
            github_executable,
        } = typed_payload(context, OperationKind::OpenPullRequest)?
        else {
            return Err(mismatched_context(OperationKind::OpenPullRequest));
        };
        if base != requested_base {
            return Err(StepExecutionError::Unsupported(
                "open pull request request does not match its typed execution context".to_owned(),
            ));
        }
        validate_open_pull_request_plan(git, branch, upstream, remote, remote_urls, target)
            .map_err(StepExecutionError::Blocked)?;
        let github = match github_executable {
            Some(executable) => GitHub::with_executable(&self.repo_root, executable),
            None => GitHub::new(&self.repo_root),
        };
        let current_repository = github.repository().map_err(StepExecutionError::GitHub)?;
        if current_repository.name_with_owner != *repository {
            return Err(StepExecutionError::Blocked(
                "Open pull request blocked: GitHub repository changed since the plan was shown."
                    .to_owned(),
            ));
        }
        if let Some(existing) = github
            .existing_pull_requests(branch)
            .map_err(StepExecutionError::GitHub)?
            .into_iter()
            .next()
        {
            return Ok(ExecutionOutput::PullRequest {
                url: existing.url,
                existing: true,
            });
        }
        match github.create_pull_request(&CreatePullRequest {
            title: title.clone(),
            body: String::new(),
            base: base.clone(),
            head: branch.clone(),
        }) {
            Ok(created) => Ok(ExecutionOutput::PullRequest {
                url: created.url,
                existing: false,
            }),
            Err(error) => match github.existing_pull_requests(branch) {
                Ok(existing) => existing
                    .into_iter()
                    .next()
                    .map(|pull_request| ExecutionOutput::PullRequest {
                        url: pull_request.url,
                        existing: true,
                    })
                    .ok_or(StepExecutionError::GitHub(error)),
                Err(_) => Err(StepExecutionError::GitHub(error)),
            },
        }
    }
}

#[derive(Debug, Clone)]
struct PromptSequenceExecutor {
    repo_root: PathBuf,
    audit: AuditDestination,
}

impl PromptSequenceExecutor {
    fn current() -> Self {
        Self {
            repo_root: current_dir(),
            audit: AuditDestination::Environment,
        }
    }

    #[cfg(test)]
    fn with_audit_paths(repo_root: impl Into<PathBuf>, paths: StorePaths) -> Self {
        Self {
            repo_root: repo_root.into(),
            audit: AuditDestination::Paths(paths),
        }
    }

    fn execute(&self, sequence: QueuedPromptSequence) -> PromptSequenceExecutionResult {
        let total_steps = sequence.total_steps();
        let planner = OperationPlanner {
            repo_root: self.repo_root.clone(),
            github_executable: None,
        };
        let executor = PlanExecutor {
            repo_root: self.repo_root.clone(),
            audit: self.audit.clone(),
        };
        let mut step_results = Vec::with_capacity(total_steps);

        let first = Self::execute_prepared_step(&executor, 1, sequence.first);
        let first_succeeded = first.succeeded;
        step_results.push(first);
        if !first_succeeded {
            return PromptSequenceExecutionResult::new(total_steps, step_results);
        }

        for (index, request) in sequence.remaining_requests.into_iter().enumerate() {
            let step_number = index + 2;
            let operation = match planner.plan_request(request.clone()) {
                Ok(operation) => operation,
                Err(error) => {
                    step_results.push(PromptSequenceStepResult::planning_failed(
                        step_number,
                        prompt_sequence_request_title(&request),
                        error,
                    ));
                    return PromptSequenceExecutionResult::new(total_steps, step_results);
                }
            };
            let step = Self::execute_prepared_step(&executor, step_number, operation);
            let succeeded = step.succeeded;
            step_results.push(step);
            if !succeeded {
                return PromptSequenceExecutionResult::new(total_steps, step_results);
            }
        }

        PromptSequenceExecutionResult::new(total_steps, step_results)
    }

    fn execute_prepared_step(
        executor: &PlanExecutor,
        step_number: usize,
        operation: PreparedOperation,
    ) -> PromptSequenceStepResult {
        let title = operation.plan.title.clone();
        let refresh_status = should_refresh_status_after(&operation.plan);
        let execution = executor.execute(&operation.plan, operation.context);
        PromptSequenceStepResult::executed(
            step_number,
            title,
            execution.message(),
            execution.succeeded(),
            refresh_status,
        )
    }
}

#[derive(Debug)]
struct PromptSequenceExecutionResult {
    total_steps: usize,
    step_results: Vec<PromptSequenceStepResult>,
}

impl PromptSequenceExecutionResult {
    fn new(total_steps: usize, step_results: Vec<PromptSequenceStepResult>) -> Self {
        Self {
            total_steps,
            step_results,
        }
    }

    fn should_refresh_status(&self) -> bool {
        self.step_results.iter().any(|result| result.refresh_status)
    }

    fn message(&self) -> String {
        let failed_step = self.step_results.iter().find(|result| !result.succeeded);
        let mut lines = match failed_step {
            Some(result) if result.planned => vec![format!(
                "Prompt sequence stopped after step {} of {}.",
                result.step_number, self.total_steps
            )],
            Some(result) => vec![format!(
                "Prompt sequence stopped before step {} of {}.",
                result.step_number, self.total_steps
            )],
            None => vec![format!(
                "Prompt sequence completed {} step(s).",
                self.total_steps
            )],
        };
        lines.extend(self.step_results.iter().map(|result| {
            format!(
                "Step {} ({}): {}",
                result.step_number, result.title, result.message
            )
        }));
        lines.join("\n")
    }
}

#[derive(Debug)]
struct PromptSequenceStepResult {
    step_number: usize,
    title: String,
    message: String,
    succeeded: bool,
    refresh_status: bool,
    planned: bool,
}

impl PromptSequenceStepResult {
    fn executed(
        step_number: usize,
        title: String,
        message: String,
        succeeded: bool,
        refresh_status: bool,
    ) -> Self {
        Self {
            step_number,
            title,
            message,
            succeeded,
            refresh_status,
            planned: true,
        }
    }

    fn planning_failed(step_number: usize, title: String, error: String) -> Self {
        Self {
            step_number,
            title,
            message: format!("planning failed: {error}"),
            succeeded: false,
            refresh_status: false,
            planned: false,
        }
    }
}

fn prompt_sequence_request_title(request: &OperationRequest) -> String {
    prompt_sequence_request_preview(request)
        .map(|preview| preview.summary)
        .unwrap_or_else(|_error| "prompt step".to_owned())
}

#[derive(Debug, Clone)]
enum AuditDestination {
    Environment,
    #[cfg(test)]
    Paths(StorePaths),
}

impl AuditDestination {
    fn begin(&self, repo_id: Option<RepoId>, operation: &str) -> Result<PendingAudit, String> {
        match self {
            Self::Environment => begin_audit_operation(repo_id, operation),
            #[cfg(test)]
            Self::Paths(paths) => {
                begin_audit_operation_with_paths(repo_id, operation, paths.clone())
            }
        }
    }
}

#[derive(Debug)]
struct PlanExecutionResult {
    planned_step_count: usize,
    step_results: Vec<StepExecutionResult>,
    plan_error: Option<String>,
}

impl PlanExecutionResult {
    fn succeeded(&self) -> bool {
        self.plan_error.is_none()
            && self.step_results.len() == self.planned_step_count
            && self.step_results.iter().all(StepExecutionResult::succeeded)
    }

    fn message(&self) -> String {
        if let Some(error) = &self.plan_error {
            return operation_message("operation", Err(error.clone()));
        }
        if self.planned_step_count == 1 {
            return self
                .step_results
                .first()
                .map(StepExecutionResult::message)
                .unwrap_or_else(|| {
                    operation_message("operation", Err("operation plan has no steps".to_owned()))
                });
        }

        let failed_step = self
            .step_results
            .iter()
            .position(|step_result| !step_result.succeeded());
        let stopped_early = self.step_results.len() < self.planned_step_count;
        let mut lines = match (failed_step, stopped_early) {
            (Some(index), true) => vec![format!(
                "Operation stopped after step {} of {}.",
                index + 1,
                self.planned_step_count
            )],
            (Some(_index), false) => vec![format!(
                "Operation completed {} step(s) with failures.",
                self.step_results.len()
            )],
            (None, _) => vec![format!(
                "Operation completed {} step(s).",
                self.step_results.len()
            )],
        };
        lines.extend(
            self.step_results
                .iter()
                .enumerate()
                .map(|(index, step_result)| {
                    format!(
                        "Step {} ({}): {}",
                        index + 1,
                        step_result.summary,
                        step_result.message()
                    )
                }),
        );
        lines.join("\n")
    }
}

#[derive(Debug)]
struct StepExecutionResult {
    kind: OperationKind,
    summary: String,
    outcome: StepExecutionOutcome,
}

impl StepExecutionResult {
    fn succeeded(&self) -> bool {
        matches!(
            &self.outcome,
            StepExecutionOutcome::Ran {
                result: Ok(_),
                audit_result: Ok(())
            }
        )
    }

    fn message(&self) -> String {
        let action = self.kind.action_label();
        match &self.outcome {
            StepExecutionOutcome::AuditStartFailed { error } => {
                operation_message(action, Err(error.clone()))
            }
            StepExecutionOutcome::Ran {
                result: Ok(output),
                audit_result: Ok(()),
            } => success_message(self.kind, action, output),
            StepExecutionOutcome::Ran {
                result: Ok(output),
                audit_result: Err(error),
            } => success_with_audit_error_message(self.kind, action, output, error),
            StepExecutionOutcome::Ran {
                result: Err(error),
                audit_result: Ok(()),
            } => failure_message(self.kind, action, error, None),
            StepExecutionOutcome::Ran {
                result: Err(error),
                audit_result: Err(audit_error),
            } => failure_message(self.kind, action, error, Some(audit_error)),
        }
    }
}

#[derive(Debug)]
enum StepExecutionOutcome {
    AuditStartFailed {
        error: String,
    },
    Ran {
        result: StepRunResult,
        audit_result: Result<(), String>,
    },
}

type StepRunResult = Result<ExecutionOutput, StepExecutionError>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExecutionOutput {
    Git(GitOutput),
    Branches(Vec<BranchInfo>),
    PullRequest { url: String, existing: bool },
}

#[derive(Debug)]
enum StepExecutionError {
    Git(GitError),
    GitHub(GhError),
    Blocked(String),
    Unsupported(String),
}

impl StepExecutionError {
    fn audit_message(&self) -> String {
        match self {
            Self::Git(error) => sanitized_git_error(error),
            Self::GitHub(error) => sanitized_github_error(error),
            Self::Blocked(message) => format!("operation blocked: {message}"),
            Self::Unsupported(message) => message.clone(),
        }
    }
}

impl std::fmt::Display for StepExecutionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Git(error) => write!(formatter, "{error}"),
            Self::GitHub(error) => write!(formatter, "{error}"),
            Self::Blocked(message) | Self::Unsupported(message) => write!(formatter, "{message}"),
        }
    }
}

fn git_output(result: Result<GitOutput, GitError>) -> StepRunResult {
    result
        .map(ExecutionOutput::Git)
        .map_err(StepExecutionError::Git)
}

fn stage_paths_for_step(
    plan: &OperationPlan,
    context: &ExecutionContext,
) -> Result<Vec<PathBuf>, StepExecutionError> {
    let OperationRequest::StagePaths { paths } = &plan.request else {
        return Err(StepExecutionError::Unsupported(
            "stage step requires a typed stage paths request".to_owned(),
        ));
    };
    if let Some(payload) = &context.payload {
        let PendingPayload::StagePaths { paths } = payload else {
            return Err(mismatched_context(OperationKind::StagePaths));
        };
        return Ok(paths.clone());
    }
    Ok(paths.iter().map(PathBuf::from).collect())
}

fn unstage_paths_for_step(
    plan: &OperationPlan,
    context: &ExecutionContext,
) -> Result<Vec<PathBuf>, StepExecutionError> {
    let OperationRequest::UnstagePaths { paths } = &plan.request else {
        return Err(StepExecutionError::Unsupported(
            "unstage step requires a typed unstage paths request".to_owned(),
        ));
    };
    if let Some(payload) = &context.payload {
        let PendingPayload::UnstagePaths { paths } = payload else {
            return Err(mismatched_context(OperationKind::UnstagePaths));
        };
        return Ok(paths.clone());
    }
    Ok(paths.iter().map(PathBuf::from).collect())
}

fn require_pull_request(plan: &OperationPlan, rebase: bool) -> Result<(), StepExecutionError> {
    match &plan.request {
        OperationRequest::Pull { rebase: requested } if *requested == rebase => Ok(()),
        _ if rebase => Err(StepExecutionError::Unsupported(
            "pull rebase step requires a typed pull rebase request".to_owned(),
        )),
        _ => Err(StepExecutionError::Unsupported(
            "pull step requires a typed pull request".to_owned(),
        )),
    }
}

fn typed_payload(
    context: &ExecutionContext,
    kind: OperationKind,
) -> Result<&PendingPayload, StepExecutionError> {
    context.payload.as_ref().ok_or_else(|| {
        StepExecutionError::Unsupported(format!(
            "{} step requires typed execution context",
            kind.action_label()
        ))
    })
}

fn mismatched_context(kind: OperationKind) -> StepExecutionError {
    StepExecutionError::Unsupported(format!(
        "{} step received mismatched typed execution context",
        kind.action_label()
    ))
}

fn success_message(kind: OperationKind, action: &str, output: &ExecutionOutput) -> String {
    match output {
        ExecutionOutput::Branches(branches) => branch_list_message(branches),
        ExecutionOutput::PullRequest { url, existing } => {
            if *existing {
                format!("Pull request already open: {url}")
            } else {
                format!("Pull request created: {url}")
            }
        }
        ExecutionOutput::Git(output) => {
            let output = git_output_text(output);
            if should_show_git_output(kind) && !output.is_empty() {
                format!("{action} succeeded:\n{output}")
            } else {
                format!("{action} succeeded")
            }
        }
    }
}

fn success_with_audit_error_message(
    kind: OperationKind,
    action: &str,
    output: &ExecutionOutput,
    audit_error: &str,
) -> String {
    let success = success_message(kind, action, output);
    match output {
        ExecutionOutput::Branches(_) => {
            format!("{action} succeeded, but audit finalization failed: {audit_error}\n{success}")
        }
        ExecutionOutput::PullRequest { .. } => {
            format!("{success}, but audit finalization failed: {audit_error}")
        }
        ExecutionOutput::Git(output) => {
            let output = git_output_text(output);
            if should_show_git_output(kind) && !output.is_empty() {
                format!(
                    "{action} succeeded, but audit finalization failed: {audit_error}\n{output}"
                )
            } else {
                format!("{action} succeeded, but audit finalization failed: {audit_error}")
            }
        }
    }
}

fn failure_message(
    kind: OperationKind,
    action: &str,
    error: &StepExecutionError,
    audit_error: Option<&String>,
) -> String {
    let mut message = match (kind, error) {
        (OperationKind::Branches, StepExecutionError::Git(error)) => {
            format!("Unable to list branches: {error}")
        }
        (_, StepExecutionError::Blocked(message)) => message.clone(),
        _ => format!("{action} failed: {error}"),
    };
    if let Some(audit_error) = audit_error {
        message.push_str(&format!("; audit finalization failed: {audit_error}"));
    }
    message
}

fn should_show_git_output(kind: OperationKind) -> bool {
    matches!(
        kind,
        OperationKind::Fetch
            | OperationKind::Commit
            | OperationKind::PushCurrentBranch
            | OperationKind::PushSetUpstream
            | OperationKind::PullFastForward
            | OperationKind::PullRebase
            | OperationKind::CheckoutBranch
            | OperationKind::CreateBranch
            | OperationKind::MergeFastForward
            | OperationKind::Rebase
    )
}

fn branch_list_message(branches: &[BranchInfo]) -> String {
    if branches.is_empty() {
        return "No branches found.".to_owned();
    }
    let lines = branches
        .iter()
        .map(|branch| {
            let marker = if branch.current { "*" } else { " " };
            let kind = match branch.kind {
                BranchKind::Local => "local",
                BranchKind::Remote => "remote",
            };
            let upstream = branch
                .upstream
                .as_ref()
                .map(|upstream| format!(" -> {upstream}"))
                .unwrap_or_default();
            format!("{marker} {kind} {}{upstream}", branch.name)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("Branches:\n{lines}")
}

fn git_output_text(output: &GitOutput) -> String {
    [output.stdout.trim(), output.stderr.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuditTerminalResult {
    result_label: &'static str,
    message: String,
}

impl AuditTerminalResult {
    fn completed() -> Self {
        Self {
            result_label: "ok",
            message: "completed".to_owned(),
        }
    }

    fn error(message: String) -> Self {
        Self {
            result_label: "error",
            message: sanitize_audit_message(&message),
        }
    }

    fn from_step_result(result: &StepRunResult) -> Self {
        match result {
            Ok(_output) => Self::completed(),
            Err(error) => Self::error(error.audit_message()),
        }
    }
}

fn begin_audit_operation(repo_id: Option<RepoId>, operation: &str) -> Result<PendingAudit, String> {
    let paths = StorePaths::from_environment()
        .map_err(|error| format!("audit failed before operation: {error}"))?;
    begin_audit_operation_with_paths(repo_id, operation, paths)
}

fn begin_audit_operation_with_paths(
    repo_id: Option<RepoId>,
    operation: &str,
    paths: StorePaths,
) -> Result<PendingAudit, String> {
    let store = LocalStore::open(paths)
        .map_err(|error| format!("audit failed before operation: {error}"))?;
    let entry = AuditEntry::new(repo_id.clone(), operation, "started", "pending")
        .map_err(|error| format!("audit failed before operation: {error}"))?;
    store
        .append_audit(entry)
        .map_err(|error| format!("audit failed before operation: {error}"))?;
    Ok(PendingAudit {
        repo_id,
        operation: operation.to_owned(),
        store,
    })
}

struct PendingAudit {
    repo_id: Option<RepoId>,
    operation: String,
    store: LocalStore,
}

impl PendingAudit {
    fn finish(self, result: &AuditTerminalResult) -> Result<(), String> {
        let entry = AuditEntry::new(
            self.repo_id,
            self.operation,
            result.result_label,
            result.message.clone(),
        )
        .map_err(|error| format!("audit failed: {error}"))?;
        self.store
            .append_audit(entry)
            .map_err(|error| format!("audit failed: {error}"))
    }
}

fn sanitize_audit_message(message: &str) -> String {
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let redacted = normalized
        .split(' ')
        .filter(|part| !part.is_empty())
        .map(redact_audit_word)
        .collect::<Vec<_>>()
        .join(" ");
    let sanitized = if redacted.is_empty() {
        "empty message".to_owned()
    } else {
        redacted
    };
    truncate_audit_message(&sanitized)
}

fn redact_audit_word(word: &str) -> String {
    let lower = word.to_ascii_lowercase();
    if !AUDIT_SECRET_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return word.to_owned();
    }

    if let Some((key, _value)) = word.split_once('=') {
        return format!("{key}=[redacted]");
    }
    if let Some((key, _value)) = word.split_once(':') {
        return format!("{key}:[redacted]");
    }
    "[redacted]".to_owned()
}

fn truncate_audit_message(message: &str) -> String {
    let mut output = String::new();
    for (index, character) in message.chars().enumerate() {
        if index == MAX_AUDIT_MESSAGE_LEN {
            output.push_str("...");
            return output;
        }
        output.push(character);
    }
    output
}

fn sanitized_git_error(error: &GitError) -> String {
    match error {
        GitError::GitFailed { status, .. } => format!("git failed with status {status}"),
        GitError::Io { .. } => "git failed before execution".to_owned(),
        GitError::Utf8 { stream, .. } => format!("git returned non-UTF-8 {stream}"),
        GitError::Blocked { message } => format!("operation blocked: {message}"),
        GitError::Parse { message } => format!("failed to parse git output: {message}"),
    }
}

fn sanitized_github_error(error: &GhError) -> String {
    match error {
        GhError::MissingCli | GhError::NotAuthenticated | GhError::InvalidInput { .. } => {
            error.to_string()
        }
        GhError::CommandFailed { status } => format!("GitHub CLI failed with status {status}"),
        GhError::Io { .. } => "GitHub CLI failed before execution".to_owned(),
        GhError::InvalidOutput { operation } => {
            format!("GitHub CLI returned invalid output for {operation}")
        }
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
    fn prompt_sequences_queue_visible_ordered_plan() -> Result<(), Box<dyn Error>> {
        let mut app = App::new();
        app.focus = Focus::Prompt;
        app.prompt = "fetch && branches".to_owned();

        app.submit_prompt();

        assert!(app.prompt.is_empty());
        assert!(app.details.contains("Prompt sequence plan:"));
        assert!(app.details.contains("1. fetch default remote"));
        assert!(app.details.contains("2. list local and remote branches"));
        assert!(app.details.contains("Press y to run 2 prompt steps"));
        let pending = app
            .operation_queue
            .pending()
            .ok_or_else(|| std::io::Error::other("missing queued prompt sequence"))?;
        assert!(pending.sequence.is_some());
        assert_eq!(
            pending.plan.confirmation.requirement,
            ConfirmationRequirement::VisiblePlan
        );
        Ok(())
    }

    #[test]
    fn unsupported_mixed_prompt_sequence_rejects_without_queue() {
        let mut app = App::new();
        app.focus = Focus::Prompt;
        app.prompt = "fetch and git push".to_owned();

        app.submit_prompt();

        assert!(app.details.contains("Raw Git commands are not supported"));
        assert_eq!(app.operation_queue.pending(), None);
    }

    #[test]
    fn manual_bulk_actions_queue_same_plan_as_request_planner() -> Result<(), Box<dyn Error>> {
        for (key_code, request) in [
            (KeyCode::Char('a'), OperationRequest::StageAll),
            (KeyCode::Char('A'), OperationRequest::UnstageAll),
        ] {
            let expected = OperationPlanner::current()
                .plan_request(request)
                .map_err(std::io::Error::other)?;
            let mut app = App::new();
            app.focus = Focus::Status;

            app.handle_key(key(key_code));

            let Some(pending) = app.operation_queue.pending() else {
                return Err(std::io::Error::other("missing pending operation").into());
            };
            assert_eq!(pending.plan, expected.plan);
            assert_eq!(pending.context, expected.context);
            assert_eq!(app.details, expected.plan.preview_text());
        }
        Ok(())
    }

    #[test]
    fn parsed_commit_prompt_matches_manual_planner_output() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("prompt-planner-commit")?;
        std::fs::write(repo.join("file.txt"), "hello\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        let ParsedPrompt::Single(request) = parse_prompt("commit -m \"ship staged\"")? else {
            return Err(std::io::Error::other("expected single prompt").into());
        };
        let planner = OperationPlanner::new(repo);

        let prompt_operation = planner
            .plan_request(request)
            .map_err(std::io::Error::other)?;
        let manual_operation = planner
            .plan_request(OperationRequest::Commit {
                message: "ship staged".to_owned(),
            })
            .map_err(std::io::Error::other)?;

        assert_eq!(prompt_operation, manual_operation);
        assert_eq!(
            prompt_operation.plan.confirmation.requirement,
            ConfirmationRequirement::VisiblePlan
        );
        assert!(!prompt_operation.plan.preview_text().contains("commit -m"));
        Ok(())
    }

    #[test]
    fn commit_push_sequence_previews_ordered_plan_without_planning_push()
    -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("prompt-sequence-preview")?;
        std::fs::write(repo.join("file.txt"), "hello\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        let requests = vec![
            OperationRequest::Commit {
                message: "ship staged".to_owned(),
            },
            OperationRequest::Push,
        ];
        let planner = OperationPlanner::new(repo);

        let sequence = planner
            .plan_prompt_sequence(requests)
            .map_err(std::io::Error::other)?;
        let manual_commit = planner
            .plan_request(OperationRequest::Commit {
                message: "ship staged".to_owned(),
            })
            .map_err(std::io::Error::other)?;

        assert_eq!(sequence.sequence.first, manual_commit);
        assert_eq!(
            sequence.sequence.remaining_requests,
            vec![OperationRequest::Push]
        );
        let preview = sequence.plan.preview_text();
        assert!(preview.contains("Prompt sequence plan:"));
        assert!(preview.contains("1. commit 1 staged file(s)"));
        assert!(preview.contains("message: ship staged"));
        assert!(preview.contains("2. push current branch"));
        assert!(preview.contains("planned after step 1 succeeds"));
        assert_eq!(
            sequence.plan.confirmation.requirement,
            ConfirmationRequirement::VisiblePlan
        );
        Ok(())
    }

    #[test]
    fn commit_sequence_preparation_failure_does_not_queue_push() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("prompt-sequence-no-staged")?;
        let planner = OperationPlanner::new(repo);

        let error = match planner.plan_prompt_sequence(vec![
            OperationRequest::Commit {
                message: "ship staged".to_owned(),
            },
            OperationRequest::Push,
        ]) {
            Ok(_sequence) => {
                return Err(std::io::Error::other(
                    "commit preparation should fail before push is planned",
                )
                .into());
            }
            Err(error) => error,
        };

        assert!(error.contains("there are no staged changes"));
        Ok(())
    }

    #[test]
    fn prompt_sequence_stops_before_push_when_commit_execution_fails() -> Result<(), Box<dyn Error>>
    {
        let repo = isolated_git_repo("prompt-sequence-commit-fails")?;
        configure_git_identity(&repo)?;
        std::fs::write(repo.join("file.txt"), "hello\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        let sequence = OperationPlanner::new(&repo)
            .plan_prompt_sequence(vec![
                OperationRequest::Commit {
                    message: "ship staged".to_owned(),
                },
                OperationRequest::Push,
            ])
            .map_err(std::io::Error::other)?;
        std::fs::write(repo.join("file.txt"), "changed after preview\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        let paths = isolated_store_paths("prompt-sequence-commit-fails-audit")?;

        let result = PromptSequenceExecutor::with_audit_paths(&repo, paths.clone())
            .execute(sequence.sequence);

        assert!(
            result
                .message()
                .contains("Prompt sequence stopped after step 1 of 2.")
        );
        let entries = LocalStore::open(paths)?.list_audit_entries()?;
        let operations = entries
            .iter()
            .map(|entry| entry.operation.as_str())
            .collect::<Vec<_>>();
        assert_eq!(operations, vec!["commit", "commit"]);
        assert_eq!(entries[1].result, "error");
        Ok(())
    }

    #[test]
    fn prompt_sequence_replans_push_after_commit() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("prompt-sequence-replan-push")?;
        let remote = isolated_bare_git_repo("prompt-sequence-replan-push-remote")?;
        configure_git_identity(&repo)?;
        std::fs::write(repo.join("file.txt"), "base\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "initial"])?;
        let branch = git_stdout(&repo, &["branch", "--show-current"])?
            .trim()
            .to_owned();
        let remote_arg = remote.to_string_lossy().to_string();
        git_stdout(&repo, &["remote", "add", "origin", remote_arg.as_str()])?;
        git_stdout(&repo, &["push", "-u", "origin", branch.as_str()])?;
        std::fs::write(repo.join("file.txt"), "changed\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        let sequence = OperationPlanner::new(&repo)
            .plan_prompt_sequence(vec![
                OperationRequest::Commit {
                    message: "ship staged".to_owned(),
                },
                OperationRequest::Push,
            ])
            .map_err(std::io::Error::other)?;
        let paths = isolated_store_paths("prompt-sequence-replan-push-audit")?;

        let result = PromptSequenceExecutor::with_audit_paths(&repo, paths.clone())
            .execute(sequence.sequence);

        assert!(
            result
                .message()
                .contains("Prompt sequence completed 2 step(s).")
        );
        let local_head = git_stdout(&repo, &["rev-parse", "HEAD"])?;
        let remote_ref = format!("refs/heads/{branch}");
        let remote_head = git_stdout(&repo, &["ls-remote", "origin", remote_ref.as_str()])?;
        assert!(remote_head.starts_with(local_head.trim()));
        let entries = LocalStore::open(paths)?.list_audit_entries()?;
        let operations = entries
            .iter()
            .map(|entry| entry.operation.as_str())
            .collect::<Vec<_>>();
        assert_eq!(operations, vec!["commit", "commit", "push", "push"]);
        assert!(entries.iter().all(|entry| entry.result != "error"));
        Ok(())
    }

    #[test]
    fn parsed_low_risk_prompts_match_manual_planner_output() -> Result<(), Box<dyn Error>> {
        let planner = OperationPlanner::current();
        for (prompt, request) in [
            ("fetch", OperationRequest::Fetch),
            ("branches", OperationRequest::Branches),
        ] {
            let ParsedPrompt::Single(parsed_request) = parse_prompt(prompt)? else {
                return Err(std::io::Error::other("expected single prompt").into());
            };

            let prompt_operation = planner
                .plan_request(parsed_request)
                .map_err(std::io::Error::other)?;
            let manual_operation = planner
                .plan_request(request)
                .map_err(std::io::Error::other)?;

            assert_eq!(prompt_operation, manual_operation);
            assert_eq!(
                prompt_operation.plan.confirmation.requirement,
                ConfirmationRequirement::NormalSelection
            );
        }
        Ok(())
    }

    #[test]
    fn parsed_high_risk_prompt_keeps_explicit_confirmation() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("prompt-planner-rebase")?;
        configure_git_identity(&repo)?;
        std::fs::write(repo.join("file.txt"), "base\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "initial"])?;
        let base = git_stdout(&repo, &["branch", "--show-current"])?
            .trim()
            .to_owned();
        git_stdout(&repo, &["checkout", "-b", "feature/rebase"])?;
        let ParsedPrompt::Single(request) = parse_prompt(&format!("rebase {base}"))? else {
            return Err(std::io::Error::other("expected single prompt").into());
        };

        let operation = OperationPlanner::new(repo)
            .plan_request(request)
            .map_err(std::io::Error::other)?;

        assert_eq!(
            operation.plan.confirmation.requirement,
            ConfirmationRequirement::ExplicitConfirmation
        );
        assert_eq!(operation.plan.request, OperationRequest::Rebase { base });
        assert!(matches!(
            operation.context.payload,
            Some(PendingPayload::Rebase { .. })
        ));
        Ok(())
    }

    #[test]
    fn open_pull_request_plan_previews_target_and_creates_pull_request()
    -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-create")?;
        let fake_gh = fake_gh("open-pr-create", false)?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
        };

        let operation = planner
            .plan_request(OperationRequest::OpenPullRequest { base: None })
            .map_err(std::io::Error::other)?;

        let preview = operation.plan.preview_text();
        assert!(preview.contains("provider: GitHub"));
        assert!(preview.contains("remote: origin"));
        assert!(preview.contains("head: feature/open-pr"));
        assert!(preview.contains("base: main"));
        assert!(preview.contains("title: feature/open-pr"));
        assert!(preview.contains("https://github.com/octo/repo/compare/main...feature/open-pr"));
        assert_eq!(
            operation.plan.confirmation.requirement,
            ConfirmationRequirement::VisiblePlan
        );
        let paths = isolated_store_paths("open-pr-create-audit")?;
        let execution = PlanExecutor::with_audit_paths(&repo, paths.clone())
            .execute(&operation.plan, operation.context);

        assert!(execution.succeeded(), "{}", execution.message());
        assert_eq!(
            execution.message(),
            "Pull request created: https://github.com/octo/repo/pull/43"
        );
        let entries = LocalStore::open(paths)?.list_audit_entries()?;
        assert_eq!(entries[0].operation, "open_pull_request");
        assert!(
            entries
                .iter()
                .all(|entry| !entry.message.contains("pull/43"))
        );
        Ok(())
    }

    #[test]
    fn existing_pull_request_is_surfaced_without_creation() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-existing")?;
        let fake_gh = fake_gh("open-pr-existing", true)?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
        };
        let operation = planner
            .plan_request(OperationRequest::OpenPullRequest { base: None })
            .map_err(std::io::Error::other)?;

        assert!(
            operation
                .plan
                .preview_text()
                .contains("target: https://github.com/octo/repo/pull/42")
        );
        let execution =
            PlanExecutor::with_audit_paths(&repo, isolated_store_paths("open-pr-existing-audit")?)
                .execute(&operation.plan, operation.context);

        assert!(execution.succeeded(), "{}", execution.message());
        assert_eq!(
            execution.message(),
            "Pull request already open: https://github.com/octo/repo/pull/42"
        );
        assert!(
            !std::fs::read_to_string(fake_gh.with_file_name("invocations"))?.contains("pr:create")
        );
        Ok(())
    }

    #[test]
    fn open_pull_request_blocks_missing_upstream_and_matching_base() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("open-pr-invalid-state")?;
        configure_git_identity(&repo)?;
        std::fs::write(repo.join("file.txt"), "base\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "initial"])?;
        let planner = OperationPlanner::new(&repo);

        let error = match planner.plan_request(OperationRequest::OpenPullRequest { base: None }) {
            Ok(_) => return Err(std::io::Error::other("missing upstream must fail closed").into()),
            Err(error) => error,
        };
        assert!(error.contains("has no upstream"));

        let remote = isolated_bare_git_repo("open-pr-invalid-state-remote")?;
        let branch = git_stdout(&repo, &["branch", "--show-current"])?
            .trim()
            .to_owned();
        let remote_arg = remote.to_string_lossy().to_string();
        git_stdout(&repo, &["remote", "add", "origin", remote_arg.as_str()])?;
        git_stdout(&repo, &["push", "-u", "origin", branch.as_str()])?;
        let fake_gh = fake_gh("open-pr-invalid-state", false)?;
        let planner = OperationPlanner {
            repo_root: repo,
            github_executable: Some(fake_gh),
        };
        let error =
            match planner.plan_request(OperationRequest::OpenPullRequest { base: Some(branch) }) {
                Ok(_) => return Err(std::io::Error::other("base=head must fail closed").into()),
                Err(error) => error,
            };
        assert!(error.contains("base branch must differ"));
        Ok(())
    }

    #[test]
    fn operation_plans_use_guardrail_risk_levels() {
        let branch = BranchTarget {
            name: "feature/auth".to_owned(),
            reference: "refs/heads/feature/auth".to_owned(),
            oid: "abcdef1234567890".to_owned(),
            kind: BranchKind::Local,
        };

        assert_eq!(fetch_plan().confirmation.risk_level, RiskLevel::Low);
        assert_eq!(
            stage_paths_plan(vec!["file.txt".to_owned()])
                .confirmation
                .risk_level,
            RiskLevel::Low
        );
        assert_eq!(
            commit_plan("message", 1).confirmation.risk_level,
            RiskLevel::Medium
        );
        assert_eq!(
            push_plan("feature/auth", "origin/feature/auth", 1)
                .confirmation
                .risk_level,
            RiskLevel::Medium
        );
        assert_eq!(
            push_set_upstream_plan("feature/auth", "origin")
                .confirmation
                .risk_level,
            RiskLevel::Medium
        );
        assert_eq!(
            pull_plan(false, "origin/main", 1).confirmation.risk_level,
            RiskLevel::Medium
        );
        assert_eq!(
            checkout_plan(&branch).confirmation.risk_level,
            RiskLevel::Medium
        );
        assert_eq!(
            create_branch_plan("feature/new", Some(&branch), "feature/auth at abcdef123456")
                .confirmation
                .risk_level,
            RiskLevel::Medium
        );
        assert_eq!(
            merge_plan("main", &branch).confirmation.risk_level,
            RiskLevel::Medium
        );
        assert_eq!(
            pull_plan(true, "origin/main", 1).confirmation.risk_level,
            RiskLevel::High
        );
        assert_eq!(
            rebase_plan("feature/new", &branch).confirmation.risk_level,
            RiskLevel::High
        );
    }

    #[test]
    fn single_push_url_blocks_ambiguous_destinations() {
        assert_eq!(
            single_push_url("origin", &["ssh://example.test/repo.git".to_owned()]),
            Ok("ssh://example.test/repo.git")
        );
        assert!(single_push_url("origin", &[]).is_err());
        assert!(
            single_push_url(
                "origin",
                &[
                    "ssh://example.test/one.git".to_owned(),
                    "ssh://example.test/two.git".to_owned(),
                ]
            )
            .is_err()
        );
    }

    #[test]
    fn branch_summary_shows_ahead_behind() {
        let branch = BranchState {
            head: Head::Branch("main".to_owned()),
            upstream: Some("origin/main".to_owned()),
            ahead: 2,
            behind: 1,
            unborn: false,
        };

        assert_eq!(
            branch_summary(Some(&branch)),
            "main -> origin/main | ahead 2 behind 1"
        );
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
    fn partial_rename_uses_section_specific_pathspecs() {
        let entry = StatusEntry {
            path: std::path::PathBuf::from("new.txt"),
            original_path: Some(std::path::PathBuf::from("old.txt")),
            index: ChangeKind::Renamed,
            worktree: ChangeKind::Modified,
            entry_type: StatusEntryType::Renamed,
        };

        let rows = FileRow::from_entry(&entry);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].section, FileSection::Staged);
        assert_eq!(rows[0].pathspecs.len(), 2);
        assert_eq!(rows[1].section, FileSection::Unstaged);
        assert_eq!(rows[1].pathspecs, vec![std::path::PathBuf::from("new.txt")]);
    }

    #[test]
    fn staged_plan_items_include_only_staged_rows() {
        let entries = vec![
            StatusEntry {
                path: std::path::PathBuf::from("staged.txt"),
                original_path: None,
                index: ChangeKind::Added,
                worktree: ChangeKind::Unmodified,
                entry_type: StatusEntryType::Ordinary,
            },
            StatusEntry {
                path: std::path::PathBuf::from("unstaged.txt"),
                original_path: None,
                index: ChangeKind::Unmodified,
                worktree: ChangeKind::Modified,
                entry_type: StatusEntryType::Ordinary,
            },
        ];

        let items = staged_plan_items(&entries);

        assert_eq!(items.len(), 1);
        assert!(items[0].contains("staged.txt"));
    }

    #[test]
    fn status_window_follows_selected_file() {
        let mut app = App::new();
        app.focus = Focus::Status;
        app.last_viewport.status = Rect::new(0, 0, 30, 5);
        let visible_len = status_file_visible_len(app.last_viewport.status);
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

        assert_eq!(
            app.operation_queue
                .pending()
                .and_then(|operation| operation.context.payload.as_ref()),
            Some(&PendingPayload::StageAll)
        );
        assert!(app.details.contains("Stage all plan:"));
        let queue_text = queue_panel_text(app.operation_queue.pending());
        assert!(queue_text[0].contains("Stage all plan"));
        assert!(queue_text[0].contains("Risk: medium"));
        assert!(queue_text[1].contains("stage all working tree changes"));
        assert!(queue_text[2].contains("Press y to stage all changes"));

        app.handle_key(key(KeyCode::Char('n')));

        assert_eq!(app.operation_queue.pending(), None);
        assert_eq!(app.details, "Operation cancelled.");
    }

    #[test]
    fn queue_panel_shows_empty_placeholder_only_without_pending_plan() {
        let app = App::new();

        let queue_text = queue_panel_text(app.operation_queue.pending());

        assert_eq!(queue_text[0], "No queued operations.");
    }

    #[test]
    fn unrelated_key_cancels_pending_confirmation() {
        let mut app = App::new();
        app.focus = Focus::Status;

        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Down));

        assert_eq!(app.operation_queue.pending(), None);
        assert_eq!(app.details, "Operation cancelled.");
    }

    #[test]
    fn high_risk_rebase_confirmations_reject_plain_y() {
        let branch = BranchTarget {
            name: "origin/main".to_owned(),
            reference: "refs/remotes/origin/main".to_owned(),
            oid: "abcdef1234567890".to_owned(),
            kind: BranchKind::Remote,
        };
        let target = HeadTarget {
            oid: Some("1234567890abcdef".to_owned()),
            reference: Some("refs/heads/feature/new".to_owned()),
        };

        let actions = [
            QueuedOperation::with_payload(
                pull_plan(true, "origin/main", 1),
                PendingPayload::PullRebase {
                    local_branch: "feature/new".to_owned(),
                    target: target.clone(),
                    upstream: "origin/main".to_owned(),
                    remote: "origin".to_owned(),
                    upstream_branch: "main".to_owned(),
                    upstream_oid: Some("abcdef1234567890".to_owned()),
                },
            ),
            QueuedOperation::with_payload(
                rebase_plan("feature/new", &branch),
                PendingPayload::Rebase {
                    base: branch,
                    target,
                },
            ),
        ];

        for action in actions {
            let mut app = App::new();
            let payload = action.context.payload.clone();
            app.operation_queue.enqueue(action);

            app.handle_key(key(KeyCode::Char('y')));

            assert_eq!(
                app.operation_queue
                    .pending()
                    .and_then(|operation| operation.context.payload.as_ref()),
                payload.as_ref()
            );
            assert_eq!(
                app.details,
                "Explicit confirmation required: press uppercase Y to confirm or n to cancel."
            );
        }
    }

    #[test]
    fn mouse_click_cancels_pending_confirmation() {
        let mut app = App::new();
        app.focus = Focus::Status;

        app.handle_key(key(KeyCode::Char('a')));
        app.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        }));

        assert_eq!(app.operation_queue.pending(), None);
        assert_eq!(app.details, "Operation cancelled.");
    }

    #[test]
    fn stage_operations_are_audited() -> Result<(), Box<dyn Error>> {
        let paths = isolated_store_paths("stage-audit")?;
        let result = AuditTerminalResult::completed();

        for operation in [
            "stage_path",
            "unstage_path",
            "stage_all",
            "unstage_all",
            "commit",
        ] {
            let audit = begin_audit_operation_with_paths(None, operation, paths.clone())?;
            audit.finish(&result)?;
        }

        let entries = LocalStore::open(paths)?.list_audit_entries()?;
        assert_eq!(entries.len(), 10);
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
        assert_eq!(entries[8].operation, "commit");
        assert_eq!(entries[9].operation, "commit");
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
        let result = AuditTerminalResult::error("operation blocked: test failure".to_owned());

        let audit = begin_audit_operation_with_paths(None, "stage_all", paths.clone())?;
        let audit_result = audit.finish(&result);

        assert!(audit_result.is_ok());
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
    fn audit_errors_do_not_persist_raw_git_output() -> Result<(), Box<dyn Error>> {
        let paths = isolated_store_paths("sanitized-audit")?;
        let status = std::process::Command::new("git")
            .arg("not-a-real-bitbygit-command")
            .output()?
            .status;
        let error = StepExecutionError::Git(GitError::GitFailed {
            args: vec!["commit-tree".to_owned(), "<tree>".to_owned()],
            status,
            stdout: "raw stdout token".to_owned(),
            stderr: "raw stderr secret".to_owned(),
        });
        let result = AuditTerminalResult::error(error.audit_message());

        let audit = begin_audit_operation_with_paths(None, "commit", paths.clone())?;
        let audit_result = audit.finish(&result);

        assert!(audit_result.is_ok());
        let entries = LocalStore::open(paths)?.list_audit_entries()?;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].result, "error");
        assert!(entries[1].message.contains("git failed with status"));
        assert!(!entries[1].message.contains("raw stdout token"));
        assert!(!entries[1].message.contains("raw stderr secret"));
        Ok(())
    }

    #[test]
    fn audit_messages_redact_common_secret_markers() -> Result<(), Box<dyn Error>> {
        let paths = isolated_store_paths("redacted-audit")?;
        let result = AuditTerminalResult::error(
            "credential helper failed token=raw-token secret=raw-secret Authorization: gho_raw"
                .to_owned(),
        );

        let audit = begin_audit_operation_with_paths(None, "push", paths.clone())?;
        let audit_result = audit.finish(&result);

        assert!(audit_result.is_ok());
        let entries = LocalStore::open(paths)?.list_audit_entries()?;
        assert_eq!(entries.len(), 2);
        assert!(!entries[1].message.contains("raw-token"));
        assert!(!entries[1].message.contains("raw-secret"));
        assert!(!entries[1].message.contains("gho_raw"));
        assert!(!entries[1].message.contains('\n'));
        assert!(entries[1].message.contains("[redacted]"));
        Ok(())
    }

    #[test]
    fn typed_executor_stops_after_first_failed_step() -> Result<(), Box<dyn Error>> {
        let paths = isolated_store_paths("executor-stop")?;
        let plan = two_step_staging_plan(false);

        let execution =
            PlanExecutor::with_audit_paths("/definitely/not/a/bitbygit/repo", paths.clone())
                .execute(&plan, ExecutionContext::default());

        assert_eq!(execution.step_results.len(), 1);
        assert!(!execution.step_results[0].succeeded());
        assert!(
            execution
                .message()
                .contains("Operation stopped after step 1 of 2.")
        );
        let entries = LocalStore::open(paths)?.list_audit_entries()?;
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.operation == "stage_all"));
        assert_eq!(entries[0].result, "started");
        assert_eq!(entries[1].result, "error");
        Ok(())
    }

    #[test]
    fn typed_executor_continues_only_when_step_allows_it() -> Result<(), Box<dyn Error>> {
        let paths = isolated_store_paths("executor-continue")?;
        let plan = two_step_staging_plan(true);

        let execution =
            PlanExecutor::with_audit_paths("/definitely/not/a/bitbygit/repo", paths.clone())
                .execute(&plan, ExecutionContext::default());

        assert_eq!(execution.step_results.len(), 2);
        assert!(
            execution
                .step_results
                .iter()
                .all(|result| !result.succeeded())
        );
        assert!(
            execution
                .message()
                .contains("Operation completed 2 step(s) with failures.")
        );
        let entries = LocalStore::open(paths)?.list_audit_entries()?;
        let operations = entries
            .iter()
            .map(|entry| entry.operation.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            vec!["stage_all", "stage_all", "unstage_all", "unstage_all"]
        );
        Ok(())
    }

    #[test]
    fn existing_stage_all_workflow_executes_through_typed_steps() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("typed-stage-all")?;
        std::fs::write(repo.join("file.txt"), "hello\n")?;
        let paths = isolated_store_paths("typed-stage-all-audit")?;
        let plan = stage_all_plan();

        let execution = PlanExecutor::with_audit_paths(&repo, paths.clone()).execute(
            &plan,
            ExecutionContext::from_payload(PendingPayload::StageAll),
        );

        assert_eq!(execution.step_results.len(), 1);
        assert!(
            execution.step_results[0].succeeded(),
            "{}",
            execution.message()
        );
        assert_eq!(
            git_stdout(&repo, &["diff", "--cached", "--name-only"])?.trim(),
            "file.txt"
        );
        let entries = LocalStore::open(paths)?.list_audit_entries()?;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].result, "started");
        assert_eq!(entries[1].result, "ok");
        let expected_repo_id = Some(RepoId::from_path(&repo));
        assert!(
            entries
                .iter()
                .all(|entry| entry.repo_id.as_ref() == expected_repo_id.as_ref())
        );
        Ok(())
    }

    #[test]
    fn executor_does_not_use_prompt_text_as_execution_input() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("typed-no-raw-prompt")?;
        std::fs::write(repo.join("file.txt"), "hello\n")?;
        let paths = isolated_store_paths("typed-no-raw-prompt-audit")?;
        let plan = OperationPlan::new(
            OperationRequest::Fetch,
            "Prompt text plan",
            vec![OperationStep::new(
                OperationKind::StagePaths,
                RiskLevel::Low,
                "stage from prompt text",
            )],
            "stage file.txt",
        );

        let execution = PlanExecutor::with_audit_paths(&repo, paths)
            .execute(&plan, ExecutionContext::default());

        assert_eq!(execution.step_results.len(), 1);
        assert!(execution.message().contains("typed stage paths request"));
        assert!(
            git_stdout(&repo, &["diff", "--cached", "--name-only"])?
                .trim()
                .is_empty()
        );
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

    fn two_step_staging_plan(continue_after_failure: bool) -> OperationPlan {
        let first_step = OperationStep::new(
            OperationKind::StageAll,
            RiskLevel::Medium,
            "stage all working tree changes",
        );
        let first_step = if continue_after_failure {
            first_step.allow_safe_continuation_after_failure()
        } else {
            first_step
        };
        OperationPlan::new(
            OperationRequest::StageAll,
            "Two-step staging plan",
            vec![
                first_step,
                OperationStep::new(
                    OperationKind::UnstageAll,
                    RiskLevel::Medium,
                    "unstage all staged changes",
                ),
            ],
            "",
        )
    }

    fn isolated_git_repo(name: &str) -> Result<PathBuf, Box<dyn Error>> {
        let root = isolated_temp_root(name)?;
        std::fs::create_dir_all(&root)?;
        let output = std::process::Command::new("git")
            .arg("init")
            .arg(&root)
            .output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "git init failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
            .into());
        }
        Ok(root)
    }

    fn isolated_bare_git_repo(name: &str) -> Result<PathBuf, Box<dyn Error>> {
        let root = isolated_temp_root(name)?;
        let output = std::process::Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg(&root)
            .output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "git init --bare failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
            .into());
        }
        Ok(root)
    }

    fn pushed_branch_repo(name: &str) -> Result<PathBuf, Box<dyn Error>> {
        let repo = isolated_git_repo(name)?;
        let remote = isolated_bare_git_repo(&format!("{name}-remote"))?;
        configure_git_identity(&repo)?;
        std::fs::write(repo.join("file.txt"), "base\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "initial"])?;
        git_stdout(&repo, &["checkout", "-b", "feature/open-pr"])?;
        std::fs::write(repo.join("file.txt"), "feature\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "feature"])?;
        let remote_arg = remote.to_string_lossy().to_string();
        git_stdout(&repo, &["remote", "add", "origin", remote_arg.as_str()])?;
        git_stdout(&repo, &["push", "-u", "origin", "feature/open-pr"])?;
        Ok(repo)
    }

    fn fake_gh(name: &str, existing: bool) -> Result<PathBuf, Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let root = isolated_temp_root(&format!("fake-gh-{name}"))?;
        std::fs::create_dir_all(&root)?;
        let executable = root.join("gh");
        let invocations = root.join("invocations");
        let pull_requests = if existing {
            "[{\"number\":42,\"url\":\"https://github.com/octo/repo/pull/42\",\"title\":\"Existing PR\",\"baseRefName\":\"main\",\"headRefName\":\"feature/open-pr\"}]"
        } else {
            "[]"
        };
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s:%s\\n' \"$1\" \"$2\" >> '{}'\ncase \"$1:$2\" in\n--version:*) exit 0 ;;\nauth:status) exit 0 ;;\nrepo:view) printf '%s\\n' '{{\"nameWithOwner\":\"octo/repo\",\"defaultBranchRef\":{{\"name\":\"main\"}}}}' ;;\npr:list) printf '%s\\n' '{}' ;;\npr:create) printf '%s\\n' 'https://github.com/octo/repo/pull/43' ;;\n*) exit 1 ;;\nesac\n",
                invocations.display(),
                pull_requests
            ),
        )?;
        let mut permissions = std::fs::metadata(&executable)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions)?;
        Ok(executable)
    }

    fn git_stdout(repo: &std::path::Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            ))
            .into());
        }
        Ok(String::from_utf8(output.stdout)?)
    }

    fn configure_git_identity(repo: &std::path::Path) -> Result<(), Box<dyn Error>> {
        git_stdout(repo, &["config", "user.email", "bitbygit@example.invalid"])?;
        git_stdout(repo, &["config", "user.name", "bitbygit tests"])?;
        git_stdout(repo, &["config", "commit.gpgsign", "false"])?;
        Ok(())
    }

    fn isolated_store_paths(name: &str) -> Result<StorePaths, Box<dyn Error>> {
        let root = isolated_temp_root(name)?;
        Ok(StorePaths::from_roots(
            root.join("config"),
            root.join("data"),
        ))
    }

    fn isolated_temp_root(name: &str) -> Result<PathBuf, Box<dyn Error>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        Ok(std::env::temp_dir().join(format!("bitbygit-tui-{name}-{}-{now}", std::process::id())))
    }
}
