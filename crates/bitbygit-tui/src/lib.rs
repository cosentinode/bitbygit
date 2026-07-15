use std::error::Error;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::time::{Duration, Instant};

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
    policy::{EffectivePolicy, PolicyEvaluation},
    prompt_parser::{ParsedPrompt, parse_prompt},
};
use bitbygit_gh::{CreatePullRequest, GhError, GitHub};
use bitbygit_git::{
    BranchInfo, BranchKind, BranchState, BranchTarget, ChangeKind, Git, GitError, GitOutput, Head,
    HeadTarget, RepositoryOperation, StatusEntry, StatusEntryType,
};
use bitbygit_store::{AuditEntry, LocalStore, RepoId, StorePaths};

const MAX_PROMPT_LEN: usize = 512;
const MAX_AUDIT_MESSAGE_LEN: usize = 512;
const POLICY_RELOAD_INTERVAL: Duration = Duration::from_secs(1);
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
    let mut app = App::from_startup(startup_policy_from_environment());
    let mut last_policy_reload = Instant::now();

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

        let input_ready = event::poll(Duration::from_millis(100))?;
        if input_ready || last_policy_reload.elapsed() >= POLICY_RELOAD_INTERVAL {
            app.reload_policy(&AuditDestination::Environment);
            last_policy_reload = Instant::now();
        }
        if input_ready {
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
    repository_operation: Option<RepositoryOperation>,
    selected_file: usize,
    file_scroll: usize,
    details: String,
    config_diagnostic: Option<String>,
    operation_queue: OperationQueue,
    policy: EffectivePolicy,
    should_quit: bool,
    last_viewport: Viewport,
}

impl App {
    pub fn new() -> Self {
        Self::with_policy(EffectivePolicy::default())
    }

    fn with_policy(policy: EffectivePolicy) -> Self {
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
            repository_operation: None,
            selected_file: 0,
            file_scroll: 0,
            details: "No repository status loaded yet.".to_owned(),
            config_diagnostic: None,
            operation_queue: OperationQueue::default(),
            policy,
            should_quit: false,
            last_viewport: Viewport::default(),
        }
    }

    fn from_startup(startup: StartupPolicy) -> Self {
        let mut app = Self::with_policy(startup.policy);
        app.config_diagnostic = startup.diagnostic;
        app.load_current_dir();
        app
    }

    fn reload_policy(&mut self, destination: &AuditDestination) {
        let reload = destination.reload_policy(&self.policy);
        let recovered = self.config_diagnostic.is_some() && reload.diagnostic.is_none();
        self.policy = reload.policy;
        self.config_diagnostic = reload.diagnostic;
        if recovered {
            self.refresh_status();
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
        if let Some(error) = direct_read_policy_error(&self.policy, OperationKind::RefreshStatus) {
            self.details = error;
            return;
        }
        match Git::new(current_dir()).status() {
            Ok(status) => {
                self.files = status
                    .entries
                    .iter()
                    .flat_map(FileRow::from_entry)
                    .collect();
                self.branch = Some(status.branch);
                self.repository_operation = status.operation;
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
                self.repository_operation = None;
                self.selected_file = 0;
                self.details = format!("Unable to read repository status: {error}");
            }
        }
    }

    fn refresh_diff(&mut self) {
        if let Some(error) = direct_read_policy_error(&self.policy, OperationKind::ViewDiff) {
            self.details = error;
            return;
        }
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
        match OperationPlanner::current(self.policy.clone()).plan_stage_pathspecs(pathspecs) {
            Ok(operation) => self.submit_prepared_operation(operation),
            Err(error) => self.details = error,
        }
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
        match OperationPlanner::current(self.policy.clone()).plan_unstage_pathspecs(pathspecs) {
            Ok(operation) => self.submit_prepared_operation(operation),
            Err(error) => self.details = error,
        }
    }

    fn submit_operation_request(&mut self, request: OperationRequest) {
        match OperationPlanner::current(self.policy.clone()).plan_request(request) {
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
        let executor = PlanExecutor::current(self.policy.clone());
        self.execute_operation_with(&executor, plan, context);
    }

    fn execute_operation_with(
        &mut self,
        executor: &PlanExecutor,
        plan: OperationPlan,
        context: ExecutionContext,
    ) {
        let message = executor.execute(&plan, context).message();
        self.reload_policy(&executor.audit);
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
            self.execute_prompt_sequence(sequence, operation.plan.confirmation.requirement);
        } else {
            self.execute_operation(operation.plan, operation.context);
        }
    }

    fn execute_prompt_sequence(
        &mut self,
        sequence: QueuedPromptSequence,
        confirmed_requirement: ConfirmationRequirement,
    ) {
        let executor = PromptSequenceExecutor::current(self.policy.clone());
        self.execute_prompt_sequence_with(&executor, sequence, confirmed_requirement);
    }

    fn execute_prompt_sequence_with(
        &mut self,
        executor: &PromptSequenceExecutor,
        sequence: QueuedPromptSequence,
        confirmed_requirement: ConfirmationRequirement,
    ) {
        let result = executor.execute_confirmed(sequence, confirmed_requirement);
        self.policy = result.policy().clone();
        self.reload_policy(&executor.audit);
        if result.should_refresh_status() {
            self.refresh_status();
        }
        self.details = result.message();
    }

    fn submit_prompt(&mut self) {
        if !self.policy.prompt_enabled() {
            self.details = "Prompt input is disabled by policy.".to_owned();
            return;
        }
        let parsed = match parse_prompt(&self.prompt) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.details = error.to_string();
                return;
            }
        };
        match parsed {
            ParsedPrompt::Single(request) => {
                match OperationPlanner::current(self.policy.clone()).plan_request(request) {
                    Ok(operation) => {
                        self.prompt.clear();
                        self.submit_prepared_operation(operation);
                    }
                    Err(error) => self.details = error,
                }
            }
            ParsedPrompt::Sequence(requests) => {
                match OperationPlanner::current(self.policy.clone()).plan_prompt_sequence(requests)
                {
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
        let visible_len = status_file_visible_len(area, self.repository_operation.is_some());
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
    deferred_pull_request_targets: Vec<Option<DeferredPullRequestTarget>>,
    policy_evaluations: Vec<PolicyEvaluation>,
    sequence_policy_evaluations: Vec<PolicyEvaluation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeferredPullRequestTarget {
    repository: GitHubRepository,
    head_repository: GitHubRepository,
    head: String,
    base: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SequenceBranch {
    name: String,
    upstream: SequenceUpstream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SequenceUpstream {
    Existing,
    Missing,
    Bound { remote: String, branch: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TypedPushTarget {
    remote: String,
    branch: String,
    sets_upstream: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitHubRepository(String);

impl GitHubRepository {
    fn new(hostname: &str, name_with_owner: &str) -> Self {
        Self(format!("{hostname}/{name_with_owner}"))
    }

    fn hostname(&self) -> &str {
        self.0.split_once('/').map_or("", |(hostname, _)| hostname)
    }

    fn name_with_owner(&self) -> &str {
        self.0
            .split_once('/')
            .map_or("", |(_, name_with_owner)| name_with_owner)
    }

    fn eq_ignore_ascii_case(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(&other.0)
    }
}

impl QueuedPromptSequence {
    fn new(
        first: PreparedOperation,
        remaining_requests: Vec<OperationRequest>,
        deferred_pull_request_targets: Vec<Option<DeferredPullRequestTarget>>,
        policy_evaluations: Vec<PolicyEvaluation>,
        sequence_policy_evaluations: Vec<PolicyEvaluation>,
    ) -> Self {
        Self {
            first,
            remaining_requests,
            deferred_pull_request_targets,
            policy_evaluations,
            sequence_policy_evaluations,
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
        tracking_oid: Option<String>,
        upstream_oid: Option<String>,
    },
    PullRebase {
        local_branch: String,
        target: HeadTarget,
        upstream: String,
        remote: String,
        upstream_branch: String,
        tracking_oid: Option<String>,
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
        upstream_branch: String,
        remote_urls: Vec<String>,
        target: HeadTarget,
        base: String,
        title: String,
        repository: String,
        github_repository: GitHubRepository,
        head_github_repository: GitHubRepository,
        head_repository: String,
        head: String,
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
        if area.width < 50 || area.height < 18 {
            return Self::compact(area);
        }

        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(8),
                Constraint::Length(7),
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
    } else if app.operation_queue.has_pending() {
        let area = Rect::new(
            areas.repos.x,
            areas.repos.y,
            areas.prompt.width,
            areas.prompt.y.saturating_sub(areas.repos.y),
        );
        frame.render_widget(Clear, area);
        frame.render_widget(compact_queue_panel(app, area.width), area);
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
    repository: &GitHubRepository,
    existing_url: Option<&str>,
) -> OperationPlan {
    let target = existing_url.map(ToOwned::to_owned).unwrap_or_else(|| {
        format!(
            "https://{}/compare/{}...{}?expand=1",
            repository.0,
            compare_url_ref(base),
            compare_url_ref(head)
        )
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

fn compare_url_ref(reference: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut encoded = String::with_capacity(reference.len());
    for byte in reference.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                encoded.push(char::from(byte));
            }
            _ => {
                encoded.push('%');
                encoded.push(char::from(HEX[usize::from(byte >> 4)]));
                encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
        }
    }
    encoded
}

fn disabled_operation_message(operation: OperationKind) -> String {
    format!(
        "Operation blocked: {} is disabled by policy.",
        operation.action_label()
    )
}

fn direct_read_policy_error(policy: &EffectivePolicy, operation: OperationKind) -> Option<String> {
    if policy.is_operation_disabled(operation) {
        return Some(disabled_operation_message(operation));
    }
    let requirement = policy
        .evaluate_confirmation(RiskLevel::Low, operation, None)
        .requirement;
    match requirement {
        ConfirmationRequirement::NormalSelection => None,
        ConfirmationRequirement::Blocked => Some(format!(
            "Operation blocked: {} is blocked by confirmation policy.",
            operation.action_label()
        )),
        requirement => Some(format!(
            "Operation blocked: {} requires {}; direct UI reads support normal selection only.",
            operation.action_label(),
            confirmation_requirement_label(requirement)
        )),
    }
}

fn operation_request_kind(request: &OperationRequest) -> Option<OperationKind> {
    match request {
        OperationRequest::RefreshStatus => Some(OperationKind::RefreshStatus),
        OperationRequest::ViewDiff { .. } => Some(OperationKind::ViewDiff),
        OperationRequest::Fetch => Some(OperationKind::Fetch),
        OperationRequest::StagePaths { .. } => Some(OperationKind::StagePaths),
        OperationRequest::UnstagePaths { .. } => Some(OperationKind::UnstagePaths),
        OperationRequest::StageAll => Some(OperationKind::StageAll),
        OperationRequest::UnstageAll => Some(OperationKind::UnstageAll),
        OperationRequest::Commit { .. } => Some(OperationKind::Commit),
        OperationRequest::Push => Some(OperationKind::PushCurrentBranch),
        OperationRequest::Pull { rebase: false } => Some(OperationKind::PullFastForward),
        OperationRequest::Pull { rebase: true } => Some(OperationKind::PullRebase),
        OperationRequest::Branches => Some(OperationKind::Branches),
        OperationRequest::Checkout { .. } => Some(OperationKind::CheckoutBranch),
        OperationRequest::CreateBranch { .. } => Some(OperationKind::CreateBranch),
        OperationRequest::Merge { .. } => Some(OperationKind::MergeFastForward),
        OperationRequest::Rebase { .. } => Some(OperationKind::Rebase),
        OperationRequest::OpenPullRequest { .. } => Some(OperationKind::OpenPullRequest),
        OperationRequest::PromptSequence { .. } => None,
    }
}

fn apply_policy_to_plan(policy: &EffectivePolicy, plan: &mut OperationPlan, branch: Option<&str>) {
    let evaluation = evaluate_plan_policy(policy, plan, branch);
    apply_policy_evaluation(plan, evaluation);
}

fn evaluate_plan_policy(
    policy: &EffectivePolicy,
    plan: &OperationPlan,
    branch: Option<&str>,
) -> PolicyEvaluation {
    combined_policy_evaluation(
        plan.steps
            .iter()
            .map(|step| policy.evaluate_confirmation(step.risk_level, step.kind, branch)),
    )
}

fn evaluate_sequence_plan_policy(
    policy: &EffectivePolicy,
    plan: &OperationPlan,
    branch: Option<&str>,
) -> PolicyEvaluation {
    combined_policy_evaluation(plan.steps.iter().map(|step| {
        policy.evaluate_confirmation(sequence_step_risk(step.risk_level), step.kind, branch)
    }))
}

fn combined_policy_evaluation(
    evaluations: impl IntoIterator<Item = PolicyEvaluation>,
) -> PolicyEvaluation {
    let mut requirement = ConfirmationRequirement::NormalSelection;
    let mut reasons = Vec::new();
    for evaluation in evaluations {
        requirement = requirement.max(evaluation.requirement);
        for reason in evaluation.reasons {
            if !reasons.contains(&reason) {
                reasons.push(reason);
            }
        }
    }
    PolicyEvaluation {
        requirement,
        reasons,
    }
}

fn apply_policy_evaluation(plan: &mut OperationPlan, evaluation: PolicyEvaluation) {
    let previous_requirement = plan.confirmation.requirement;
    let requirement = previous_requirement.max(evaluation.requirement);
    plan.confirmation.requirement = requirement;
    plan.confirmation.reason =
        (!evaluation.reasons.is_empty()).then(|| evaluation.reasons.join("; "));
    if requirement != previous_requirement {
        plan.confirmation.prompt = confirmation_prompt(plan, requirement);
    }
}

fn confirmation_prompt(plan: &OperationPlan, requirement: ConfirmationRequirement) -> String {
    let action = confirmation_action(plan);
    match requirement {
        ConfirmationRequirement::NormalSelection => String::new(),
        ConfirmationRequirement::VisiblePlan => {
            format!("Press y to {action} or n to cancel.")
        }
        ConfirmationRequirement::ExplicitConfirmation => {
            format!("Explicit confirmation required: press uppercase Y to {action} or n to cancel.")
        }
        ConfirmationRequirement::Blocked => {
            format!("Policy blocks this operation: {action}. Press n to dismiss.")
        }
    }
}

fn confirmation_action(plan: &OperationPlan) -> String {
    let prompt = plan.confirmation.prompt.as_str();
    for prefix in [
        "Press y to ",
        "Explicit confirmation required: press uppercase Y to ",
    ] {
        if let Some(action) = prompt
            .strip_prefix(prefix)
            .and_then(|action| action.strip_suffix(" or n to cancel."))
        {
            return action.to_owned();
        }
    }
    if let OperationRequest::PromptSequence { requests } = &plan.request {
        return format!("run {} prompt steps", requests.len());
    }
    plan.first_step()
        .map(|step| step.kind.action_label().to_owned())
        .unwrap_or_else(|| "run the operation".to_owned())
}

fn confirmation_requirement_label(requirement: ConfirmationRequirement) -> &'static str {
    match requirement {
        ConfirmationRequirement::NormalSelection => "normal selection",
        ConfirmationRequirement::VisiblePlan => "a visible plan",
        ConfirmationRequirement::ExplicitConfirmation => "explicit uppercase-Y confirmation",
        ConfirmationRequirement::Blocked => "the operation to be blocked",
    }
}

fn confirmation_covers(
    plan: &OperationPlan,
    confirmed_requirement: ConfirmationRequirement,
    previewed_policy: &PolicyEvaluation,
    current_policy: &PolicyEvaluation,
) -> bool {
    plan.confirmation.requirement != ConfirmationRequirement::Blocked
        && plan.confirmation.requirement <= confirmed_requirement
        && current_policy.requirement <= previewed_policy.requirement
        && protected_policy_reasons_covered(&previewed_policy.reasons, current_policy)
}

fn sequence_policy_confirmation_covers(
    confirmed_requirement: ConfirmationRequirement,
    previewed_policies: &[PolicyEvaluation],
    current_policies: &[PolicyEvaluation],
) -> bool {
    confirmed_requirement != ConfirmationRequirement::Blocked
        && previewed_policies.len() == current_policies.len()
        && previewed_policies
            .iter()
            .zip(current_policies)
            .all(|(previewed, current)| {
                previewed.requirement != ConfirmationRequirement::Blocked
                    && current.requirement != ConfirmationRequirement::Blocked
                    && previewed.requirement <= confirmed_requirement
                    && current.requirement <= previewed.requirement
                    && protected_policy_reasons_covered(&previewed.reasons, current)
            })
}

fn protected_policy_reasons_covered(
    previewed_reasons: &[String],
    current_policy: &PolicyEvaluation,
) -> bool {
    current_policy
        .reasons
        .iter()
        .filter(|reason| reason.starts_with("protected branch "))
        .all(|reason| previewed_reasons.contains(reason))
}

fn prompt_sequence_policy_evaluations(
    policy: &EffectivePolicy,
    git: &Git,
    requests: &[OperationRequest],
    initial_branch: Option<String>,
) -> Result<(Vec<PolicyEvaluation>, Vec<PolicyEvaluation>), String> {
    let mut branch = initial_branch;
    let mut evaluations = Vec::with_capacity(requests.len());
    let mut sequence_evaluations = Vec::with_capacity(requests.len());
    for request in requests {
        let preview = prompt_sequence_request_preview(request)?;
        evaluations.push(policy.evaluate_confirmation(
            preview.risk_level,
            preview.kind,
            branch.as_deref(),
        ));
        sequence_evaluations.push(policy.evaluate_confirmation(
            sequence_step_risk(preview.risk_level),
            preview.kind,
            branch.as_deref(),
        ));
        match request {
            OperationRequest::Checkout { branch: target } if branch.as_deref() != Some(target) => {
                branch = Some(sequence_checkout_resulting_branch(git, target)?);
            }
            OperationRequest::CreateBranch { branch: target, .. } => {
                branch = Some(target.clone());
            }
            _ => {}
        }
    }
    Ok((evaluations, sequence_evaluations))
}

fn sequence_checkout_resulting_branch(git: &Git, target: &str) -> Result<String, String> {
    if let Some(target) = git
        .branch_target(target)
        .map_err(|error| format!("Unable to resolve sequence checkout: {error}"))?
    {
        return checkout_resulting_branch(&target);
    }
    let Some((remote, branch)) = target.split_once('/') else {
        return Ok(target.to_owned());
    };
    let remote_exists = git
        .remotes()
        .ok()
        .is_some_and(|remotes| remotes.iter().any(|candidate| candidate.name == remote));
    if remote_exists {
        Ok(branch.to_owned())
    } else {
        Ok(target.to_owned())
    }
}

fn checkout_resulting_branch(target: &BranchTarget) -> Result<String, String> {
    match target.kind {
        BranchKind::Local => Ok(target.name.clone()),
        BranchKind::Remote => target
            .name
            .split_once('/')
            .map(|(_remote, branch)| branch.to_owned())
            .filter(|branch| !branch.is_empty())
            .ok_or_else(|| {
                format!(
                    "Checkout blocked: remote branch {} has no local branch name.",
                    target.name
                )
            }),
    }
}

fn prompt_sequence_plan(
    requests: &[OperationRequest],
    first: &PreparedOperation,
    deferred_pull_request_targets: &[Option<DeferredPullRequestTarget>],
) -> Result<OperationPlan, String> {
    let Some(first_step) = first.plan.first_step() else {
        return Err("Prompt sequence blocked: first step produced no visible plan.".to_owned());
    };
    let mut steps = vec![prompt_sequence_first_step(1, first_step)];
    for ((index, request), pull_request_target) in requests
        .iter()
        .enumerate()
        .skip(1)
        .zip(deferred_pull_request_targets)
    {
        steps.push(prompt_sequence_deferred_step(
            index + 1,
            request,
            pull_request_target.as_ref(),
        )?);
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
    pull_request_target: Option<&DeferredPullRequestTarget>,
) -> Result<OperationStep, String> {
    let mut preview = prompt_sequence_request_preview(request)?;
    if let Some(target) = pull_request_target {
        preview.details.retain(|detail| {
            !detail.starts_with("base: ") && !detail.starts_with("re-plan branch, remote, base")
        });
        preview
            .details
            .push(format!("repository: {}", target.repository.0));
        preview.details.push(format!("head: {}", target.head));
        preview.details.push(format!("base: {}", target.base));
        preview.details.push(format!(
            "target: https://{}/compare/{}...{}?expand=1",
            target.repository.0,
            compare_url_ref(&target.base),
            compare_url_ref(&target.head)
        ));
        preview.details.push(
            "revalidate the bound provider, repository, head, and base before execution".to_owned(),
        );
    }
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
        OperationRequest::OpenPullRequest { base } => {
            let preview = PromptSequenceStepPreview::new(
                OperationKind::OpenPullRequest,
                RiskLevel::Medium,
                "open or surface pull request",
            )
            .with_detail("provider: GitHub")
            .with_detail(
                "re-plan branch, remote, base, and pull request state immediately before this step",
            );
            Ok(match base {
                Some(base) => preview.with_detail(format!("base: {base}")),
                None => preview.with_detail("base: repository default (resolved after push)"),
            })
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
    let visible_len = status_file_visible_len(area, app.repository_operation.is_some());
    let mut lines = vec![Line::from(branch_summary(app.branch.as_ref()))];
    if let Some(operation) = app.repository_operation {
        lines.push(
            Line::from(format!("{} IN PROGRESS", operation_label(operation))).style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        );
    }
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

fn operation_label(operation: RepositoryOperation) -> &'static str {
    match operation {
        RepositoryOperation::Merge => "MERGE",
        RepositoryOperation::Rebase => "REBASE",
    }
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
    policy: EffectivePolicy,
    #[cfg(test)]
    ssh_executable: Option<PathBuf>,
}

impl OperationPlanner {
    fn current(policy: EffectivePolicy) -> Self {
        Self {
            repo_root: current_dir(),
            github_executable: None,
            policy,
            #[cfg(test)]
            ssh_executable: None,
        }
    }

    #[cfg(test)]
    fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
            github_executable: None,
            policy: EffectivePolicy::default(),
            ssh_executable: None,
        }
    }

    #[cfg(test)]
    fn with_policy(repo_root: impl Into<PathBuf>, policy: EffectivePolicy) -> Self {
        Self {
            repo_root: repo_root.into(),
            github_executable: None,
            policy,
            ssh_executable: None,
        }
    }

    fn plan_request(&self, request: OperationRequest) -> Result<PreparedOperation, String> {
        if let Some(operation) = operation_request_kind(&request) {
            self.ensure_operation_enabled(operation)?;
        }
        if let Some(operation) = self.preflight_blocked_request(&request)? {
            return Ok(operation);
        }
        let operation = match request {
            OperationRequest::RefreshStatus => {
                return Err("Refresh status is handled directly by the UI.".to_owned());
            }
            OperationRequest::ViewDiff { .. } => {
                return Err("View diff is handled directly by the UI.".to_owned());
            }
            OperationRequest::StagePaths { paths } => PreparedOperation::new(
                stage_paths_plan(paths.clone()),
                ExecutionContext::from_payload(PendingPayload::StagePaths {
                    paths: paths.into_iter().map(PathBuf::from).collect(),
                }),
            ),
            OperationRequest::UnstagePaths { paths } => PreparedOperation::new(
                unstage_paths_plan(paths.clone()),
                ExecutionContext::from_payload(PendingPayload::UnstagePaths {
                    paths: paths.into_iter().map(PathBuf::from).collect(),
                }),
            ),
            OperationRequest::StageAll => PreparedOperation::new(
                stage_all_plan(),
                ExecutionContext::from_payload(PendingPayload::StageAll),
            ),
            OperationRequest::UnstageAll => PreparedOperation::new(
                unstage_all_plan(),
                ExecutionContext::from_payload(PendingPayload::UnstageAll),
            ),
            OperationRequest::Commit { message } => self.plan_commit(message)?,
            OperationRequest::Fetch => {
                PreparedOperation::new(fetch_plan(), ExecutionContext::default())
            }
            OperationRequest::Push => self.plan_push()?,
            OperationRequest::Pull { rebase } => self.plan_pull(rebase)?,
            OperationRequest::Branches => {
                PreparedOperation::new(branches_plan(), ExecutionContext::default())
            }
            OperationRequest::Checkout { branch } => self.plan_checkout(branch)?,
            OperationRequest::CreateBranch { branch, base } => {
                self.plan_create_branch(branch, base)?
            }
            OperationRequest::Merge { branch } => self.plan_merge(branch)?,
            OperationRequest::Rebase { base } => self.plan_rebase(base)?,
            OperationRequest::OpenPullRequest { base } => self.plan_open_pull_request(base)?,
            OperationRequest::PromptSequence { .. } => {
                return Err("Prompt sequences are handled by prompt submission.".to_owned());
            }
        };
        Ok(self.apply_policy(operation))
    }

    fn preflight_blocked_request(
        &self,
        request: &OperationRequest,
    ) -> Result<Option<PreparedOperation>, String> {
        let OperationRequest::Pull { rebase } = request else {
            return Ok(None);
        };
        let status = self
            .git()
            .status()
            .map_err(|error| format!("Unable to prepare pull plan: {error}"))?;
        let local_branch = branch_name(&status.branch)?;
        let preview = prompt_sequence_request_preview(request)?;
        let evaluation = self.policy.evaluate_confirmation(
            preview.risk_level,
            preview.kind,
            Some(&local_branch),
        );
        if evaluation.requirement != ConfirmationRequirement::Blocked {
            return Ok(None);
        }

        let upstream = status.branch.upstream.as_deref().unwrap_or("no upstream");
        let mut plan = pull_plan(*rebase, upstream, status.branch.behind);
        apply_policy_evaluation(&mut plan, evaluation);
        Ok(Some(PreparedOperation::new(
            plan,
            ExecutionContext::default(),
        )))
    }

    fn plan_prompt_sequence(
        &self,
        requests: Vec<OperationRequest>,
    ) -> Result<PreparedPromptSequence, String> {
        if requests.len() < 2 {
            return Err("Prompt sequence requires at least two steps.".to_owned());
        }
        for (index, request) in requests.iter().enumerate() {
            let preview = prompt_sequence_request_preview(request).map_err(|error| {
                format!("Prompt sequence step {} is blocked: {error}", index + 1)
            })?;
            self.ensure_operation_enabled(preview.kind)
                .map_err(|error| {
                    format!("Prompt sequence step {} is blocked: {error}", index + 1)
                })?;
        }
        let git = self.git();
        let branch = git
            .status()
            .ok()
            .and_then(|status| branch_name(&status.branch).ok())
            .or_else(|| {
                git.head_target()
                    .ok()
                    .and_then(|target| head_target_branch(&target).map(ToOwned::to_owned))
            });
        let deferred_pull_request_targets =
            self.deferred_pull_request_targets(&requests, branch.clone())?;
        let (policy_evaluations, sequence_policy_evaluations) =
            prompt_sequence_policy_evaluations(&self.policy, &git, &requests, branch)?;
        let first_request = requests
            .first()
            .cloned()
            .ok_or_else(|| "Prompt sequence requires at least two steps.".to_owned())?;
        let blocked = policy_evaluations
            .iter()
            .chain(&sequence_policy_evaluations)
            .any(|evaluation| evaluation.requirement == ConfirmationRequirement::Blocked);
        let first = if blocked {
            prompt_sequence_preflight_operation(first_request)?
        } else {
            self.plan_request(first_request)?
        };
        let mut plan = prompt_sequence_plan(&requests, &first, &deferred_pull_request_targets)?;
        apply_policy_evaluation(
            &mut plan,
            combined_policy_evaluation(
                policy_evaluations
                    .iter()
                    .chain(&sequence_policy_evaluations)
                    .cloned(),
            ),
        );
        let remaining_requests = requests.into_iter().skip(1).collect();
        Ok(PreparedPromptSequence::new(
            plan,
            QueuedPromptSequence::new(
                first,
                remaining_requests,
                deferred_pull_request_targets,
                policy_evaluations,
                sequence_policy_evaluations,
            ),
        ))
    }

    fn deferred_pull_request_targets(
        &self,
        requests: &[OperationRequest],
        initial_branch: Option<String>,
    ) -> Result<Vec<Option<DeferredPullRequestTarget>>, String> {
        let git = self.git();
        let mut branch = initial_branch.map(|name| SequenceBranch {
            name,
            upstream: SequenceUpstream::Existing,
        });
        let mut targets = Vec::with_capacity(requests.len().saturating_sub(1));

        for (index, request) in requests.iter().enumerate() {
            if index > 0 {
                let target = match request {
                    OperationRequest::OpenPullRequest { base } => {
                        let branch = branch.as_ref().ok_or_else(|| {
                            "Open pull request blocked: unable to resolve the deferred branch."
                                .to_owned()
                        })?;
                        Some(self.deferred_pull_request_target(&git, branch, base.as_deref())?)
                    }
                    _ => None,
                };
                targets.push(target);
            }

            if !requests[index + 1..]
                .iter()
                .any(|request| matches!(request, OperationRequest::OpenPullRequest { .. }))
            {
                continue;
            }
            match request {
                OperationRequest::Checkout { branch: target }
                    if branch.as_ref().map(|branch| &branch.name) != Some(target) =>
                {
                    let target = git
                        .branch_target(target)
                        .map_err(|error| format!("Unable to resolve sequence checkout: {error}"))?
                        .ok_or_else(|| {
                            format!(
                                "Prompt sequence blocked: checkout target {target} cannot be validated during preflight; create a new sequence preview."
                            )
                        })?;
                    git.ensure_remote_checkout_target_available(&target)
                        .map_err(|error| {
                            format!(
                                "Prompt sequence blocked: checkout target cannot be validated during preflight: {error}; create a new sequence preview."
                            )
                        })?;
                    let name = checkout_resulting_branch(&target)?;
                    let upstream = if target.kind == BranchKind::Remote {
                        let (remote, branch) = target.name.split_once('/').ok_or_else(|| {
                            format!(
                                "Prompt sequence blocked: remote checkout target {} cannot be bound during preflight; create a new sequence preview.",
                                target.name
                            )
                        })?;
                        SequenceUpstream::Bound {
                            remote: remote.to_owned(),
                            branch: branch.to_owned(),
                        }
                    } else {
                        SequenceUpstream::Existing
                    };
                    branch = Some(SequenceBranch { name, upstream });
                }
                OperationRequest::CreateBranch { branch: target, .. } => {
                    branch = Some(SequenceBranch {
                        name: target.clone(),
                        upstream: SequenceUpstream::Missing,
                    });
                }
                OperationRequest::Push => {
                    let branch = branch.as_mut().ok_or_else(|| {
                        "Prompt sequence blocked: push branch cannot be resolved during preflight; create a new sequence preview."
                            .to_owned()
                    })?;
                    let target = self.typed_push_target(&git, &branch.name, &branch.upstream)?;
                    if target.sets_upstream {
                        branch.upstream = SequenceUpstream::Bound {
                            remote: target.remote,
                            branch: target.branch,
                        };
                    }
                }
                _ => {}
            }
        }

        Ok(targets)
    }

    fn deferred_pull_request_target(
        &self,
        git: &Git,
        branch: &SequenceBranch,
        requested_base: Option<&str>,
    ) -> Result<DeferredPullRequestTarget, String> {
        let push_target = self.typed_push_target(git, &branch.name, &branch.upstream)?;
        if push_target.sets_upstream {
            return Err(
                "Open pull request blocked: the deferred branch will not have an upstream; add `push` before `open pr` and create a new sequence preview."
                    .to_owned(),
            );
        }
        let TypedPushTarget {
            remote,
            branch: upstream_branch,
            ..
        } = push_target;
        let remote_urls = git
            .remote_push_urls(&remote)
            .map_err(|error| format!("Unable to prepare pull request remote: {error}"))?;
        let push_url = single_pull_request_push_url(&remote, &remote_urls)?;
        let head_repository = github_repository_from_push_url(push_url)
            .map_err(|reason| format!("Open pull request blocked: remote {remote} {reason}"))?;
        let github_repository = github_base_repository(git, &remote, &head_repository)?;
        if !head_repository
            .hostname()
            .eq_ignore_ascii_case(github_repository.hostname())
        {
            return Err(
                "Open pull request blocked: source and upstream remotes use different GitHub hosts."
                    .to_owned(),
            );
        }
        let github = self.github(&github_repository);
        let repository = github.repository().map_err(open_pull_request_gh_error)?;
        let canonical_github_repository =
            GitHubRepository::new(github_repository.hostname(), &repository.name_with_owner);
        let canonical_head_repository = if head_repository.eq_ignore_ascii_case(&github_repository)
        {
            canonical_github_repository.clone()
        } else {
            let head = self
                .github(&head_repository)
                .repository()
                .map_err(open_pull_request_gh_error)?;
            GitHubRepository::new(head_repository.hostname(), &head.name_with_owner)
        };
        let head = pull_request_head(
            &canonical_head_repository,
            &canonical_github_repository,
            &upstream_branch,
        )?;
        let configured_base =
            requested_base.is_none() && self.policy.default_pull_request_base().is_some();
        let base = requested_base
            .map(ToOwned::to_owned)
            .or_else(|| {
                self.policy
                    .default_pull_request_base()
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| repository.default_branch.clone());
        validate_pull_request_base_head(
            &canonical_head_repository,
            &canonical_github_repository,
            &base,
            &upstream_branch,
        )?;
        if !github
            .branch_exists(&base)
            .map_err(open_pull_request_gh_error)?
        {
            let mut error = format!(
                "Open pull request blocked: base branch {base} was not found in {}.",
                repository.name_with_owner
            );
            if configured_base {
                error.push_str(
                    " Update pull-requests.default-base-branch or choose an existing branch with `open pr to <branch>`.",
                );
            }
            return Err(error);
        }

        Ok(DeferredPullRequestTarget {
            repository: canonical_github_repository,
            head_repository: canonical_head_repository,
            head,
            base,
        })
    }

    fn plan_stage_pathspecs(&self, paths: Vec<PathBuf>) -> Result<PreparedOperation, String> {
        self.ensure_operation_enabled(OperationKind::StagePaths)?;
        Ok(self.apply_policy(PreparedOperation::new(
            stage_paths_plan(file_path_labels(&paths)),
            ExecutionContext::from_payload(PendingPayload::StagePaths { paths }),
        )))
    }

    fn plan_unstage_pathspecs(&self, paths: Vec<PathBuf>) -> Result<PreparedOperation, String> {
        self.ensure_operation_enabled(OperationKind::UnstagePaths)?;
        Ok(self.apply_policy(PreparedOperation::new(
            unstage_paths_plan(file_path_labels(&paths)),
            ExecutionContext::from_payload(PendingPayload::UnstagePaths { paths }),
        )))
    }

    fn ensure_operation_enabled(&self, operation: OperationKind) -> Result<(), String> {
        if self.policy.is_operation_disabled(operation) {
            Err(disabled_operation_message(operation))
        } else {
            Ok(())
        }
    }

    fn apply_policy(&self, mut operation: PreparedOperation) -> PreparedOperation {
        let branch = policy_branch(&operation.context);
        apply_policy_to_plan(&self.policy, &mut operation.plan, branch);
        operation
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

        let upstream = status.branch.upstream;
        let sequence_upstream = match upstream.as_deref() {
            Some(upstream) => {
                let (remote, branch) = git
                    .upstream_push_target(&branch)
                    .map_err(|error| format!("Unable to prepare push target: {error}"))?
                    .ok_or_else(|| {
                        format!("Push blocked: unable to resolve upstream {upstream}.")
                    })?;
                let expected_upstream = format!("{remote}/{branch}");
                if expected_upstream != upstream {
                    return Err(format!(
                        "Push blocked: upstream config does not match {upstream}."
                    ));
                }
                SequenceUpstream::Bound { remote, branch }
            }
            None => SequenceUpstream::Missing,
        };
        let push_target = self.typed_push_target(&git, &branch, &sequence_upstream)?;
        let remote_urls = git
            .remote_push_urls(&push_target.remote)
            .map_err(|error| format!("Unable to snapshot push remote URLs: {error}"))?;
        let push_url = single_push_url(&push_target.remote, &remote_urls)?;
        let expected_remote_oid = git
            .remote_url_head_oid(push_url, &push_target.branch)
            .map_err(|error| format!("Unable to snapshot push remote: {error}"))?;
        if push_target.sets_upstream {
            Ok(PreparedOperation::new(
                push_set_upstream_plan(&branch, &push_target.remote),
                ExecutionContext::from_payload(PendingPayload::PushSetUpstream {
                    remote: push_target.remote,
                    branch,
                    target: head_target,
                    expected_remote_oid,
                    remote_urls,
                }),
            ))
        } else {
            let upstream = upstream.ok_or_else(|| {
                "Push blocked: tracked push destination has no upstream.".to_owned()
            })?;
            let destination = format!("{}/{}", push_target.remote, push_target.branch);
            Ok(PreparedOperation::new(
                push_plan(&branch, &destination, ahead),
                ExecutionContext::from_payload(PendingPayload::Push {
                    local_branch: branch,
                    target: head_target,
                    remote: push_target.remote,
                    upstream_branch: push_target.branch,
                    upstream,
                    expected_remote_oid,
                    remote_urls,
                }),
            ))
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
        let tracking_oid = git
            .remote_tracking_oid(&remote, &upstream_branch)
            .map_err(|error| format!("Unable to snapshot pull tracking ref: {error}"))?;
        let upstream_oid = git
            .fetch_remote_branch_for_plan(&remote, &upstream_branch)
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
        let local_oid = head_target
            .oid
            .as_deref()
            .ok_or_else(|| "Pull blocked: current branch has no commit.".to_owned())?;
        let (ahead, behind) = git
            .ahead_behind(local_oid, &upstream_oid)
            .map_err(|error| format!("Unable to compare pull target: {error}"))?;
        if ahead > 0 && behind > 0 && !rebase {
            return Err("Pull blocked: branch has diverged. Use `pull --rebase` explicitly or resolve manually.".to_owned());
        }
        if rebase && !status.is_clean() {
            return Err("Pull rebase blocked: working tree must be clean.".to_owned());
        }
        let plan = pull_plan(rebase, &upstream, behind);
        let payload = if rebase {
            PendingPayload::PullRebase {
                local_branch,
                target: head_target,
                upstream,
                remote,
                upstream_branch,
                tracking_oid,
                upstream_oid: Some(upstream_oid),
            }
        } else {
            PendingPayload::Pull {
                local_branch,
                target: head_target,
                upstream,
                remote,
                upstream_branch,
                tracking_oid,
                upstream_oid: Some(upstream_oid),
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
        let (tracking_remote, tracking_branch) = git
            .upstream_push_target(&branch)
            .map_err(|error| format!("Unable to prepare pull request target: {error}"))?
            .ok_or_else(|| {
                "Open pull request blocked: unable to resolve the current branch upstream."
                    .to_owned()
            })?;
        if format!("{tracking_remote}/{tracking_branch}") != upstream {
            return Err(format!(
                "Open pull request blocked: upstream config does not match {upstream}."
            ));
        }
        let push_target = self.typed_push_target(
            &git,
            &branch,
            &SequenceUpstream::Bound {
                remote: tracking_remote,
                branch: tracking_branch,
            },
        )?;
        let remote = push_target.remote;
        let upstream_branch = push_target.branch;
        let remote_urls = git
            .remote_push_urls(&remote)
            .map_err(|error| format!("Unable to prepare pull request remote: {error}"))?;
        let push_url = single_pull_request_push_url(&remote, &remote_urls)?;
        let head_github_repository = github_repository_from_push_url(push_url)
            .map_err(|reason| format!("Open pull request blocked: remote {remote} {reason}"))?;
        let target = git
            .head_target()
            .map_err(|error| format!("Unable to snapshot pull request branch: {error}"))?;
        let local_oid = target.oid.as_deref().ok_or_else(|| {
            "Open pull request blocked: current branch needs a commit before opening a pull request."
                .to_owned()
        })?;
        let github_repository = github_base_repository(&git, &remote, &head_github_repository)?;
        if !head_github_repository
            .hostname()
            .eq_ignore_ascii_case(github_repository.hostname())
        {
            return Err(
                "Open pull request blocked: source and upstream remotes use different GitHub hosts."
                    .to_owned(),
            );
        }
        let github = self.github(&github_repository);
        let repository = github.repository().map_err(open_pull_request_gh_error)?;
        let head_github = self.github(&head_github_repository);
        let canonical_head_repository =
            if head_github_repository.eq_ignore_ascii_case(&github_repository) {
                repository.clone()
            } else {
                head_github
                    .repository()
                    .map_err(open_pull_request_gh_error)?
            };
        let remote_oid = head_github
            .branch_oid(&upstream_branch)
            .map_err(open_pull_request_gh_error)?;
        if remote_oid.as_deref() != Some(local_oid) {
            return Err("Open pull request blocked: current branch is not pushed to its upstream. Push it with `push` first.".to_owned());
        }
        let head_repository = canonical_head_repository.name_with_owner;
        let canonical_head_github_repository =
            GitHubRepository::new(head_github_repository.hostname(), &head_repository);
        let canonical_github_repository =
            GitHubRepository::new(github_repository.hostname(), &repository.name_with_owner);
        let head = pull_request_head(
            &canonical_head_github_repository,
            &canonical_github_repository,
            &upstream_branch,
        )?;
        let configured_base =
            requested_base.is_none() && self.policy.default_pull_request_base().is_some();
        let base = requested_base
            .or_else(|| {
                self.policy
                    .default_pull_request_base()
                    .map(ToOwned::to_owned)
            })
            .unwrap_or(repository.default_branch);
        validate_pull_request_base_head(
            &canonical_head_github_repository,
            &canonical_github_repository,
            &base,
            &upstream_branch,
        )?;
        if !github
            .branch_exists(&base)
            .map_err(open_pull_request_gh_error)?
        {
            let mut error = format!(
                "Open pull request blocked: base branch {base} was not found in {}.",
                repository.name_with_owner
            );
            if configured_base {
                error.push_str(
                    " Update pull-requests.default-base-branch or choose an existing branch with `open pr to <branch>`.",
                );
            }
            return Err(error);
        }
        let existing = github
            .existing_pull_requests(&head)
            .map_err(open_pull_request_gh_error)?;
        let existing_url =
            matching_pull_request(&existing, &base, &upstream_branch, &head_repository)
                .map(|pull_request| pull_request.url.as_str());
        let title = branch.clone();
        let request = OperationRequest::OpenPullRequest {
            base: Some(base.clone()),
        };
        Ok(PreparedOperation::new(
            open_pull_request_plan(
                request,
                &remote,
                &head,
                &base,
                &title,
                &canonical_github_repository,
                existing_url,
            ),
            ExecutionContext::from_payload(PendingPayload::OpenPullRequest {
                branch,
                upstream,
                remote,
                upstream_branch,
                remote_urls,
                target,
                base,
                title,
                repository: repository.name_with_owner,
                github_repository,
                head_github_repository,
                head_repository,
                head,
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

    fn typed_push_target(
        &self,
        git: &Git,
        branch: &str,
        upstream: &SequenceUpstream,
    ) -> Result<TypedPushTarget, String> {
        let upstream = match upstream {
            SequenceUpstream::Existing => git
                .upstream_push_target(branch)
                .map_err(|error| format!("Unable to prepare push target: {error}"))?,
            SequenceUpstream::Missing => None,
            SequenceUpstream::Bound { remote, branch } => Some((remote.clone(), branch.clone())),
        };
        let default_remote = default_remote_name(git);
        let (remote, target_branch) = git
            .typed_push_target(
                branch,
                upstream
                    .as_ref()
                    .map(|(remote, branch)| (remote.as_str(), branch.as_str())),
                default_remote.as_deref(),
            )
            .map_err(|error| format!("Unable to prepare push target: {error}"))?
            .ok_or_else(|| {
                "Push blocked: no configured push destination could be resolved.".to_owned()
            })?;
        Ok(TypedPushTarget {
            remote,
            branch: target_branch,
            sets_upstream: upstream.is_none(),
        })
    }

    fn git(&self) -> Git {
        #[cfg(test)]
        if let Some(executable) = &self.ssh_executable {
            return Git::with_ssh_executable(self.repo_root.clone(), executable);
        }
        Git::new(self.repo_root.clone())
    }

    fn github(&self, repository: &GitHubRepository) -> GitHub {
        match &self.github_executable {
            Some(executable) => GitHub::with_executable_and_repository(
                &self.repo_root,
                executable,
                repository.hostname(),
                repository.name_with_owner(),
            ),
            None => GitHub::with_executable_and_repository(
                &self.repo_root,
                "gh",
                repository.hostname(),
                repository.name_with_owner(),
            ),
        }
    }
}

fn default_remote_name(git: &Git) -> Option<String> {
    let remotes = git.remotes().ok()?;
    remotes
        .iter()
        .find(|remote| remote.name == "origin")
        .or_else(|| remotes.first())
        .map(|remote| remote.name.clone())
}

fn prompt_sequence_preflight_operation(
    request: OperationRequest,
) -> Result<PreparedOperation, String> {
    let preview = prompt_sequence_request_preview(&request)?;
    let mut step = OperationStep::new(preview.kind, preview.risk_level, preview.summary);
    for detail in preview.details {
        step = step.with_detail(detail);
    }
    Ok(PreparedOperation::new(
        OperationPlan::new(
            request,
            "Prompt sequence preflight",
            vec![step],
            String::new(),
        ),
        ExecutionContext::default(),
    ))
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

fn github_repository_from_push_url(url: &str) -> Result<GitHubRepository, &'static str> {
    let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("ssh") {
            return Err("uses an unsupported URL; configure an HTTPS or SSH GitHub remote URL.");
        }
        let (authority, path) = rest
            .split_once('/')
            .ok_or("has an invalid URL; use HOST/OWNER/REPOSITORY form.")?;
        (
            authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host),
            path,
        )
    } else {
        let (authority, path) = url
            .split_once(':')
            .ok_or("uses an unsupported URL; configure an HTTPS or SSH GitHub remote URL.")?;
        if authority.contains('/') || path.starts_with(':') {
            return Err("uses an unsupported URL; configure an HTTPS or SSH GitHub remote URL.");
        }
        (
            authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host),
            path,
        )
    };
    if host.contains(':') {
        return Err(
            "uses a custom port, which is not supported by the GitHub CLI integration; configure a hostname-only remote URL.",
        );
    }
    let hostname = host.to_ascii_lowercase();
    if !valid_github_hostname(&hostname) {
        return Err(
            "has an invalid GitHub hostname; configure a valid HTTPS or SSH GitHub remote URL.",
        );
    }
    let mut components = path.trim_end_matches('/').split('/');
    let owner = components
        .next()
        .ok_or("does not identify a GitHub OWNER/REPOSITORY.")?;
    let repository = components
        .next()
        .ok_or("does not identify a GitHub OWNER/REPOSITORY.")?;
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    if owner.is_empty()
        || repository.is_empty()
        || components.next().is_some()
        || owner.contains(['?', '#'])
        || repository.contains(['?', '#'])
    {
        return Err("does not identify a GitHub OWNER/REPOSITORY.");
    }
    Ok(GitHubRepository::new(
        &hostname,
        &format!("{owner}/{repository}"),
    ))
}

fn valid_github_hostname(hostname: &str) -> bool {
    !hostname.is_empty()
        && hostname.len() <= 253
        && hostname.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
}

fn github_base_repository(
    git: &Git,
    push_remote: &str,
    head_repository: &GitHubRepository,
) -> Result<GitHubRepository, String> {
    let remotes = git
        .remotes()
        .map_err(|error| format!("Unable to prepare pull request base remote: {error}"))?;
    if let Some(fetch_url) = remotes
        .iter()
        .find(|remote| remote.name == push_remote)
        .and_then(|remote| remote.fetch_url.as_deref())
    {
        let fetch_repository = github_repository_from_push_url(fetch_url).map_err(|reason| {
            format!("Open pull request blocked: remote {push_remote} fetch URL {reason}")
        })?;
        if !fetch_repository.eq_ignore_ascii_case(head_repository) {
            return Ok(fetch_repository);
        }
    }
    let upstream = remotes.into_iter().find(|remote| remote.name == "upstream");
    let Some(upstream) = upstream else {
        return Ok(head_repository.clone());
    };
    let url = upstream
        .fetch_url
        .as_deref()
        .or(upstream.push_url.as_deref())
        .ok_or_else(|| {
            "Open pull request blocked: upstream remote has no URL; configure the base repository or remove the remote."
                .to_owned()
        })?;
    github_repository_from_push_url(url)
        .map_err(|reason| format!("Open pull request blocked: upstream remote {reason}"))
}

fn pull_request_head(
    head_repository: &GitHubRepository,
    base_repository: &GitHubRepository,
    branch: &str,
) -> Result<String, String> {
    if head_repository.eq_ignore_ascii_case(base_repository) {
        return Ok(branch.to_owned());
    }
    if !head_repository
        .hostname()
        .eq_ignore_ascii_case(base_repository.hostname())
    {
        return Err(
            "Open pull request blocked: source and upstream remotes use different GitHub hosts."
                .to_owned(),
        );
    }
    let (owner, _) = head_repository
        .name_with_owner()
        .split_once('/')
        .ok_or_else(|| {
            "Open pull request blocked: source remote does not identify a GitHub repository."
                .to_owned()
        })?;
    Ok(format!("{owner}:{branch}"))
}

fn validate_pull_request_base_head(
    head_repository: &GitHubRepository,
    base_repository: &GitHubRepository,
    base: &str,
    head_branch: &str,
) -> Result<(), String> {
    if head_repository.eq_ignore_ascii_case(base_repository) && base == head_branch {
        return Err(
            "Open pull request blocked: base branch must differ from the pull request head."
                .to_owned(),
        );
    }
    Ok(())
}

fn matching_pull_request<'a>(
    pull_requests: &'a [bitbygit_gh::PullRequest],
    base: &str,
    branch: &str,
    head_repository: &str,
) -> Option<&'a bitbygit_gh::PullRequest> {
    pull_requests.iter().find(|pull_request| {
        pull_request.base_ref_name == base
            && pull_request.head_ref_name == branch
            && pull_request
                .head_repository
                .as_ref()
                .is_some_and(|pull_request_head_repository| {
                    pull_request_head_repository
                        .name_with_owner
                        .eq_ignore_ascii_case(head_repository)
                })
    })
}

fn validate_push_plan(
    git: &Git,
    branch: &str,
    expected_upstream: Option<&str>,
    target: &HeadTarget,
    remote: &str,
    remote_branch: &str,
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
    let upstream = match expected_upstream {
        Some(expected_upstream) => {
            let upstream = git
                .upstream_push_target(branch)
                .map_err(|error| format!("Unable to revalidate push target: {error}"))?
                .ok_or_else(|| {
                    "Push blocked: upstream config is no longer available.".to_owned()
                })?;
            if format!("{}/{}", upstream.0, upstream.1) != expected_upstream {
                return Err(
                    "Push blocked: upstream config changed since the plan was shown.".to_owned(),
                );
            }
            Some(upstream)
        }
        None => None,
    };
    let default_remote = default_remote_name(git);
    let current_target = git
        .typed_push_target(
            branch,
            upstream
                .as_ref()
                .map(|(remote, branch)| (remote.as_str(), branch.as_str())),
            default_remote.as_deref(),
        )
        .map_err(|error| format!("Unable to revalidate push target: {error}"))?;
    if current_target
        .as_ref()
        .map(|(remote, branch)| (remote.as_str(), branch.as_str()))
        != Some((remote, remote_branch))
    {
        return Err("Push blocked: push destination changed since the plan was shown.".to_owned());
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
    upstream_branch: &str,
    remote_urls: &[String],
    target: &HeadTarget,
) -> Result<(), StepExecutionError> {
    if git.head_target().map_err(StepExecutionError::Git)? != *target {
        return Err(StepExecutionError::Blocked(
            "Open pull request blocked: branch target changed since the plan was shown.".to_owned(),
        ));
    }
    let status = git.status().map_err(StepExecutionError::Git)?;
    if branch_name(&status.branch).map_err(StepExecutionError::Blocked)? != branch {
        return Err(StepExecutionError::Blocked(
            "Open pull request blocked: current branch changed since the plan was shown."
                .to_owned(),
        ));
    }
    if status.branch.upstream.as_deref() != Some(expected_upstream) {
        return Err(StepExecutionError::Blocked(
            "Open pull request blocked: upstream changed since the plan was shown.".to_owned(),
        ));
    }
    let tracking_target = git
        .upstream_push_target(branch)
        .map_err(StepExecutionError::Git)?
        .ok_or_else(|| {
            StepExecutionError::Blocked(
                "Open pull request blocked: upstream config is no longer available.".to_owned(),
            )
        })?;
    if format!("{}/{}", tracking_target.0, tracking_target.1) != expected_upstream {
        return Err(StepExecutionError::Blocked(
            "Open pull request blocked: upstream config changed since the plan was shown."
                .to_owned(),
        ));
    }
    let current_target = git
        .typed_push_target(
            branch,
            Some((tracking_target.0.as_str(), tracking_target.1.as_str())),
            None,
        )
        .map_err(StepExecutionError::Git)?;
    if current_target
        .as_ref()
        .map(|(name, branch)| (name.as_str(), branch.as_str()))
        != Some((remote, upstream_branch))
    {
        return Err(StepExecutionError::Blocked(
            "Open pull request blocked: push target changed since the plan was shown.".to_owned(),
        ));
    }
    let current_remote_urls = git
        .remote_push_urls(remote)
        .map_err(StepExecutionError::Git)?;
    if current_remote_urls != remote_urls {
        return Err(StepExecutionError::Blocked(
            "Open pull request blocked: remote URLs changed since the plan was shown.".to_owned(),
        ));
    }
    single_pull_request_push_url(remote, &current_remote_urls)
        .map_err(StepExecutionError::Blocked)?;
    Ok(())
}

fn status_file_visible_len(area: Rect, has_operation_banner: bool) -> usize {
    status_visible_len(area).saturating_sub(1 + usize::from(has_operation_banner))
}

fn details_panel(app: &App) -> Paragraph<'_> {
    Paragraph::new(details_text(app))
        .block(panel_block("Details", app.focus == Focus::Details))
        .wrap(Wrap { trim: true })
}

fn details_text(app: &App) -> String {
    app.config_diagnostic.as_ref().map_or_else(
        || app.details.clone(),
        |diagnostic| {
            format!(
                "Configuration reload error: {diagnostic}\n\n{}",
                app.details
            )
        },
    )
}

fn queue_panel(app: &App) -> Paragraph<'_> {
    let text = queue_panel_text(app.operation_queue.pending())
        .into_iter()
        .map(Line::from)
        .collect::<Vec<_>>();
    Paragraph::new(text).block(panel_block("Queue", app.focus == Focus::Queue))
}

fn compact_queue_panel(app: &App, width: u16) -> Paragraph<'_> {
    let text = app
        .operation_queue
        .pending()
        .map(|operation| {
            let plan = &operation.plan;
            if width < 30 {
                vec![
                    Line::from(format!(
                        "Keys: {}",
                        compact_accepted_confirmation_keys(plan, width)
                    )),
                    Line::from(tiny_policy_reason(plan)),
                    Line::from(confirmation_action(plan)),
                ]
            } else {
                vec![
                    Line::from(format!(
                        "Keys: {}",
                        compact_accepted_confirmation_keys(plan, width)
                    )),
                    Line::from(format!("Policy: {}", compact_policy_reason(plan))),
                    Line::from(format!("Confirm: {}", confirmation_action(plan))),
                ]
            }
        })
        .unwrap_or_default();
    Paragraph::new(text).block(panel_block("Pending confirmation", false))
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
        format!("Accepted keys: {}", accepted_confirmation_keys(plan)),
        format!("Policy: {}", policy_reason(plan)),
        format!("Confirm: {}", confirmation_copy(plan)),
        format!("Steps: {}", plan_steps_summary(plan)),
    ]
}

fn policy_reason(plan: &OperationPlan) -> String {
    let reason = plan
        .confirmation
        .reason
        .as_deref()
        .unwrap_or("safe default confirmation policy");
    let controlling_suffix = match plan.confirmation.requirement {
        ConfirmationRequirement::NormalSelection => "normal-selection confirmation",
        ConfirmationRequirement::VisiblePlan => "visible-plan confirmation",
        ConfirmationRequirement::ExplicitConfirmation => "explicit confirmation",
        ConfirmationRequirement::Blocked => "blocked confirmation",
    };
    let mut reasons = reason.split("; ").collect::<Vec<_>>();
    reasons.sort_by_key(|reason| {
        match (
            reason.ends_with(controlling_suffix),
            reason.starts_with("protected branch "),
        ) {
            (true, true) => 0,
            (true, false) => 1,
            (false, true) => 2,
            (false, false) => 3,
        }
    });
    reasons.join("; ")
}

fn compact_policy_reason(plan: &OperationPlan) -> String {
    let reason = policy_reason(plan);
    let controlling = reason.split("; ").next().unwrap_or(reason.as_str());
    if let Some((risk, requirement)) = controlling.split_once(" risk policy requires ") {
        return format!(
            "{risk} risk: {}",
            requirement.trim_end_matches(" confirmation")
        );
    }
    if controlling.starts_with("protected branch ") {
        return controlling
            .split_once(':')
            .map_or(controlling, |(branch, _detail)| branch)
            .to_owned();
    }
    controlling.to_owned()
}

fn tiny_policy_reason(plan: &OperationPlan) -> String {
    let reason = compact_policy_reason(plan);
    if let Some(branch) = reason.strip_prefix("protected branch ") {
        return format!("Protected: {}", branch.chars().take(7).collect::<String>());
    }
    if let Some((risk, requirement)) = reason.split_once(" risk: ") {
        return format!("{risk}: {requirement}");
    }
    format!("Policy: {reason}")
}

fn compact_accepted_confirmation_keys(plan: &OperationPlan, width: u16) -> &'static str {
    if width < 30 {
        return match plan.confirmation.requirement {
            ConfirmationRequirement::NormalSelection => "none",
            ConfirmationRequirement::VisiblePlan => "y/n/Esc",
            ConfirmationRequirement::ExplicitConfirmation => "Y/n/Esc",
            ConfirmationRequirement::Blocked => "n/Esc only",
        };
    }
    match plan.confirmation.requirement {
        ConfirmationRequirement::NormalSelection => "none; runs on selection",
        ConfirmationRequirement::VisiblePlan => "y confirm; n/Esc cancel",
        ConfirmationRequirement::ExplicitConfirmation => "Y confirm; n/Esc cancel",
        ConfirmationRequirement::Blocked => "n/Esc dismiss; confirm disabled",
    }
}

fn accepted_confirmation_keys(plan: &OperationPlan) -> &'static str {
    match plan.confirmation.requirement {
        ConfirmationRequirement::NormalSelection => "none; runs on selection",
        ConfirmationRequirement::VisiblePlan => "y confirm; n/Esc cancel",
        ConfirmationRequirement::ExplicitConfirmation => "uppercase Y confirm; n/Esc cancel",
        ConfirmationRequirement::Blocked => "n/Esc dismiss; confirm disabled",
    }
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
    format!(
        "{} [{}]",
        confirmation_action(plan),
        accepted_confirmation_keys(plan)
    )
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

struct StartupPolicy {
    policy: EffectivePolicy,
    diagnostic: Option<String>,
}

fn startup_policy_from_environment() -> StartupPolicy {
    match StorePaths::from_environment() {
        Ok(paths) => startup_policy_from_paths(&paths),
        Err(error) => StartupPolicy::failed(error.to_string()),
    }
}

fn startup_policy_from_paths(paths: &StorePaths) -> StartupPolicy {
    match LocalStore::open(paths.clone()) {
        Ok(store) => {
            let loaded = store.load_config();
            StartupPolicy {
                policy: EffectivePolicy::new(&loaded.settings),
                diagnostic: loaded.diagnostic.map(|diagnostic| diagnostic.to_string()),
            }
        }
        Err(error) => StartupPolicy::failed(error.to_string()),
    }
}

impl StartupPolicy {
    fn failed(error: String) -> Self {
        Self {
            policy: EffectivePolicy::safe_fallback(),
            diagnostic: Some(format!(
                "configuration could not be loaded: {error}; using the safe fallback configuration"
            )),
        }
    }
}

#[derive(Debug)]
struct PolicyReload {
    policy: EffectivePolicy,
    diagnostic: Option<String>,
}

fn policy_reload_from_paths(paths: &StorePaths, fallback: &EffectivePolicy) -> PolicyReload {
    match LocalStore::open(paths.clone()) {
        Ok(store) => {
            let loaded = store.load_config();
            match loaded.diagnostic {
                Some(diagnostic) => PolicyReload {
                    policy: fallback.clone(),
                    diagnostic: Some(format!(
                        "{}; retaining the last valid policy until the configuration is fixed",
                        diagnostic
                            .to_string()
                            .trim_end_matches("; using the safe fallback configuration")
                    )),
                },
                None => PolicyReload {
                    policy: EffectivePolicy::new(&loaded.settings),
                    diagnostic: None,
                },
            }
        }
        Err(error) => PolicyReload {
            policy: fallback.clone(),
            diagnostic: Some(format!(
                "configuration could not be reloaded: {error}; retaining the last valid policy until the configuration is fixed"
            )),
        },
    }
}

fn operation_message(action: &str, result: Result<(), String>) -> String {
    match result {
        Ok(()) => format!("{action} succeeded"),
        Err(error) => format!("{action} failed: {error}"),
    }
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

fn policy_branch(context: &ExecutionContext) -> Option<&str> {
    match context.payload.as_ref()? {
        PendingPayload::Commit { target, .. }
        | PendingPayload::Rebase { target, .. }
        | PendingPayload::Merge { target, .. } => head_target_branch(target),
        PendingPayload::Push { local_branch, .. }
        | PendingPayload::Pull { local_branch, .. }
        | PendingPayload::PullRebase { local_branch, .. } => Some(local_branch),
        PendingPayload::PushSetUpstream { branch, .. } => Some(branch),
        PendingPayload::StagePaths { .. }
        | PendingPayload::UnstagePaths { .. }
        | PendingPayload::StageAll
        | PendingPayload::UnstageAll
        | PendingPayload::Checkout { .. }
        | PendingPayload::CreateBranch { .. }
        | PendingPayload::OpenPullRequest { .. } => None,
    }
}

fn prepared_pull_request_target(
    operation: &PreparedOperation,
) -> Option<DeferredPullRequestTarget> {
    let PendingPayload::OpenPullRequest {
        base,
        repository,
        github_repository,
        head_github_repository,
        head_repository,
        head,
        ..
    } = operation.context.payload.as_ref()?
    else {
        return None;
    };
    Some(DeferredPullRequestTarget {
        repository: GitHubRepository::new(github_repository.hostname(), repository),
        head_repository: GitHubRepository::new(head_github_repository.hostname(), head_repository),
        head: head.clone(),
        base: base.clone(),
    })
}

fn head_target_branch(target: &HeadTarget) -> Option<&str> {
    target.reference.as_deref()?.strip_prefix("refs/heads/")
}

#[derive(Debug, Clone)]
struct PlanExecutor {
    repo_root: PathBuf,
    audit: AuditDestination,
    policy: EffectivePolicy,
    #[cfg(test)]
    ssh_executable: Option<PathBuf>,
}

impl PlanExecutor {
    fn current(policy: EffectivePolicy) -> Self {
        Self {
            repo_root: current_dir(),
            audit: AuditDestination::Environment,
            policy,
            #[cfg(test)]
            ssh_executable: None,
        }
    }

    #[cfg(test)]
    fn with_audit_paths(repo_root: impl Into<PathBuf>, paths: StorePaths) -> Self {
        Self {
            repo_root: repo_root.into(),
            audit: AuditDestination::Paths(paths),
            policy: EffectivePolicy::default(),
            ssh_executable: None,
        }
    }

    #[cfg(test)]
    fn with_audit_paths_and_ssh(
        repo_root: impl Into<PathBuf>,
        paths: StorePaths,
        ssh_executable: impl Into<PathBuf>,
    ) -> Self {
        Self {
            repo_root: repo_root.into(),
            audit: AuditDestination::Paths(paths),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(ssh_executable.into()),
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

        if let Err(error) = self.validate_policy(plan, &context) {
            return PlanExecutionResult {
                planned_step_count: plan.steps.len(),
                step_results: Vec::new(),
                plan_error: Some(error),
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

    fn validate_policy(
        &self,
        plan: &OperationPlan,
        context: &ExecutionContext,
    ) -> Result<(), String> {
        if let Some(step) = plan
            .steps
            .iter()
            .find(|step| self.policy.is_operation_disabled(step.kind))
        {
            return Err(disabled_operation_message(step.kind));
        }
        if plan.confirmation.requirement == ConfirmationRequirement::Blocked {
            return Err("operation is blocked by the policy captured in the preview".to_owned());
        }
        let current_policy = evaluate_plan_policy(&self.policy, plan, policy_branch(context));
        if current_policy.requirement > plan.confirmation.requirement {
            return Err(format!(
                "policy now requires {}; review the updated plan before execution",
                confirmation_requirement_label(current_policy.requirement)
            ));
        }
        let previewed_reasons: Vec<String> = plan
            .confirmation
            .reason
            .as_deref()
            .map(|reasons| reasons.split("; ").map(ToOwned::to_owned).collect())
            .unwrap_or_default();
        if !protected_policy_reasons_covered(&previewed_reasons, &current_policy) {
            return Err(
                "policy now applies new protected-branch rules; review the updated plan before execution"
                    .to_owned(),
            );
        }
        Ok(())
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
        #[cfg(test)]
        if let Some(executable) = &self.ssh_executable {
            return Git::with_ssh_executable(self.repo_root.clone(), executable);
        }
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
            upstream_branch,
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
        validate_push_plan(git, branch, None, target, remote, branch, remote_urls)
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
            tracking_oid,
            upstream_oid,
        } = typed_payload(context, OperationKind::PullFastForward)?
        else {
            return Err(mismatched_context(OperationKind::PullFastForward));
        };
        validate_pull_plan(git, false, local_branch, upstream, target)
            .map_err(StepExecutionError::Blocked)?;
        let upstream_oid = upstream_oid.as_deref().ok_or_else(|| {
            StepExecutionError::Blocked("Pull blocked: fetched upstream is unavailable.".to_owned())
        })?;
        git.publish_remote_tracking(
            remote,
            upstream_branch,
            upstream_oid,
            tracking_oid.as_deref(),
        )
        .map_err(StepExecutionError::Git)?;
        git_output(git.pull_ff_only_from(remote, upstream_branch, Some(upstream_oid), target))
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
            tracking_oid,
            upstream_oid,
        } = typed_payload(context, OperationKind::PullRebase)?
        else {
            return Err(mismatched_context(OperationKind::PullRebase));
        };
        validate_pull_plan(git, true, local_branch, upstream, target)
            .map_err(StepExecutionError::Blocked)?;
        let upstream_oid = upstream_oid.as_deref().ok_or_else(|| {
            StepExecutionError::Blocked(
                "Pull rebase blocked: fetched upstream is unavailable.".to_owned(),
            )
        })?;
        git.publish_remote_tracking(
            remote,
            upstream_branch,
            upstream_oid,
            tracking_oid.as_deref(),
        )
        .map_err(StepExecutionError::Git)?;
        git_output(git.pull_rebase_from(remote, upstream_branch, Some(upstream_oid), target))
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
            upstream_branch,
            remote_urls,
            target,
            base,
            title,
            repository,
            github_repository,
            head_github_repository,
            head_repository,
            head,
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
        validate_open_pull_request_plan(
            git,
            branch,
            upstream,
            remote,
            upstream_branch,
            remote_urls,
            target,
        )?;
        if !github_base_repository(git, remote, head_github_repository)
            .map_err(StepExecutionError::Blocked)?
            .eq_ignore_ascii_case(github_repository)
        {
            return Err(StepExecutionError::Blocked(
                "Open pull request blocked: GitHub base repository changed since the plan was shown."
                    .to_owned(),
            ));
        }
        let github = match github_executable {
            Some(executable) => GitHub::with_executable_and_repository(
                &self.repo_root,
                executable,
                github_repository.hostname(),
                github_repository.name_with_owner(),
            ),
            None => GitHub::with_executable_and_repository(
                &self.repo_root,
                "gh",
                github_repository.hostname(),
                github_repository.name_with_owner(),
            ),
        };
        let current_repository = github.repository().map_err(StepExecutionError::GitHub)?;
        if !current_repository
            .name_with_owner
            .eq_ignore_ascii_case(repository)
        {
            return Err(StepExecutionError::Blocked(
                "Open pull request blocked: GitHub repository changed since the plan was shown."
                    .to_owned(),
            ));
        }
        let head_github = match github_executable {
            Some(executable) => GitHub::with_executable_and_repository(
                &self.repo_root,
                executable,
                head_github_repository.hostname(),
                head_github_repository.name_with_owner(),
            ),
            None => GitHub::with_executable_and_repository(
                &self.repo_root,
                "gh",
                head_github_repository.hostname(),
                head_github_repository.name_with_owner(),
            ),
        };
        let current_head_repository =
            if head_github_repository.eq_ignore_ascii_case(github_repository) {
                current_repository.clone()
            } else {
                head_github
                    .repository()
                    .map_err(StepExecutionError::GitHub)?
            };
        if !current_head_repository
            .name_with_owner
            .eq_ignore_ascii_case(head_repository)
        {
            return Err(StepExecutionError::Blocked(
                "Open pull request blocked: GitHub head repository changed since the plan was shown."
                    .to_owned(),
            ));
        }
        let local_oid = target.oid.as_deref().ok_or_else(|| {
            StepExecutionError::Blocked(
                "Open pull request blocked: planned branch has no commit.".to_owned(),
            )
        })?;
        let remote_oid = head_github
            .branch_oid(upstream_branch)
            .map_err(StepExecutionError::GitHub)?;
        if remote_oid.as_deref() != Some(local_oid) {
            return Err(StepExecutionError::Blocked(
                "Open pull request blocked: current branch is no longer pushed to its upstream."
                    .to_owned(),
            ));
        }
        let current_head = pull_request_head(
            &GitHubRepository::new(
                head_github_repository.hostname(),
                &current_head_repository.name_with_owner,
            ),
            &GitHubRepository::new(
                github_repository.hostname(),
                &current_repository.name_with_owner,
            ),
            upstream_branch,
        )
        .map_err(StepExecutionError::Blocked)?;
        if current_head != *head {
            return Err(StepExecutionError::Blocked(
                "Open pull request blocked: GitHub head changed since the plan was shown."
                    .to_owned(),
            ));
        }
        let existing = github
            .existing_pull_requests(head)
            .map_err(StepExecutionError::GitHub)?;
        if let Some(existing) =
            matching_pull_request(&existing, base, upstream_branch, head_repository)
        {
            return Ok(ExecutionOutput::PullRequest {
                url: existing.url.clone(),
                existing: true,
            });
        }
        if !github
            .branch_exists(base)
            .map_err(StepExecutionError::GitHub)?
        {
            return Err(StepExecutionError::Blocked(format!(
                "Open pull request blocked: base branch {base} no longer exists in {repository}."
            )));
        }
        match github.create_pull_request(&CreatePullRequest {
            title: title.clone(),
            body: String::new(),
            base: base.clone(),
            head: head.clone(),
        }) {
            Ok(created) => Ok(ExecutionOutput::PullRequest {
                url: created.url,
                existing: false,
            }),
            Err(error) => match github.existing_pull_requests(head) {
                Ok(existing) => {
                    matching_pull_request(&existing, base, upstream_branch, head_repository)
                        .map(|pull_request| ExecutionOutput::PullRequest {
                            url: pull_request.url.clone(),
                            existing: true,
                        })
                        .ok_or(StepExecutionError::GitHub(error))
                }
                Err(_) => Err(StepExecutionError::GitHub(error)),
            },
        }
    }
}

#[derive(Debug, Clone)]
struct PromptSequenceExecutor {
    repo_root: PathBuf,
    audit: AuditDestination,
    github_executable: Option<PathBuf>,
    policy: EffectivePolicy,
    #[cfg(test)]
    ssh_executable: Option<PathBuf>,
}

impl PromptSequenceExecutor {
    fn current(policy: EffectivePolicy) -> Self {
        Self {
            repo_root: current_dir(),
            audit: AuditDestination::Environment,
            github_executable: None,
            policy,
            #[cfg(test)]
            ssh_executable: None,
        }
    }

    #[cfg(test)]
    fn with_audit_paths(repo_root: impl Into<PathBuf>, paths: StorePaths) -> Self {
        Self {
            repo_root: repo_root.into(),
            audit: AuditDestination::Paths(paths),
            github_executable: None,
            policy: EffectivePolicy::default(),
            ssh_executable: None,
        }
    }

    #[cfg(test)]
    fn with_audit_paths_and_tools(
        repo_root: impl Into<PathBuf>,
        paths: StorePaths,
        github_executable: impl Into<PathBuf>,
        ssh_executable: impl Into<PathBuf>,
    ) -> Self {
        Self {
            repo_root: repo_root.into(),
            audit: AuditDestination::Paths(paths),
            github_executable: Some(github_executable.into()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(ssh_executable.into()),
        }
    }

    #[cfg(test)]
    fn execute(&self, sequence: QueuedPromptSequence) -> PromptSequenceExecutionResult {
        self.execute_confirmed(sequence, ConfirmationRequirement::ExplicitConfirmation)
    }

    fn execute_confirmed(
        &self,
        sequence: QueuedPromptSequence,
        confirmed_requirement: ConfirmationRequirement,
    ) -> PromptSequenceExecutionResult {
        let total_steps = sequence.total_steps();
        let QueuedPromptSequence {
            first,
            remaining_requests,
            deferred_pull_request_targets,
            policy_evaluations,
            sequence_policy_evaluations,
        } = sequence;
        let mut step_results = Vec::with_capacity(total_steps);

        let mut current_policy = self.load_policy(&self.policy);
        let git = Git::new(self.repo_root.clone());
        let requests = std::iter::once(first.plan.request.clone())
            .chain(remaining_requests.iter().cloned())
            .collect::<Vec<_>>();
        let branch = policy_branch(&first.context)
            .map(ToOwned::to_owned)
            .or_else(|| {
                git.status()
                    .ok()
                    .and_then(|status| branch_name(&status.branch).ok())
            });
        let current_pull_request_targets = OperationPlanner {
            repo_root: self.repo_root.clone(),
            github_executable: self.github_executable.clone(),
            policy: current_policy.clone(),
            #[cfg(test)]
            ssh_executable: self.ssh_executable.clone(),
        }
        .deferred_pull_request_targets(&requests, branch.clone());
        if current_pull_request_targets.as_ref() != Ok(&deferred_pull_request_targets) {
            step_results.push(PromptSequenceStepResult::planning_failed(
                1,
                first.plan.title.clone(),
                "pull request target changed since the sequence preview; create a new sequence preview and confirmation"
                    .to_owned(),
            ));
            return PromptSequenceExecutionResult::new(total_steps, step_results, current_policy);
        }
        let (current_policies, current_sequence_policies) =
            match prompt_sequence_policy_evaluations(&current_policy, &git, &requests, branch) {
                Ok(evaluations) => evaluations,
                Err(error) => {
                    step_results.push(PromptSequenceStepResult::planning_failed(
                        1,
                        first.plan.title.clone(),
                        format!("policy requires a new sequence preview and confirmation: {error}"),
                    ));
                    return PromptSequenceExecutionResult::new(
                        total_steps,
                        step_results,
                        current_policy,
                    );
                }
            };
        if !sequence_policy_confirmation_covers(
            confirmed_requirement,
            &policy_evaluations,
            &current_policies,
        ) || !sequence_policy_confirmation_covers(
            confirmed_requirement,
            &sequence_policy_evaluations,
            &current_sequence_policies,
        ) {
            step_results.push(PromptSequenceStepResult::planning_failed(
                1,
                first.plan.title.clone(),
                "policy requires a new sequence preview and confirmation".to_owned(),
            ));
            return PromptSequenceExecutionResult::new(total_steps, step_results, current_policy);
        }

        let mut previewed_policies = policy_evaluations.into_iter();
        let mut previewed_sequence_policies = sequence_policy_evaluations.into_iter();
        let mut previewed_pull_request_targets = deferred_pull_request_targets.into_iter();

        let (Some(previewed_policy), Some(previewed_sequence_policy)) = (
            previewed_policies.next(),
            previewed_sequence_policies.next(),
        ) else {
            step_results.push(PromptSequenceStepResult::planning_failed(
                1,
                first.plan.title.clone(),
                "sequence preview is missing policy context".to_owned(),
            ));
            return PromptSequenceExecutionResult::new(total_steps, step_results, current_policy);
        };
        let branch = policy_branch(&first.context);
        let current_evaluation = evaluate_plan_policy(&current_policy, &first.plan, branch);
        let current_sequence_evaluation =
            evaluate_sequence_plan_policy(&current_policy, &first.plan, branch);
        if !confirmation_covers(
            &first.plan,
            confirmed_requirement,
            &previewed_policy,
            &current_evaluation,
        ) || !confirmation_covers(
            &first.plan,
            confirmed_requirement,
            &previewed_sequence_policy,
            &current_sequence_evaluation,
        ) {
            step_results.push(PromptSequenceStepResult::planning_failed(
                1,
                first.plan.title.clone(),
                "policy requires a new sequence preview and confirmation".to_owned(),
            ));
            return PromptSequenceExecutionResult::new(total_steps, step_results, current_policy);
        }
        let executor = PlanExecutor {
            repo_root: self.repo_root.clone(),
            audit: self.audit.clone(),
            policy: current_policy.clone(),
            #[cfg(test)]
            ssh_executable: self.ssh_executable.clone(),
        };
        let first_result = Self::execute_prepared_step(&executor, 1, first);
        let first_succeeded = first_result.succeeded;
        step_results.push(first_result);
        if !first_succeeded {
            return PromptSequenceExecutionResult::new(total_steps, step_results, current_policy);
        }

        for (index, request) in remaining_requests.into_iter().enumerate() {
            let step_number = index + 2;
            let Some(previewed_pull_request_target) = previewed_pull_request_targets.next() else {
                step_results.push(PromptSequenceStepResult::planning_failed(
                    step_number,
                    prompt_sequence_request_title(&request),
                    "sequence preview is missing pull request target context".to_owned(),
                ));
                return PromptSequenceExecutionResult::new(
                    total_steps,
                    step_results,
                    current_policy,
                );
            };
            current_policy = self.load_policy(&current_policy);
            let planner = OperationPlanner {
                repo_root: self.repo_root.clone(),
                github_executable: self.github_executable.clone(),
                policy: current_policy.clone(),
                #[cfg(test)]
                ssh_executable: self.ssh_executable.clone(),
            };
            let operation = match planner.plan_request(request.clone()) {
                Ok(operation) => operation,
                Err(error) => {
                    let error = if previewed_pull_request_target.is_some() {
                        format!(
                            "pull request target must be re-previewed and confirmed before execution: {error}"
                        )
                    } else {
                        error
                    };
                    step_results.push(PromptSequenceStepResult::planning_failed(
                        step_number,
                        prompt_sequence_request_title(&request),
                        error,
                    ));
                    return PromptSequenceExecutionResult::new(
                        total_steps,
                        step_results,
                        current_policy,
                    );
                }
            };
            if let Some(previewed_target) = previewed_pull_request_target {
                if prepared_pull_request_target(&operation).as_ref() != Some(&previewed_target) {
                    step_results.push(PromptSequenceStepResult::planning_failed(
                        step_number,
                        operation.plan.title.clone(),
                        "pull request target changed since the sequence preview; create a new sequence preview and confirmation"
                            .to_owned(),
                    ));
                    return PromptSequenceExecutionResult::new(
                        total_steps,
                        step_results,
                        current_policy,
                    );
                }
            }
            let (Some(previewed_policy), Some(previewed_sequence_policy)) = (
                previewed_policies.next(),
                previewed_sequence_policies.next(),
            ) else {
                step_results.push(PromptSequenceStepResult::planning_failed(
                    step_number,
                    operation.plan.title.clone(),
                    "sequence preview is missing policy context".to_owned(),
                ));
                return PromptSequenceExecutionResult::new(
                    total_steps,
                    step_results,
                    current_policy,
                );
            };
            current_policy = self.load_policy(&current_policy);
            let branch = policy_branch(&operation.context);
            let current_evaluation = evaluate_plan_policy(&current_policy, &operation.plan, branch);
            let current_sequence_evaluation =
                evaluate_sequence_plan_policy(&current_policy, &operation.plan, branch);
            if !confirmation_covers(
                &operation.plan,
                confirmed_requirement,
                &previewed_policy,
                &current_evaluation,
            ) || !confirmation_covers(
                &operation.plan,
                confirmed_requirement,
                &previewed_sequence_policy,
                &current_sequence_evaluation,
            ) {
                step_results.push(PromptSequenceStepResult::planning_failed(
                    step_number,
                    operation.plan.title.clone(),
                    "policy requires a new sequence preview and confirmation".to_owned(),
                ));
                return PromptSequenceExecutionResult::new(
                    total_steps,
                    step_results,
                    current_policy,
                );
            }
            let executor = PlanExecutor {
                repo_root: self.repo_root.clone(),
                audit: self.audit.clone(),
                policy: current_policy.clone(),
                #[cfg(test)]
                ssh_executable: self.ssh_executable.clone(),
            };
            let step = Self::execute_prepared_step(&executor, step_number, operation);
            let succeeded = step.succeeded;
            step_results.push(step);
            if !succeeded {
                return PromptSequenceExecutionResult::new(
                    total_steps,
                    step_results,
                    current_policy,
                );
            }
        }

        PromptSequenceExecutionResult::new(total_steps, step_results, current_policy)
    }

    fn load_policy(&self, fallback: &EffectivePolicy) -> EffectivePolicy {
        self.audit.load_policy(fallback)
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
    policy: EffectivePolicy,
}

impl PromptSequenceExecutionResult {
    fn new(
        total_steps: usize,
        step_results: Vec<PromptSequenceStepResult>,
        policy: EffectivePolicy,
    ) -> Self {
        Self {
            total_steps,
            step_results,
            policy,
        }
    }

    fn policy(&self) -> &EffectivePolicy {
        &self.policy
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
    fn load_policy(&self, fallback: &EffectivePolicy) -> EffectivePolicy {
        self.reload_policy(fallback).policy
    }

    fn reload_policy(&self, fallback: &EffectivePolicy) -> PolicyReload {
        match self {
            Self::Environment => match StorePaths::from_environment() {
                Ok(paths) => policy_reload_from_paths(&paths, fallback),
                Err(error) => PolicyReload {
                    policy: fallback.clone(),
                    diagnostic: Some(format!(
                        "configuration paths could not be resolved: {error}; retaining the last valid policy until the configuration is fixed"
                    )),
                },
            },
            #[cfg(test)]
            Self::Paths(paths) => policy_reload_from_paths(paths, fallback),
        }
    }

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
        GhError::MissingCli | GhError::NotAuthenticated { .. } | GhError::InvalidInput { .. } => {
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
    use bitbygit_core::config::{AppConfig, ConfirmationSetting, OperationFamily};
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
    fn disabled_prompt_cannot_bypass_operation_policy_or_parse_raw_commands() {
        let mut config = AppConfig::default();
        config.prompt.enabled = false;
        config.policy.disabled_operations = vec![OperationFamily::Fetch];
        let mut app = App::with_policy(EffectivePolicy::new(&config));
        app.focus = Focus::Prompt;
        app.prompt = "fetch".to_owned();

        app.submit_prompt();

        assert_eq!(app.details, "Prompt input is disabled by policy.");
        assert_eq!(app.prompt, "fetch");
        assert_eq!(app.operation_queue.pending(), None);

        config.prompt.enabled = true;
        app.policy = EffectivePolicy::new(&config);
        app.submit_prompt();
        assert!(app.details.contains("fetch is disabled by policy"));
        assert_eq!(app.operation_queue.pending(), None);

        app.prompt = "git fetch".to_owned();
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
            let expected = OperationPlanner::current(EffectivePolicy::default())
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
    fn prompt_sequence_stops_before_push_and_pr_when_commit_execution_fails()
    -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("prompt-sequence-commit-fails")?;
        let fake_gh = fake_gh("prompt-sequence-commit-fails", false)?;
        std::fs::write(repo.join("file.txt"), "hello\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        let sequence = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        }
        .plan_prompt_sequence(vec![
            OperationRequest::Commit {
                message: "ship staged".to_owned(),
            },
            OperationRequest::Push,
            OperationRequest::OpenPullRequest { base: None },
        ])
        .map_err(std::io::Error::other)?;
        std::fs::write(repo.join("file.txt"), "changed after preview\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        let paths = isolated_store_paths("prompt-sequence-commit-fails-audit")?;

        let result = PromptSequenceExecutor::with_audit_paths_and_tools(
            &repo,
            paths.clone(),
            &fake_gh,
            test_ssh_command()?,
        )
        .execute(sequence.sequence);

        assert!(
            result
                .message()
                .contains("Prompt sequence stopped after step 1 of 3.")
        );
        assert!(
            !std::fs::read_to_string(fake_gh.with_file_name("invocations"))?.contains("pr:create")
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
    fn prompt_sequence_stops_before_pr_when_push_fails() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("prompt-sequence-push-fails")?;
        let fake_gh = fake_gh("prompt-sequence-push-fails", false)?;
        let ssh = test_ssh_command()?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(ssh.clone()),
        };
        let sequence = planner
            .plan_prompt_sequence(vec![
                OperationRequest::Push,
                OperationRequest::OpenPullRequest { base: None },
            ])
            .map_err(std::io::Error::other)?;
        let remote = git_stdout(&repo, &["config", "--get", "remote.origin.testbare"])?;
        std::fs::remove_dir_all(remote.trim())?;
        let paths = isolated_store_paths("prompt-sequence-push-fails-audit")?;

        let result =
            PromptSequenceExecutor::with_audit_paths_and_tools(&repo, paths.clone(), &fake_gh, ssh)
                .execute(sequence.sequence);

        assert!(
            result
                .message()
                .contains("Prompt sequence stopped after step 1 of 2.")
        );
        assert!(
            !std::fs::read_to_string(fake_gh.with_file_name("invocations"))?.contains("pr:create")
        );
        let entries = LocalStore::open(paths)?.list_audit_entries()?;
        let operations = entries
            .iter()
            .map(|entry| entry.operation.as_str())
            .collect::<Vec<_>>();
        assert_eq!(operations, vec!["push", "push"]);
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
    fn commit_push_pr_sequence_replans_and_creates_pr() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("prompt-sequence-open-pr")?;
        let fake_gh = fake_gh("prompt-sequence-open-pr", false)?;
        let ssh = test_ssh_command()?;
        std::fs::write(repo.join("file.txt"), "ready for review\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(ssh.clone()),
        };

        let ParsedPrompt::Sequence(requests) =
            parse_prompt("commit -m \"ship review\" and push and open pr")?
        else {
            return Err(std::io::Error::other("expected prompt sequence").into());
        };
        let sequence = planner
            .plan_prompt_sequence(requests)
            .map_err(std::io::Error::other)?;

        let preview = sequence.plan.preview_text();
        assert!(preview.contains("1. commit 1 staged file(s)"));
        assert!(preview.contains("2. push current branch"));
        assert!(preview.contains("3. open or surface pull request"));
        assert!(preview.contains("repository: github.com/octo/repo"));
        assert!(preview.contains("base: main"));
        assert!(preview.contains("planned after step 2 succeeds"));
        let paths = isolated_store_paths("prompt-sequence-open-pr-audit")?;
        let result =
            PromptSequenceExecutor::with_audit_paths_and_tools(&repo, paths.clone(), &fake_gh, ssh)
                .execute(sequence.sequence);

        let message = result.message();
        assert!(message.contains("Prompt sequence completed 3 step(s)."));
        assert!(message.contains("Pull request created: https://github.com/octo/repo/pull/43"));
        let invocations = std::fs::read_to_string(fake_gh.with_file_name("invocations"))?;
        assert!(invocations.contains("pr:create"));
        let entries = LocalStore::open(paths)?.list_audit_entries()?;
        let operations = entries
            .iter()
            .map(|entry| entry.operation.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            vec![
                "commit",
                "commit",
                "push",
                "push",
                "open_pull_request",
                "open_pull_request"
            ]
        );
        assert!(
            entries
                .iter()
                .filter(|entry| entry.operation == "open_pull_request")
                .all(|entry| !entry.message.contains("pull/43")
                    && !entry.message.contains("feature/open-pr"))
        );
        Ok(())
    }

    #[test]
    fn remote_checkout_push_pr_revalidates_target_before_any_step() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("prompt-sequence-remote-checkout-pr")?;
        let fork = isolated_bare_git_repo("prompt-sequence-remote-checkout-pr-fork")?;
        configure_git_identity(&repo)?;
        std::fs::write(repo.join("file.txt"), "base\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "initial"])?;
        let original_branch = git_stdout(&repo, &["branch", "--show-current"])?;
        git_stdout(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "ssh://git@github.com/upstream/repo.git",
            ],
        )?;
        git_stdout(
            &repo,
            &[
                "remote",
                "add",
                "upstream",
                "ssh://git@github.com/upstream/repo.git",
            ],
        )?;
        add_github_remote(&repo, "fork", &fork)?;
        git_stdout(&repo, &["update-ref", "refs/remotes/fork/topic", "HEAD"])?;
        let fake_gh = fake_gh("prompt-sequence-remote-checkout-pr", false)?;
        let ssh = test_ssh_command()?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(ssh.clone()),
        };
        let sequence = planner
            .plan_prompt_sequence(vec![
                OperationRequest::Checkout {
                    branch: "fork/topic".to_owned(),
                },
                OperationRequest::Push,
                OperationRequest::OpenPullRequest { base: None },
            ])
            .map_err(std::io::Error::other)?;

        let preview = sequence.plan.preview_text();
        assert!(preview.contains("repository: github.com/upstream/repo"));
        assert!(preview.contains("head: octo:topic"));
        assert!(preview.contains(
            "target: https://github.com/upstream/repo/compare/main...octo%3Atopic?expand=1"
        ));
        git_stdout(&repo, &["config", "remote.pushDefault", "origin"])?;
        let paths = isolated_store_paths("prompt-sequence-remote-checkout-pr-audit")?;

        let result =
            PromptSequenceExecutor::with_audit_paths_and_tools(&repo, paths.clone(), &fake_gh, ssh)
                .execute(sequence.sequence);

        let message = result.message();
        assert!(
            message.contains("Prompt sequence stopped before step 1 of 3."),
            "{message}"
        );
        assert!(
            message.contains("pull request target changed since the sequence preview"),
            "{message}"
        );
        assert_eq!(
            git_stdout(&repo, &["branch", "--show-current"])?,
            original_branch
        );
        assert!(LocalStore::open(paths)?.list_audit_entries()?.is_empty());
        assert!(
            !std::process::Command::new("git")
                .arg("--git-dir")
                .arg(&fork)
                .args(["show-ref", "--verify", "--quiet", "refs/heads/topic"])
                .status()?
                .success(),
            "push must not run before the changed pull request target is rejected"
        );
        Ok(())
    }

    #[test]
    fn remote_checkout_push_pr_uses_configured_push_destination() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("prompt-sequence-remote-checkout-push-target")?;
        let origin = isolated_bare_git_repo("prompt-sequence-remote-checkout-origin")?;
        configure_git_identity(&repo)?;
        std::fs::write(repo.join("file.txt"), "base\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "initial"])?;
        git_stdout(
            &repo,
            &["remote", "add", "fork", "ssh://git@github.com/bob/repo.git"],
        )?;
        git_stdout(&repo, &["update-ref", "refs/remotes/fork/topic", "HEAD"])?;
        add_github_remote(&repo, "origin", &origin)?;
        git_stdout(&repo, &["config", "remote.pushDefault", "origin"])?;
        let fake_gh = fake_gh("prompt-sequence-remote-checkout-push-target", false)?;
        let ssh = test_ssh_command()?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(ssh.clone()),
        };

        let sequence = planner
            .plan_prompt_sequence(vec![
                OperationRequest::Checkout {
                    branch: "fork/topic".to_owned(),
                },
                OperationRequest::Push,
                OperationRequest::OpenPullRequest { base: None },
            ])
            .map_err(std::io::Error::other)?;

        let preview = sequence.plan.preview_text();
        assert!(
            preview.contains("repository: github.com/octo/repo"),
            "{preview}"
        );
        assert!(preview.contains("head: topic"), "{preview}");
        let paths = isolated_store_paths("prompt-sequence-remote-checkout-push-target-audit")?;
        let result =
            PromptSequenceExecutor::with_audit_paths_and_tools(&repo, paths, &fake_gh, ssh)
                .execute(sequence.sequence);

        assert!(
            result.message().contains("completed 3 step(s)"),
            "{}",
            result.message()
        );
        let local_head = git_stdout(&repo, &["rev-parse", "HEAD"])?;
        let pushed = git_stdout(&origin, &["rev-parse", "refs/heads/topic"])?;
        assert_eq!(pushed.trim(), local_head.trim());
        let invocations = std::fs::read_to_string(fake_gh.with_file_name("invocations"))?;
        assert!(invocations.contains("--head topic"), "{invocations}");
        Ok(())
    }

    #[test]
    fn create_branch_push_pr_uses_branch_push_remote() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("prompt-sequence-create-branch-push-target")?;
        let fork = isolated_bare_git_repo("prompt-sequence-create-branch-fork")?;
        configure_git_identity(&repo)?;
        std::fs::write(repo.join("file.txt"), "base\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "initial"])?;
        git_stdout(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "ssh://git@github.com/upstream/repo.git",
            ],
        )?;
        git_stdout(
            &repo,
            &[
                "remote",
                "add",
                "upstream",
                "ssh://git@github.com/upstream/repo.git",
            ],
        )?;
        add_github_remote(&repo, "fork", &fork)?;
        git_stdout(&repo, &["config", "branch.feature/new.pushRemote", "fork"])?;
        let fake_gh = fake_gh("prompt-sequence-create-branch-push-target", false)?;
        let ssh = test_ssh_command()?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(ssh.clone()),
        };

        let sequence = planner
            .plan_prompt_sequence(vec![
                OperationRequest::CreateBranch {
                    branch: "feature/new".to_owned(),
                    base: None,
                },
                OperationRequest::Push,
                OperationRequest::OpenPullRequest { base: None },
            ])
            .map_err(std::io::Error::other)?;

        let preview = sequence.plan.preview_text();
        assert!(
            preview.contains("repository: github.com/upstream/repo"),
            "{preview}"
        );
        assert!(preview.contains("head: octo:feature/new"), "{preview}");
        let paths = isolated_store_paths("prompt-sequence-create-branch-push-target-audit")?;
        let result =
            PromptSequenceExecutor::with_audit_paths_and_tools(&repo, paths, &fake_gh, ssh)
                .execute(sequence.sequence);

        assert!(
            result.message().contains("completed 3 step(s)"),
            "{}",
            result.message()
        );
        let local_head = git_stdout(&repo, &["rev-parse", "HEAD"])?;
        let pushed = git_stdout(&fork, &["rev-parse", "refs/heads/feature/new"])?;
        assert_eq!(pushed.trim(), local_head.trim());
        let invocations = std::fs::read_to_string(fake_gh.with_file_name("invocations"))?;
        assert!(
            invocations.contains("--head octo:feature/new"),
            "{invocations}"
        );
        Ok(())
    }

    #[test]
    fn deferred_matching_base_and_head_fails_before_any_step() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("prompt-sequence-matching-base-head")?;
        let origin = isolated_bare_git_repo("prompt-sequence-matching-base-head-origin")?;
        configure_git_identity(&repo)?;
        std::fs::write(repo.join("file.txt"), "base\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "initial"])?;
        let original_branch = git_stdout(&repo, &["branch", "--show-current"])?;
        git_stdout(&repo, &["update-ref", "refs/remotes/origin/topic", "HEAD"])?;
        add_github_remote(&repo, "origin", &origin)?;
        let fake_gh = fake_gh("prompt-sequence-matching-base-head", false)?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };

        let error = match planner.plan_prompt_sequence(vec![
            OperationRequest::Checkout {
                branch: "origin/topic".to_owned(),
            },
            OperationRequest::Push,
            OperationRequest::OpenPullRequest {
                base: Some("topic".to_owned()),
            },
        ]) {
            Ok(_) => {
                return Err(std::io::Error::other(
                    "matching base and head must fail during sequence preflight",
                )
                .into());
            }
            Err(error) => error,
        };

        assert!(error.contains("base branch must differ"), "{error}");
        assert_eq!(
            git_stdout(&repo, &["branch", "--show-current"])?,
            original_branch
        );
        assert!(
            !std::process::Command::new("git")
                .arg("--git-dir")
                .arg(&origin)
                .args(["show-ref", "--verify", "--quiet", "refs/heads/topic"])
                .status()?
                .success()
        );
        let paths = isolated_store_paths("prompt-sequence-matching-base-head-audit")?;
        assert!(LocalStore::open(paths)?.list_audit_entries()?.is_empty());
        Ok(())
    }

    #[test]
    fn deferred_configured_pull_request_base_is_previewed_and_executed()
    -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("prompt-sequence-configured-pr-base")?;
        let fake_gh = fake_gh("prompt-sequence-configured-pr-base", false)?;
        let ssh = test_ssh_command()?;
        let mut config = AppConfig::default();
        config.pull_requests.default_base_branch = Some("release".to_owned());
        let policy = EffectivePolicy::new(&config);
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: policy.clone(),
            ssh_executable: Some(ssh.clone()),
        };

        let sequence = planner
            .plan_prompt_sequence(vec![
                OperationRequest::Fetch,
                OperationRequest::OpenPullRequest { base: None },
            ])
            .map_err(std::io::Error::other)?;

        let preview = sequence.plan.preview_text();
        assert!(preview.contains("repository: github.com/octo/repo"));
        assert!(preview.contains("base: release"));
        assert!(!preview.contains("repository default"));
        assert_eq!(
            sequence.sequence.remaining_requests,
            vec![OperationRequest::OpenPullRequest { base: None }]
        );
        let paths = isolated_store_paths("prompt-sequence-configured-pr-base-audit")?;
        let store = LocalStore::open(paths.clone())?;
        std::fs::write(
            &store.paths().config_file,
            "schema-version = 1\n[pull-requests]\ndefault-base-branch = \"release\"\n",
        )?;
        let result = PromptSequenceExecutor {
            repo_root: repo,
            audit: AuditDestination::Paths(paths),
            github_executable: Some(fake_gh.clone()),
            policy,
            ssh_executable: Some(ssh),
        }
        .execute(sequence.sequence);

        assert!(
            result
                .message()
                .contains("Prompt sequence completed 2 step(s)."),
            "{}",
            result.message()
        );
        assert!(
            std::fs::read_to_string(fake_gh.with_file_name("invocations"))?
                .contains("--base release")
        );
        Ok(())
    }

    #[test]
    fn deferred_pull_request_stops_when_config_changes_effective_base() -> Result<(), Box<dyn Error>>
    {
        let repo = pushed_branch_repo("prompt-sequence-changed-pr-base")?;
        let fake_gh = fake_gh("prompt-sequence-changed-pr-base", false)?;
        let paths = isolated_store_paths("prompt-sequence-changed-pr-base-audit")?;
        let store = LocalStore::open(paths.clone())?;
        std::fs::write(
            &store.paths().config_file,
            "schema-version = 1\n[pull-requests]\ndefault-base-branch = \"release\"\n",
        )?;
        let policy = EffectivePolicy::new(&store.load_config().settings);
        let ssh_root = isolated_temp_root("prompt-sequence-changed-pr-base-ssh")?;
        std::fs::create_dir_all(&ssh_root)?;
        let ssh = ssh_root.join("ssh");
        std::fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nprintf '%s\\n' 'schema-version = 1' '[pull-requests]' 'default-base-branch = \"main\"' > '{}'\ntarget=$(git config --get-regexp '^remote\\..*\\.testbare$' | cut -d' ' -f2-)\nexec git-upload-pack \"$target\"\n",
                store.paths().config_file.display()
            ),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&ssh)?.permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&ssh, permissions)?;
        }
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: policy.clone(),
            ssh_executable: Some(ssh.clone()),
        };
        let sequence = planner
            .plan_prompt_sequence(vec![
                OperationRequest::Fetch,
                OperationRequest::OpenPullRequest { base: None },
            ])
            .map_err(std::io::Error::other)?;
        assert!(sequence.plan.preview_text().contains("base: release"));

        let result = PromptSequenceExecutor {
            repo_root: repo,
            audit: AuditDestination::Paths(paths),
            github_executable: Some(fake_gh.clone()),
            policy,
            ssh_executable: Some(ssh),
        }
        .execute(sequence.sequence);

        let message = result.message();
        assert!(
            message.contains("Prompt sequence stopped before step 2 of 2."),
            "{message}"
        );
        assert!(
            message.contains("pull request target changed since the sequence preview"),
            "{message}"
        );
        assert!(message.contains("new sequence preview and confirmation"));
        assert!(
            !std::fs::read_to_string(fake_gh.with_file_name("invocations"))?.contains("pr:create")
        );
        Ok(())
    }

    #[test]
    fn deferred_explicit_pull_request_base_keeps_enterprise_parity() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("prompt-sequence-enterprise-explicit-base")?;
        git_stdout(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                "https://git.example.com/octo/repo.git",
            ],
        )?;
        let fake_gh = fake_gh("prompt-sequence-enterprise-explicit-base", false)?;
        let mut config = AppConfig::default();
        config.pull_requests.default_base_branch = Some("release".to_owned());
        let sequence = OperationPlanner {
            repo_root: repo,
            github_executable: Some(fake_gh),
            policy: EffectivePolicy::new(&config),
            ssh_executable: Some(test_ssh_command()?),
        }
        .plan_prompt_sequence(vec![
            OperationRequest::Branches,
            OperationRequest::OpenPullRequest {
                base: Some("main".to_owned()),
            },
        ])
        .map_err(std::io::Error::other)?;

        let preview = sequence.plan.preview_text();
        assert!(preview.contains("repository: git.example.com/octo/repo"));
        assert!(preview.contains("base: main"));
        assert!(!preview.contains("base: release"));
        assert_eq!(
            sequence.sequence.remaining_requests,
            vec![OperationRequest::OpenPullRequest {
                base: Some("main".to_owned())
            }]
        );
        Ok(())
    }

    #[test]
    fn missing_gh_blocks_sequence_before_push_with_setup_guidance() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("prompt-sequence-missing-gh")?;
        let ssh = test_ssh_command()?;
        std::fs::write(repo.join("file.txt"), "push before setup check\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "ahead"])?;
        let missing_gh = repo.join("missing-gh");
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(missing_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(ssh.clone()),
        };
        let error = match planner.plan_prompt_sequence(vec![
            OperationRequest::Push,
            OperationRequest::OpenPullRequest { base: None },
        ]) {
            Ok(_) => return Err(std::io::Error::other("missing gh must block preview").into()),
            Err(error) => error,
        };

        assert!(error.contains(bitbygit_gh::INSTALL_GH_GUIDANCE));
        let branch = git_stdout(&repo, &["branch", "--show-current"])?;
        let remote = git_stdout_with_ssh(
            &repo,
            &[
                "ls-remote",
                "origin",
                &format!("refs/heads/{}", branch.trim()),
            ],
            &ssh,
        )?;
        assert!(!remote.starts_with(git_stdout(&repo, &["rev-parse", "HEAD"])?.trim()));
        Ok(())
    }

    #[test]
    fn gh_auth_failure_blocks_sequence_before_push_with_setup_guidance()
    -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("prompt-sequence-gh-auth")?;
        let fake_gh = fake_gh("prompt-sequence-gh-auth", false)?;
        std::fs::write(fake_gh.with_file_name("auth-fails"), "")?;
        let ssh = test_ssh_command()?;
        std::fs::write(repo.join("file.txt"), "push before auth check\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "ahead"])?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(ssh.clone()),
        };
        let error = match planner.plan_prompt_sequence(vec![
            OperationRequest::Push,
            OperationRequest::OpenPullRequest { base: None },
        ]) {
            Ok(_) => return Err(std::io::Error::other("gh auth must block preview").into()),
            Err(error) => error,
        };

        assert!(error.contains("GitHub CLI is not authenticated for github.com"));
        assert!(error.contains("gh auth login --hostname github.com"));
        let branch = git_stdout(&repo, &["branch", "--show-current"])?;
        let remote = git_stdout_with_ssh(
            &repo,
            &[
                "ls-remote",
                "origin",
                &format!("refs/heads/{}", branch.trim()),
            ],
            &ssh,
        )?;
        assert!(!remote.starts_with(git_stdout(&repo, &["rev-parse", "HEAD"])?.trim()));
        let invocations = std::fs::read_to_string(fake_gh.with_file_name("invocations"))?;
        assert!(invocations.contains("auth:status"));
        assert!(!invocations.contains("pr:create"));
        Ok(())
    }

    #[test]
    fn parsed_low_risk_prompts_match_manual_planner_output() -> Result<(), Box<dyn Error>> {
        let planner = OperationPlanner::current(EffectivePolicy::default());
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
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
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
        let execution =
            PlanExecutor::with_audit_paths_and_ssh(&repo, paths.clone(), test_ssh_command()?)
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
    fn pull_request_base_precedence_is_explicit_then_configured_then_provider()
    -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-base-precedence")?;
        let fake_gh = fake_gh("open-pr-base-precedence", false)?;
        let mut config = AppConfig::default();
        config.pull_requests.default_base_branch = Some("release".to_owned());
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::new(&config),
            ssh_executable: Some(test_ssh_command()?),
        };

        let configured = planner
            .plan_request(OperationRequest::OpenPullRequest { base: None })
            .map_err(std::io::Error::other)?;
        assert_eq!(
            configured.plan.request,
            OperationRequest::OpenPullRequest {
                base: Some("release".to_owned())
            }
        );
        assert!(configured.plan.preview_text().contains("base: release"));
        let execution = PlanExecutor::with_audit_paths_and_ssh(
            &repo,
            isolated_store_paths("open-pr-base-precedence-audit")?,
            test_ssh_command()?,
        )
        .execute(&configured.plan, configured.context);
        assert!(execution.succeeded(), "{}", execution.message());
        assert!(
            std::fs::read_to_string(fake_gh.with_file_name("invocations"))?
                .contains("--base release")
        );

        let explicit = planner
            .plan_request(OperationRequest::OpenPullRequest {
                base: Some("main".to_owned()),
            })
            .map_err(std::io::Error::other)?;
        assert_eq!(
            explicit.plan.request,
            OperationRequest::OpenPullRequest {
                base: Some("main".to_owned())
            }
        );
        assert!(explicit.plan.preview_text().contains("base: main"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn private_https_pull_request_uses_authenticated_api_not_repository_credential_helpers()
    -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-private-https")?;
        git_stdout(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/octo/repo.git",
            ],
        )?;
        let marker = repo.join("credential-helper-ran");
        git_stdout(
            &repo,
            &[
                "config",
                "credential.helper",
                &format!("!touch '{}'", marker.display()),
            ],
        )?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh("open-pr-private-https", false)?),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };

        let operation = planner
            .plan_request(OperationRequest::OpenPullRequest { base: None })
            .map_err(std::io::Error::other)?;

        assert!(operation.plan.preview_text().contains("base: main"));
        assert!(!marker.exists());
        Ok(())
    }

    #[test]
    fn enterprise_https_pull_request_scopes_all_gh_commands_to_remote_host()
    -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-enterprise-https")?;
        git_stdout(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                "https://git.example.com/octo/repo.git",
            ],
        )?;
        let fake_gh = fake_gh("open-pr-enterprise-https", false)?;
        let mut config = AppConfig::default();
        config.pull_requests.default_base_branch = Some("release".to_owned());
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::new(&config),
            ssh_executable: Some(test_ssh_command()?),
        };

        let operation = planner
            .plan_request(OperationRequest::OpenPullRequest { base: None })
            .map_err(std::io::Error::other)?;

        assert!(
            operation
                .plan
                .preview_text()
                .contains("https://git.example.com/octo/repo/compare/release...feature/open-pr")
        );
        let execution = PlanExecutor::with_audit_paths_and_ssh(
            &repo,
            isolated_store_paths("open-pr-enterprise-https-audit")?,
            test_ssh_command()?,
        )
        .execute(&operation.plan, operation.context);
        assert!(execution.succeeded(), "{}", execution.message());
        assert_eq!(
            execution.message(),
            "Pull request created: https://git.example.com/octo/repo/pull/43"
        );
        let invocations = std::fs::read_to_string(fake_gh.with_file_name("invocations"))?;
        assert!(invocations.contains("auth status --active --hostname git.example.com"));
        assert!(invocations.contains("repo view git.example.com/octo/repo"));
        assert!(invocations.contains("--hostname git.example.com"));
        assert!(invocations.contains("pr create"));
        assert!(invocations.contains("--repo git.example.com/octo/repo"));
        assert!(invocations.contains("--base release"));
        Ok(())
    }

    #[test]
    fn enterprise_ssh_remote_surfaces_existing_pull_request() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-enterprise-ssh-existing")?;
        git_stdout(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                "git@git.example.com:octo/repo.git",
            ],
        )?;
        let fake_gh = fake_gh_with_pull_requests(
            "open-pr-enterprise-ssh-existing",
            "[{\"number\":42,\"html_url\":\"https://git.example.com/octo/repo/pull/42\",\"title\":\"Existing PR\",\"base\":{\"ref\":\"main\"},\"head\":{\"ref\":\"feature/open-pr\",\"repo\":{\"full_name\":\"octo/repo\"}}}]",
            true,
        )?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };

        let operation = planner
            .plan_request(OperationRequest::OpenPullRequest { base: None })
            .map_err(std::io::Error::other)?;

        assert!(
            operation
                .plan
                .preview_text()
                .contains("https://git.example.com/octo/repo/pull/42")
        );
        let execution = PlanExecutor::with_audit_paths_and_ssh(
            &repo,
            isolated_store_paths("open-pr-enterprise-ssh-existing-audit")?,
            test_ssh_command()?,
        )
        .execute(&operation.plan, operation.context);
        assert!(execution.succeeded(), "{}", execution.message());
        assert!(execution.message().contains("Pull request already open"));
        let invocations = std::fs::read_to_string(fake_gh.with_file_name("invocations"))?;
        assert!(invocations.contains("head=octo:feature/open-pr"));
        assert!(invocations.contains("--hostname git.example.com"));
        assert!(!invocations.contains("pr:create"));
        Ok(())
    }

    #[test]
    fn pull_request_uses_differently_named_tracked_remote_branch() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-different-upstream-branch")?;
        git_stdout_with_ssh(
            &repo,
            &["push", "origin", "HEAD:review-feature"],
            &test_ssh_command()?,
        )?;
        git_stdout_with_ssh(
            &repo,
            &["push", "origin", "--delete", "feature/open-pr"],
            &test_ssh_command()?,
        )?;
        git_stdout(
            &repo,
            &[
                "branch",
                "--set-upstream-to=origin/review-feature",
                "feature/open-pr",
            ],
        )?;
        let fake_gh = fake_gh_with_pull_requests(
            "open-pr-different-upstream-branch",
            "[{\"number\":42,\"html_url\":\"https://github.com/octo/repo/pull/42\",\"title\":\"Existing PR\",\"base\":{\"ref\":\"main\"},\"head\":{\"ref\":\"review-feature\",\"repo\":{\"full_name\":\"octo/repo\"}}}]",
            true,
        )?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };

        let operation = planner
            .plan_request(OperationRequest::OpenPullRequest { base: None })
            .map_err(std::io::Error::other)?;

        let preview = operation.plan.preview_text();
        assert!(preview.contains("head: review-feature"));
        assert!(preview.contains("title: feature/open-pr"));
        assert!(preview.contains("target: https://github.com/octo/repo/pull/42"));
        let execution = PlanExecutor::with_audit_paths_and_ssh(
            &repo,
            isolated_store_paths("open-pr-different-upstream-branch-audit")?,
            test_ssh_command()?,
        )
        .execute(&operation.plan, operation.context);

        assert!(execution.succeeded(), "{}", execution.message());
        assert!(execution.message().contains("Pull request already open"));
        let invocations = std::fs::read_to_string(fake_gh.with_file_name("invocations"))?;
        assert!(invocations.contains("head=octo:review-feature"));
        assert!(!invocations.contains("head=octo:feature/open-pr"));
        assert!(!invocations.contains("pr:create"));
        Ok(())
    }

    #[test]
    fn pull_request_head_revalidation_failure_does_not_persist_remote_credentials()
    -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-credential-audit")?;
        git_stdout(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                "ssh://abc123@github.com/octo/repo.git",
            ],
        )?;
        let fake_gh = fake_gh("open-pr-credential-audit", false)?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };
        let operation = planner
            .plan_request(OperationRequest::OpenPullRequest { base: None })
            .map_err(std::io::Error::other)?;
        std::fs::write(fake_gh.with_file_name("head-missing"), "")?;
        let paths = isolated_store_paths("open-pr-credential-audit")?;

        let execution =
            PlanExecutor::with_audit_paths_and_ssh(&repo, paths.clone(), test_ssh_command()?)
                .execute(&operation.plan, operation.context);

        assert!(!execution.succeeded());
        assert!(execution.message().contains("no longer pushed"));
        let entries = LocalStore::open(paths)?.list_audit_entries()?;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].result, "error");
        assert!(entries[1].message.contains("no longer pushed"));
        assert!(!entries[1].message.contains("abc123"));
        assert!(!entries[1].message.contains("github.com/octo/repo"));
        Ok(())
    }

    #[test]
    fn existing_pull_request_is_surfaced_without_creation() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-existing")?;
        let fake_gh = fake_gh("open-pr-existing", true)?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
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
        let execution = PlanExecutor::with_audit_paths_and_ssh(
            &repo,
            isolated_store_paths("open-pr-existing-audit")?,
            test_ssh_command()?,
        )
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
    fn existing_pull_request_repository_match_is_case_insensitive() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-existing-mixed-case")?;
        git_stdout(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                "ssh://git@github.com/OCTO/REPO.git",
            ],
        )?;
        let fake_gh = fake_gh("open-pr-existing-mixed-case", true)?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };

        let operation = planner
            .plan_request(OperationRequest::OpenPullRequest { base: None })
            .map_err(std::io::Error::other)?;

        assert!(operation.plan.preview_text().contains("pull/42"));
        let execution = PlanExecutor::with_audit_paths_and_ssh(
            &repo,
            isolated_store_paths("open-pr-existing-mixed-case-audit")?,
            test_ssh_command()?,
        )
        .execute(&operation.plan, operation.context);
        assert!(execution.succeeded(), "{}", execution.message());
        assert!(execution.message().contains("already open"));
        assert!(
            !std::fs::read_to_string(fake_gh.with_file_name("invocations"))?.contains("pr:create")
        );
        Ok(())
    }

    #[test]
    fn renamed_head_repository_surfaces_existing_pull_request() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-renamed-head")?;
        git_stdout(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                "ssh://git@github.com/octo/old-repo.git",
            ],
        )?;
        let fake_gh = fake_gh("open-pr-renamed-head", true)?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };

        let operation = planner
            .plan_request(OperationRequest::OpenPullRequest { base: None })
            .map_err(std::io::Error::other)?;

        assert!(operation.plan.preview_text().contains("pull/42"));
        let execution = PlanExecutor::with_audit_paths_and_ssh(
            &repo,
            isolated_store_paths("open-pr-renamed-head-audit")?,
            test_ssh_command()?,
        )
        .execute(&operation.plan, operation.context);
        assert!(execution.succeeded(), "{}", execution.message());
        assert!(execution.message().contains("already open"));
        assert!(
            !std::fs::read_to_string(fake_gh.with_file_name("invocations"))?.contains("pr:create")
        );
        Ok(())
    }

    #[test]
    fn open_pull_request_plan_rejects_missing_base_branch() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-missing-base")?;
        let fake_gh = fake_gh("open-pr-missing-base", false)?;
        let planner = OperationPlanner {
            repo_root: repo,
            github_executable: Some(fake_gh),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };

        let error = match planner.plan_request(OperationRequest::OpenPullRequest {
            base: Some("missing".to_owned()),
        }) {
            Ok(_) => return Err(std::io::Error::other("missing base must fail closed").into()),
            Err(error) => error,
        };

        assert!(error.contains("base branch missing was not found"));
        Ok(())
    }

    #[test]
    fn missing_configured_pull_request_base_fails_closed_with_guidance()
    -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-missing-configured-base")?;
        let fake_gh = fake_gh("open-pr-missing-configured-base", false)?;
        let mut config = AppConfig::default();
        config.pull_requests.default_base_branch = Some("missing".to_owned());
        let planner = OperationPlanner {
            repo_root: repo,
            github_executable: Some(fake_gh),
            policy: EffectivePolicy::new(&config),
            ssh_executable: Some(test_ssh_command()?),
        };

        let error = match planner.plan_request(OperationRequest::OpenPullRequest { base: None }) {
            Ok(_) => {
                return Err(
                    std::io::Error::other("missing configured base must fail closed").into(),
                );
            }
            Err(error) => error,
        };

        assert!(error.contains("base branch missing was not found"));
        assert!(error.contains("pull-requests.default-base-branch"));
        assert!(error.contains("open pr to <branch>"));
        Ok(())
    }

    #[test]
    fn open_pull_request_execution_rejects_deleted_base_branch() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-deleted-base")?;
        let fake_gh = fake_gh("open-pr-deleted-base", false)?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };
        let operation = planner
            .plan_request(OperationRequest::OpenPullRequest { base: None })
            .map_err(std::io::Error::other)?;
        std::fs::write(fake_gh.with_file_name("base-missing"), "")?;

        let execution = PlanExecutor::with_audit_paths_and_ssh(
            &repo,
            isolated_store_paths("open-pr-deleted-base-audit")?,
            test_ssh_command()?,
        )
        .execute(&operation.plan, operation.context);

        assert!(!execution.succeeded());
        assert!(
            execution
                .message()
                .contains("base branch main no longer exists")
        );
        assert!(
            !std::fs::read_to_string(fake_gh.with_file_name("invocations"))?.contains("pr:create")
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn open_pull_request_plan_never_launches_ext_transport() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-ext-transport")?;
        let marker = repo.join("ext-ran");
        let push_url = format!("ext::touch {}", marker.display());
        git_stdout(&repo, &["remote", "set-url", "--push", "origin", &push_url])?;
        git_stdout(&repo, &["config", "protocol.ext.allow", "always"])?;

        let error = match OperationPlanner::new(&repo)
            .plan_request(OperationRequest::OpenPullRequest { base: None })
        {
            Ok(_) => return Err(std::io::Error::other("ext transport must fail closed").into()),
            Err(error) => error,
        };

        assert!(error.contains("unsupported URL"));
        assert!(!marker.exists());
        Ok(())
    }

    #[test]
    fn pull_request_for_other_base_is_not_surfaced_after_create_failure()
    -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-other-base")?;
        let fake_gh = fake_gh_with_pull_requests(
            "open-pr-other-base",
            "[{\"number\":42,\"html_url\":\"https://github.com/octo/repo/pull/42\",\"title\":\"Existing PR\",\"base\":{\"ref\":\"main\"},\"head\":{\"ref\":\"feature/open-pr\",\"repo\":{\"full_name\":\"octo/repo\"}}}]",
            false,
        )?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };
        let operation = planner
            .plan_request(OperationRequest::OpenPullRequest {
                base: Some("release".to_owned()),
            })
            .map_err(std::io::Error::other)?;

        assert!(
            operation
                .plan
                .preview_text()
                .contains("https://github.com/octo/repo/compare/release...feature/open-pr")
        );
        let execution = PlanExecutor::with_audit_paths_and_ssh(
            &repo,
            isolated_store_paths("open-pr-other-base-audit")?,
            test_ssh_command()?,
        )
        .execute(&operation.plan, operation.context);

        assert!(!execution.succeeded());
        assert!(!execution.message().contains("pull/42"));
        assert!(
            std::fs::read_to_string(fake_gh.with_file_name("invocations"))?
                .contains("--base release")
        );
        Ok(())
    }

    #[test]
    fn pull_request_from_another_fork_is_not_surfaced() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-fork-collision")?;
        let fake_gh = fake_gh_with_pull_requests(
            "open-pr-fork-collision",
            "[{\"number\":42,\"html_url\":\"https://github.com/bob/repo/pull/42\",\"title\":\"Other fork PR\",\"base\":{\"ref\":\"main\"},\"head\":{\"ref\":\"feature/open-pr\",\"repo\":{\"full_name\":\"bob/repo\"}}}]",
            true,
        )?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };
        let operation = planner
            .plan_request(OperationRequest::OpenPullRequest { base: None })
            .map_err(std::io::Error::other)?;

        assert!(!operation.plan.preview_text().contains("pull/42"));
        let execution = PlanExecutor::with_audit_paths_and_ssh(
            &repo,
            isolated_store_paths("open-pr-fork-collision-audit")?,
            test_ssh_command()?,
        )
        .execute(&operation.plan, operation.context);

        assert!(execution.succeeded(), "{}", execution.message());
        assert_eq!(
            execution.message(),
            "Pull request created: https://github.com/octo/repo/pull/43"
        );
        assert!(
            std::fs::read_to_string(fake_gh.with_file_name("invocations"))?.contains("pr:create")
        );
        Ok(())
    }

    #[test]
    fn pull_request_gh_commands_target_upstream_for_a_tracked_fork() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-tracked-fork")?;
        git_stdout(&repo, &["remote", "rename", "origin", "fork"])?;
        git_stdout(
            &repo,
            &[
                "remote",
                "add",
                "upstream",
                "https://github.com/upstream/repo.git",
            ],
        )?;
        let fake_gh = fake_gh("open-pr-tracked-fork", false)?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };

        let operation = planner
            .plan_request(OperationRequest::OpenPullRequest { base: None })
            .map_err(std::io::Error::other)?;
        assert!(operation.plan.preview_text().contains("remote: fork"));
        assert!(
            operation
                .plan
                .preview_text()
                .contains("https://github.com/upstream/repo/compare/main...octo%3Afeature/open-pr"),
            "{}",
            operation.plan.preview_text()
        );
        let execution = PlanExecutor::with_audit_paths_and_ssh(
            &repo,
            isolated_store_paths("open-pr-tracked-fork-audit")?,
            test_ssh_command()?,
        )
        .execute(&operation.plan, operation.context);

        assert!(execution.succeeded(), "{}", execution.message());
        let invocations = std::fs::read_to_string(fake_gh.with_file_name("invocations"))?;
        let repo_views = invocations
            .lines()
            .filter(|line| line.starts_with("repo:view"))
            .collect::<Vec<_>>();
        assert!(
            repo_views
                .iter()
                .any(|invocation| invocation.contains("view github.com/upstream/repo"))
        );
        assert!(
            repo_views
                .iter()
                .any(|invocation| invocation.contains("view github.com/octo/repo"))
        );
        assert!(
            repo_views
                .iter()
                .all(|invocation| !invocation.contains("--repo"))
        );
        for invocation in invocations
            .lines()
            .filter(|line| line.starts_with("api:--method") && line.contains("/pulls"))
        {
            assert!(invocation.contains("repos/upstream/repo/pulls"));
            assert!(invocation.contains("head=octo:feature/open-pr"));
        }
        for invocation in invocations
            .lines()
            .filter(|line| line.starts_with("pr:create"))
        {
            assert!(invocation.contains("--repo github.com/upstream/repo"));
            assert!(invocation.contains("octo:feature/open-pr"));
        }
        Ok(())
    }

    #[test]
    fn pull_request_targets_fetch_repository_for_a_split_remote() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-split-remote")?;
        git_stdout(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/upstream/repo.git",
            ],
        )?;
        git_stdout(
            &repo,
            &[
                "config",
                "--add",
                "url.https://github.com/octo/repo.git.pushInsteadOf",
                "https://github.com/upstream/repo.git",
            ],
        )?;
        let fake_gh = fake_gh("open-pr-split-remote", false)?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };

        let operation = planner
            .plan_request(OperationRequest::OpenPullRequest { base: None })
            .map_err(std::io::Error::other)?;

        let preview = operation.plan.preview_text();
        assert!(preview.contains("remote: origin"));
        assert!(preview.contains("head: octo:feature/open-pr"));
        assert!(preview.contains("https://github.com/upstream/repo/compare/main"));
        let execution = PlanExecutor::with_audit_paths_and_ssh(
            &repo,
            isolated_store_paths("open-pr-split-remote-audit")?,
            test_ssh_command()?,
        )
        .execute(&operation.plan, operation.context);

        assert!(execution.succeeded(), "{}", execution.message());
        let invocations = std::fs::read_to_string(fake_gh.with_file_name("invocations"))?;
        assert!(invocations.contains("repos/upstream/repo/pulls"));
        assert!(invocations.contains("head=octo:feature/open-pr"));
        assert!(invocations.contains("--repo github.com/upstream/repo"));
        Ok(())
    }

    #[test]
    fn pull_request_uses_default_simple_in_triangular_workflow() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-triangular-fork")?;
        git_stdout(&repo, &["remote", "rename", "origin", "fork"])?;
        git_stdout(
            &repo,
            &[
                "remote",
                "add",
                "upstream",
                "https://github.com/upstream/repo.git",
            ],
        )?;
        git_stdout(
            &repo,
            &["update-ref", "refs/remotes/upstream/main", "HEAD^"],
        )?;
        git_stdout(
            &repo,
            &["config", "branch.feature/open-pr.remote", "upstream"],
        )?;
        git_stdout(
            &repo,
            &["config", "branch.feature/open-pr.merge", "refs/heads/main"],
        )?;
        git_stdout(
            &repo,
            &["config", "branch.feature/open-pr.pushRemote", "fork"],
        )?;
        let fake_gh = fake_gh("open-pr-triangular-fork", false)?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh.clone()),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };

        let operation = planner
            .plan_request(OperationRequest::OpenPullRequest { base: None })
            .map_err(std::io::Error::other)?;

        let preview = operation.plan.preview_text();
        assert!(preview.contains("remote: fork"));
        assert!(preview.contains("head: octo:feature/open-pr"));
        assert!(preview.contains("https://github.com/upstream/repo/compare/main"));
        let execution = PlanExecutor::with_audit_paths_and_ssh(
            &repo,
            isolated_store_paths("open-pr-triangular-fork-audit")?,
            test_ssh_command()?,
        )
        .execute(&operation.plan, operation.context);
        assert!(execution.succeeded(), "{}", execution.message());
        let invocations = std::fs::read_to_string(fake_gh.with_file_name("invocations"))?;
        assert!(invocations.contains("head=octo:feature/open-pr"));
        assert!(invocations.contains("pr:create"));
        Ok(())
    }

    #[test]
    fn pull_request_accepts_suffixless_head_and_upstream_remotes() -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-suffixless-remotes")?;
        git_stdout(&repo, &["remote", "rename", "origin", "fork"])?;
        git_stdout(
            &repo,
            &[
                "remote",
                "set-url",
                "fork",
                "ssh://git@github.com/octo/repo",
            ],
        )?;
        git_stdout(
            &repo,
            &["remote", "add", "upstream", "git@github.com:upstream/repo"],
        )?;
        let fake_gh = fake_gh("open-pr-suffixless-remotes", false)?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };

        let operation = planner
            .plan_request(OperationRequest::OpenPullRequest { base: None })
            .map_err(std::io::Error::other)?;

        assert!(
            operation
                .plan
                .preview_text()
                .contains("https://github.com/upstream/repo/compare/main...octo%3Afeature/open-pr")
        );
        Ok(())
    }

    #[test]
    fn pull_request_allows_matching_branch_names_across_fork_and_upstream()
    -> Result<(), Box<dyn Error>> {
        let repo = pushed_branch_repo("open-pr-matching-fork-branch")?;
        git_stdout(&repo, &["branch", "-m", "main"])?;
        git_stdout_with_ssh(
            &repo,
            &["push", "-u", "origin", "main"],
            &test_ssh_command()?,
        )?;
        git_stdout(
            &repo,
            &[
                "remote",
                "add",
                "upstream",
                "https://github.com/upstream/repo.git",
            ],
        )?;
        let fake_gh = fake_gh("open-pr-matching-fork-branch", false)?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: Some(fake_gh),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };

        let operation = planner
            .plan_request(OperationRequest::OpenPullRequest { base: None })
            .map_err(std::io::Error::other)?;

        assert!(
            operation
                .plan
                .preview_text()
                .contains("https://github.com/upstream/repo/compare/main...octo%3Amain")
        );
        Ok(())
    }

    #[test]
    fn open_pull_request_plan_encodes_compare_ref_names() {
        let plan = open_pull_request_plan(
            OperationRequest::OpenPullRequest {
                base: Some("release#candidate".to_owned()),
            },
            "origin",
            "feature%ready",
            "release#candidate",
            "feature%ready",
            &GitHubRepository::new("github.com", "octo/repo"),
            None,
        );

        assert!(plan.preview_text().contains(
            "https://github.com/octo/repo/compare/release%23candidate...feature%25ready?expand=1"
        ));
    }

    #[test]
    fn github_remote_parser_supports_github_and_enterprise_ssh_forms() {
        for (url, hostname) in [
            ("https://github.com/octo/repo.git", "github.com"),
            ("ssh://git@github.com/octo/repo.git", "github.com"),
            ("https://git.example.com/octo/repo.git", "git.example.com"),
            ("ssh://git@git.example.com/octo/repo.git", "git.example.com"),
            ("git@git.example.com:octo/repo.git", "git.example.com"),
        ] {
            assert_eq!(
                github_repository_from_push_url(url),
                Ok(GitHubRepository::new(hostname, "octo/repo"))
            );
        }
    }

    #[test]
    fn github_remote_parser_rejects_unsupported_or_invalid_targets() {
        for url in [
            "http://git.example.com/octo/repo.git",
            "https://git.example.com:8443/octo/repo.git",
            "ssh://git@-git.example.com/octo/repo.git",
            "https://git.example.com/octo",
            "https://git.example.com/octo/repo/extra.git",
            "file:///octo/repo.git",
        ] {
            assert!(
                github_repository_from_push_url(url).is_err(),
                "{url} must fail closed"
            );
        }
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
        add_github_remote(&repo, "origin", &remote)?;
        git_stdout_with_ssh(
            &repo,
            &["push", "-u", "origin", branch.as_str()],
            &test_ssh_command()?,
        )?;
        let fake_gh = fake_gh("open-pr-invalid-state", false)?;
        let planner = OperationPlanner {
            repo_root: repo,
            github_executable: Some(fake_gh),
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
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
    fn operation_labels_are_explicit() {
        assert_eq!(operation_label(RepositoryOperation::Merge), "MERGE");
        assert_eq!(operation_label(RepositoryOperation::Rebase), "REBASE");
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
        let visible_len = status_file_visible_len(app.last_viewport.status, false);
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
        assert!(queue_text[1].contains("y confirm; n/Esc cancel"));
        assert!(queue_text[2].contains("medium risk policy requires visible-plan"));
        assert!(queue_text[3].contains("stage all changes [y confirm; n/Esc cancel]"));
        assert!(queue_text[4].contains("stage all working tree changes"));

        app.handle_key(key(KeyCode::Char('n')));

        assert_eq!(app.operation_queue.pending(), None);
        assert_eq!(app.details, "Operation cancelled.");
    }

    #[test]
    fn blocked_pull_planning_does_not_fetch_remote_tracking_refs() -> Result<(), Box<dyn Error>> {
        for (name, rebase) in [("fast-forward", false), ("rebase", true)] {
            let (repo, branch, original_tracking_oid) =
                stale_pull_tracking_repo(&format!("blocked-pull-{name}"))?;
            let mut config = AppConfig::default();
            if rebase {
                config.policy.confirmation.high = ConfirmationSetting::Blocked;
            } else {
                config.policy.confirmation.medium = ConfirmationSetting::Blocked;
            }
            let planner = OperationPlanner {
                repo_root: repo.clone(),
                github_executable: None,
                policy: EffectivePolicy::new(&config),
                ssh_executable: Some(test_ssh_command()?),
            };

            let operation = planner
                .plan_request(OperationRequest::Pull { rebase })
                .map_err(std::io::Error::other)?;

            assert_eq!(
                operation.plan.confirmation.requirement,
                ConfirmationRequirement::Blocked
            );
            assert!(operation.plan.confirmation.prompt.contains(if rebase {
                "rebase"
            } else {
                "pull"
            }));
            assert_eq!(
                git_stdout(
                    &repo,
                    &["rev-parse", &format!("refs/remotes/origin/{branch}")]
                )?
                .trim(),
                original_tracking_oid
            );
        }
        Ok(())
    }

    #[test]
    fn disabled_pull_family_rejects_both_variants_before_fetching() -> Result<(), Box<dyn Error>> {
        for (name, rebase) in [("fast-forward", false), ("rebase", true)] {
            let (repo, branch, original_tracking_oid) =
                stale_pull_tracking_repo(&format!("disabled-pull-{name}"))?;
            let mut config = AppConfig::default();
            config.policy.disabled_operations = vec![OperationFamily::Pull];
            let planner = OperationPlanner {
                repo_root: repo.clone(),
                github_executable: None,
                policy: EffectivePolicy::new(&config),
                ssh_executable: Some(test_ssh_command()?),
            };

            let error = match planner.plan_request(OperationRequest::Pull { rebase }) {
                Ok(_operation) => {
                    return Err(std::io::Error::other("disabled pull must be rejected").into());
                }
                Err(error) => error,
            };

            assert!(error.contains("pull"), "{error}");
            assert!(error.contains("disabled by policy"), "{error}");
            assert_eq!(
                git_stdout(
                    &repo,
                    &["rev-parse", &format!("refs/remotes/origin/{branch}")]
                )?
                .trim(),
                original_tracking_oid
            );
        }
        Ok(())
    }

    #[test]
    fn disabled_later_sequence_step_preflights_before_first_step_planning()
    -> Result<(), Box<dyn Error>> {
        let (repo, branch, original_tracking_oid) =
            stale_pull_tracking_repo("disabled-later-sequence-step")?;
        let mut config = AppConfig::default();
        config.policy.disabled_operations = vec![OperationFamily::Branches];
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: None,
            policy: EffectivePolicy::new(&config),
            ssh_executable: Some(test_ssh_command()?),
        };

        let error = match planner.plan_prompt_sequence(vec![
            OperationRequest::Pull { rebase: false },
            OperationRequest::Branches,
        ]) {
            Ok(_sequence) => {
                return Err(std::io::Error::other(
                    "disabled later step must reject the whole sequence",
                )
                .into());
            }
            Err(error) => error,
        };

        assert!(error.contains("step 2"), "{error}");
        assert!(error.contains("disabled by policy"), "{error}");
        assert_eq!(
            git_stdout(
                &repo,
                &["rev-parse", &format!("refs/remotes/origin/{branch}")]
            )?
            .trim(),
            original_tracking_oid
        );
        Ok(())
    }

    #[test]
    fn confirmation_blocked_later_step_does_not_fetch_temporary_remote_object()
    -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("blocked-later-step-remote-object")?;
        let remote = isolated_bare_git_repo("blocked-later-step-remote-object-remote")?;
        let updater = isolated_git_repo("blocked-later-step-remote-object-updater")?;
        let ssh = test_ssh_command()?;
        configure_git_identity(&repo)?;
        std::fs::write(repo.join("file.txt"), "base\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "initial"])?;
        let branch = git_stdout(&repo, &["branch", "--show-current"])?
            .trim()
            .to_owned();
        add_github_remote(&repo, "origin", &remote)?;
        git_stdout_with_ssh(&repo, &["push", "-u", "origin", &branch], &ssh)?;

        configure_git_identity(&updater)?;
        add_github_remote(&updater, "origin", &remote)?;
        git_stdout_with_ssh(&updater, &["fetch", "origin", &branch], &ssh)?;
        git_stdout(&updater, &["checkout", "-b", &branch, "FETCH_HEAD"])?;
        std::fs::write(updater.join("file.txt"), "remote update\n")?;
        git_stdout(&updater, &["add", "file.txt"])?;
        git_stdout(&updater, &["commit", "-m", "remote update"])?;
        let remote_oid = git_stdout(&updater, &["rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        git_stdout_with_ssh(&updater, &["push", "origin", &branch], &ssh)?;
        assert!(!git_object_exists(&repo, &remote_oid)?);

        let mut config = AppConfig::default();
        config.policy.confirmation.high = ConfirmationSetting::Blocked;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: None,
            policy: EffectivePolicy::new(&config),
            ssh_executable: Some(ssh),
        };

        let sequence = planner
            .plan_prompt_sequence(vec![
                OperationRequest::Pull { rebase: false },
                OperationRequest::Rebase {
                    base: branch.clone(),
                },
            ])
            .map_err(std::io::Error::other)?;

        assert_eq!(
            sequence.plan.confirmation.requirement,
            ConfirmationRequirement::Blocked
        );
        assert!(!git_object_exists(&repo, &remote_oid)?);
        assert!(
            git_stdout(
                &repo,
                &["for-each-ref", "--format=%(refname)", "refs/bitbygit/fetch"]
            )?
            .trim()
            .is_empty()
        );
        Ok(())
    }

    #[test]
    fn allowed_pull_publishes_staged_tracking_ref_only_during_execution()
    -> Result<(), Box<dyn Error>> {
        let (repo, branch, original_tracking_oid) = stale_pull_tracking_repo("allowed-pull")?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: None,
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };

        let operation = planner
            .plan_request(OperationRequest::Pull { rebase: false })
            .map_err(std::io::Error::other)?;
        let fetched_oid = match operation.context.payload.as_ref() {
            Some(PendingPayload::Pull { upstream_oid, .. }) => upstream_oid
                .clone()
                .ok_or_else(|| std::io::Error::other("expected fetched upstream"))?,
            _ => return Err(std::io::Error::other("expected pull payload").into()),
        };
        assert_eq!(
            git_stdout(
                &repo,
                &["rev-parse", &format!("refs/remotes/origin/{branch}")]
            )?
            .trim(),
            original_tracking_oid
        );

        let result =
            PlanExecutor::with_audit_paths(&repo, isolated_store_paths("allowed-pull-audit")?)
                .execute(&operation.plan, operation.context);

        assert!(result.succeeded(), "{}", result.message());
        assert_eq!(
            git_stdout(
                &repo,
                &["rev-parse", &format!("refs/remotes/origin/{branch}")]
            )?
            .trim(),
            fetched_oid
        );
        assert_eq!(
            git_stdout(&repo, &["rev-parse", "HEAD"])?.trim(),
            fetched_oid
        );
        Ok(())
    }

    #[test]
    fn pull_fetch_stages_tracking_update_until_changed_policy_is_revalidated()
    -> Result<(), Box<dyn Error>> {
        let (repo, branch, original_tracking_oid) =
            stale_pull_tracking_repo("in-flight-pull-policy")?;
        let paths = isolated_store_paths("in-flight-pull-policy-audit")?;
        let store = LocalStore::open(paths.clone())?;
        let ssh = policy_changing_ssh(
            "in-flight-pull-policy",
            &store.paths().config_file,
            "medium",
            "blocked",
        )?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: None,
            policy: EffectivePolicy::default(),
            ssh_executable: Some(ssh),
        };

        let operation = planner
            .plan_request(OperationRequest::Pull { rebase: false })
            .map_err(std::io::Error::other)?;

        assert_eq!(
            git_stdout(
                &repo,
                &["rev-parse", &format!("refs/remotes/origin/{branch}")]
            )?
            .trim(),
            original_tracking_oid
        );
        let executor = PlanExecutor {
            repo_root: repo.clone(),
            audit: AuditDestination::Paths(paths.clone()),
            policy: AuditDestination::Paths(paths.clone()).load_policy(&EffectivePolicy::default()),
            ssh_executable: None,
        };
        let result = executor.execute(&operation.plan, operation.context);

        assert!(!result.succeeded());
        let message = result.message();
        assert!(
            message.contains("policy now requires the operation to be blocked"),
            "{message}"
        );
        assert_eq!(
            git_stdout(
                &repo,
                &["rev-parse", &format!("refs/remotes/origin/{branch}")]
            )?
            .trim(),
            original_tracking_oid
        );
        Ok(())
    }

    #[test]
    fn deferred_pull_fetch_revalidates_changed_policy_before_publishing_tracking_ref()
    -> Result<(), Box<dyn Error>> {
        let (repo, branch, original_tracking_oid) =
            stale_pull_tracking_repo("deferred-in-flight-pull-policy")?;
        let planner = OperationPlanner::new(&repo);
        let sequence = planner
            .plan_prompt_sequence(vec![
                OperationRequest::Branches,
                OperationRequest::Pull { rebase: false },
            ])
            .map_err(std::io::Error::other)?;
        let paths = isolated_store_paths("deferred-in-flight-pull-policy-audit")?;
        let store = LocalStore::open(paths.clone())?;
        let ssh = policy_changing_ssh(
            "deferred-in-flight-pull-policy",
            &store.paths().config_file,
            "medium",
            "blocked",
        )?;
        let executor = PromptSequenceExecutor {
            repo_root: repo.clone(),
            audit: AuditDestination::Paths(paths),
            github_executable: None,
            policy: EffectivePolicy::default(),
            ssh_executable: Some(ssh),
        };

        let result =
            executor.execute_confirmed(sequence.sequence, ConfirmationRequirement::VisiblePlan);

        let message = result.message();
        assert!(
            message.contains("Prompt sequence stopped before step 2 of 2."),
            "{message}"
        );
        assert_eq!(
            git_stdout(
                &repo,
                &["rev-parse", &format!("refs/remotes/origin/{branch}")]
            )?
            .trim(),
            original_tracking_oid
        );
        Ok(())
    }

    #[test]
    fn deferred_blocked_pull_does_not_fetch_remote_tracking_ref() -> Result<(), Box<dyn Error>> {
        let (repo, branch, original_tracking_oid) =
            stale_pull_tracking_repo("deferred-blocked-pull")?;
        git_stdout(
            &repo,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/trigger:refs/remotes/origin/trigger",
            ],
        )?;
        let planner = OperationPlanner {
            repo_root: repo.clone(),
            github_executable: None,
            policy: EffectivePolicy::default(),
            ssh_executable: Some(test_ssh_command()?),
        };
        let sequence = planner
            .plan_prompt_sequence(vec![
                OperationRequest::Fetch,
                OperationRequest::Pull { rebase: false },
            ])
            .map_err(std::io::Error::other)?;
        let paths = isolated_store_paths("deferred-blocked-pull-audit")?;
        let store = LocalStore::open(paths.clone())?;
        let ssh_root = isolated_temp_root("deferred-blocked-pull-ssh")?;
        std::fs::create_dir_all(&ssh_root)?;
        let ssh = ssh_root.join("ssh");
        std::fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nprintf '%s\\n' 'schema-version = 1' '[policy.confirmation]' 'medium = \"blocked\"' > '{}'\ntarget=$(git config --get-regexp '^remote\\..*\\.testbare$' | cut -d' ' -f2-)\nexec git-upload-pack \"$target\"\n",
                store.paths().config_file.display()
            ),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&ssh)?.permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&ssh, permissions)?;
        }
        let executor = PromptSequenceExecutor {
            repo_root: repo.clone(),
            audit: AuditDestination::Paths(paths),
            github_executable: None,
            policy: EffectivePolicy::default(),
            ssh_executable: Some(ssh),
        };

        let result =
            executor.execute_confirmed(sequence.sequence, ConfirmationRequirement::VisiblePlan);

        let message = result.message();
        assert!(
            message.contains("Prompt sequence stopped before step 2 of 2."),
            "{message}"
        );
        assert_eq!(
            git_stdout(
                &repo,
                &["rev-parse", &format!("refs/remotes/origin/{branch}")]
            )?
            .trim(),
            original_tracking_oid
        );
        Ok(())
    }

    #[test]
    fn custom_policy_is_shared_by_manual_prompt_and_deferred_plans() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("custom-policy-planners")?;
        configure_git_identity(&repo)?;
        git_stdout(&repo, &["symbolic-ref", "HEAD", "refs/heads/production"])?;
        std::fs::write(repo.join("file.txt"), "ready\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        let mut config = AppConfig::default();
        config.policy.additional_protected_branches = vec!["production".to_owned()];
        config.policy.confirmation.medium = ConfirmationSetting::ExplicitConfirmation;
        let planner = OperationPlanner::with_policy(&repo, EffectivePolicy::new(&config));

        let ParsedPrompt::Single(prompt_request) = parse_prompt("commit -m ready")? else {
            return Err(std::io::Error::other("expected single prompt").into());
        };
        let prompt = planner
            .plan_request(prompt_request)
            .map_err(std::io::Error::other)?;
        let manual = planner
            .plan_request(OperationRequest::Commit {
                message: "ready".to_owned(),
            })
            .map_err(std::io::Error::other)?;
        assert_eq!(prompt, manual);
        assert_eq!(
            manual.plan.confirmation.requirement,
            ConfirmationRequirement::ExplicitConfirmation
        );
        let reason = manual
            .plan
            .confirmation
            .reason
            .as_deref()
            .unwrap_or_default();
        assert!(reason.contains("protected branch production"));
        assert!(reason.contains("medium risk policy requires explicit"));
        assert!(
            manual
                .plan
                .confirmation
                .prompt
                .contains("uppercase Y to commit")
        );

        let sequence = planner
            .plan_prompt_sequence(vec![
                OperationRequest::Commit {
                    message: "ready".to_owned(),
                },
                OperationRequest::Branches,
            ])
            .map_err(std::io::Error::other)?;
        assert_eq!(
            sequence.plan.confirmation.requirement,
            ConfirmationRequirement::ExplicitConfirmation
        );
        assert!(
            sequence
                .plan
                .confirmation
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("protected branch production"))
        );
        Ok(())
    }

    #[test]
    fn medium_explicit_policy_applies_to_low_risk_prompt_sequences() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("sequence-medium-explicit-policy")?;
        let mut config = AppConfig::default();
        config.policy.confirmation.medium = ConfirmationSetting::ExplicitConfirmation;

        let sequence = OperationPlanner::with_policy(&repo, EffectivePolicy::new(&config))
            .plan_prompt_sequence(vec![OperationRequest::Fetch, OperationRequest::Branches])
            .map_err(std::io::Error::other)?;

        assert_eq!(
            sequence.plan.confirmation.requirement,
            ConfirmationRequirement::ExplicitConfirmation
        );
        assert!(
            sequence
                .plan
                .confirmation
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("medium risk policy requires explicit"))
        );
        assert!(
            sequence
                .plan
                .confirmation
                .prompt
                .contains("uppercase Y to run 2 prompt steps")
        );
        Ok(())
    }

    #[test]
    fn medium_blocked_policy_applies_to_low_risk_prompt_sequences() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("sequence-medium-blocked-policy")?;
        let mut config = AppConfig::default();
        config.policy.confirmation.medium = ConfirmationSetting::Blocked;

        let sequence = OperationPlanner::with_policy(&repo, EffectivePolicy::new(&config))
            .plan_prompt_sequence(vec![OperationRequest::Fetch, OperationRequest::Branches])
            .map_err(std::io::Error::other)?;

        assert_eq!(
            sequence.plan.confirmation.requirement,
            ConfirmationRequirement::Blocked
        );
        assert!(
            sequence
                .plan
                .confirmation
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("medium risk policy requires blocked"))
        );
        assert!(
            sequence
                .plan
                .confirmation
                .prompt
                .contains("run 2 prompt steps")
        );
        Ok(())
    }

    #[test]
    fn low_explicit_policy_applies_to_low_risk_prompt_sequences() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("sequence-low-explicit-policy")?;
        let mut config = AppConfig::default();
        config.policy.confirmation.low = ConfirmationSetting::ExplicitConfirmation;

        let sequence = OperationPlanner::with_policy(&repo, EffectivePolicy::new(&config))
            .plan_prompt_sequence(vec![OperationRequest::Fetch, OperationRequest::Branches])
            .map_err(std::io::Error::other)?;

        assert_eq!(
            sequence.plan.confirmation.requirement,
            ConfirmationRequirement::ExplicitConfirmation
        );
        assert!(
            sequence
                .plan
                .confirmation
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("low risk policy requires explicit"))
        );
        assert!(
            sequence
                .plan
                .confirmation
                .prompt
                .contains("uppercase Y to run 2 prompt steps")
        );
        Ok(())
    }

    #[test]
    fn blocked_later_low_risk_step_blocks_sequence_before_checkout() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("sequence-later-low-blocked-policy")?;
        configure_git_identity(&repo)?;
        std::fs::write(repo.join("file.txt"), "base\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "initial"])?;
        git_stdout(&repo, &["branch", "feature/policy"])?;
        let original_branch = git_stdout(&repo, &["branch", "--show-current"])?;
        let mut config = AppConfig::default();
        config.policy.confirmation.low = ConfirmationSetting::Blocked;

        let sequence = OperationPlanner::with_policy(&repo, EffectivePolicy::new(&config))
            .plan_prompt_sequence(vec![
                OperationRequest::Checkout {
                    branch: "feature/policy".to_owned(),
                },
                OperationRequest::Fetch,
            ])
            .map_err(std::io::Error::other)?;

        assert_eq!(
            sequence.plan.confirmation.requirement,
            ConfirmationRequirement::Blocked
        );
        assert!(
            sequence
                .plan
                .confirmation
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("low risk policy requires blocked"))
        );
        let mut app = App::new();
        app.operation_queue.enqueue(QueuedOperation::new_sequence(
            sequence.plan,
            sequence.sequence,
        ));
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(
            git_stdout(&repo, &["branch", "--show-current"])?,
            original_branch
        );
        Ok(())
    }

    #[test]
    fn prompt_sequence_preview_tracks_protected_branch_transitions() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("sequence-protected-branch-transition")?;
        configure_git_identity(&repo)?;
        std::fs::write(repo.join("file.txt"), "base\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "initial"])?;
        git_stdout(&repo, &["branch", "production"])?;
        git_stdout(&repo, &["checkout", "-b", "feature/policy"])?;
        let mut config = AppConfig::default();
        config.policy.additional_protected_branches = vec!["production".to_owned()];
        let planner = OperationPlanner::with_policy(&repo, EffectivePolicy::new(&config));

        let to_protected = planner
            .plan_prompt_sequence(vec![
                OperationRequest::Checkout {
                    branch: "production".to_owned(),
                },
                OperationRequest::Push,
            ])
            .map_err(std::io::Error::other)?;
        assert!(
            to_protected
                .plan
                .confirmation
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("protected branch production"))
        );

        git_stdout(&repo, &["checkout", "production"])?;
        let to_unprotected = planner
            .plan_prompt_sequence(vec![
                OperationRequest::Checkout {
                    branch: "feature/policy".to_owned(),
                },
                OperationRequest::Push,
            ])
            .map_err(std::io::Error::other)?;
        assert!(
            !to_unprotected
                .plan
                .confirmation
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("protected branch production"))
        );

        git_stdout(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"])?;
        let remote_to_protected = planner
            .plan_prompt_sequence(vec![
                OperationRequest::Checkout {
                    branch: "origin/main".to_owned(),
                },
                OperationRequest::Push,
            ])
            .map_err(std::io::Error::other)?;
        assert!(
            remote_to_protected
                .plan
                .confirmation
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("protected branch main"))
        );
        Ok(())
    }

    #[test]
    fn executor_never_weakens_previewed_policy_and_rejects_stricter_policy()
    -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("policy-revalidation")?;
        let context = ExecutionContext::from_payload(PendingPayload::StageAll);
        let mut strict_config = AppConfig::default();
        strict_config.policy.confirmation.medium = ConfirmationSetting::ExplicitConfirmation;
        let strict_policy = EffectivePolicy::new(&strict_config);
        let mut strict_preview = stage_all_plan();
        apply_policy_to_plan(&strict_policy, &mut strict_preview, None);

        let default_executor = PlanExecutor {
            repo_root: repo.clone(),
            audit: AuditDestination::Paths(isolated_store_paths("policy-revalidation-default")?),
            policy: EffectivePolicy::default(),
            ssh_executable: None,
        };
        assert_eq!(
            strict_preview.confirmation.requirement,
            ConfirmationRequirement::ExplicitConfirmation
        );
        assert!(
            default_executor
                .validate_policy(&strict_preview, &context)
                .is_ok()
        );

        let default_preview = stage_all_plan();
        let strict_executor = PlanExecutor {
            repo_root: repo,
            audit: AuditDestination::Paths(isolated_store_paths("policy-revalidation-strict")?),
            policy: strict_policy,
            ssh_executable: None,
        };
        let error = match strict_executor.validate_policy(&default_preview, &context) {
            Ok(()) => {
                return Err(std::io::Error::other(
                    "stricter current policy must require a new preview",
                )
                .into());
            }
            Err(error) => error,
        };
        assert!(error.contains("review the updated plan"));
        Ok(())
    }

    #[test]
    fn executor_rejects_disabled_steps_without_side_effects() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("disabled-execution")?;
        std::fs::write(repo.join("file.txt"), "unstaged\n")?;
        let mut config = AppConfig::default();
        config.policy.disabled_operations = vec![OperationFamily::Stage];
        let executor = PlanExecutor {
            repo_root: repo.clone(),
            audit: AuditDestination::Paths(isolated_store_paths("disabled-execution-audit")?),
            policy: EffectivePolicy::new(&config),
            ssh_executable: None,
        };

        let result = executor.execute(
            &stage_all_plan(),
            ExecutionContext::from_payload(PendingPayload::StageAll),
        );

        assert!(!result.succeeded());
        assert!(
            result.message().contains("stage all is disabled by policy"),
            "{}",
            result.message()
        );
        assert!(
            git_stdout(&repo, &["diff", "--cached", "--name-only"])?
                .trim()
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn policy_reload_preserves_restrictive_policy_for_invalid_or_unreadable_config()
    -> Result<(), Box<dyn Error>> {
        let mut config = AppConfig::default();
        config.prompt.enabled = false;
        config.policy.disabled_operations = vec![OperationFamily::Fetch];
        let restrictive_policy = EffectivePolicy::new(&config);

        let invalid_paths = isolated_store_paths("invalid-policy-reload")?;
        let invalid_store = LocalStore::open(invalid_paths.clone())?;
        std::fs::write(&invalid_store.paths().config_file, "not valid toml = [")?;
        let invalid_reload =
            AuditDestination::Paths(invalid_paths).reload_policy(&restrictive_policy);
        assert_eq!(invalid_reload.policy, restrictive_policy);
        assert!(
            invalid_reload
                .diagnostic
                .is_some_and(|diagnostic| diagnostic.contains("is invalid")
                    && diagnostic.contains("retaining the last valid policy"))
        );

        let unreadable_paths = isolated_store_paths("unreadable-policy-reload")?;
        let unreadable_store = LocalStore::open(unreadable_paths.clone())?;
        std::fs::create_dir(&unreadable_store.paths().config_file)?;
        let unreadable_reload =
            AuditDestination::Paths(unreadable_paths).reload_policy(&restrictive_policy);
        assert_eq!(unreadable_reload.policy, restrictive_policy);
        assert!(
            unreadable_reload
                .diagnostic
                .is_some_and(|diagnostic| diagnostic.contains("could not be read")
                    && diagnostic.contains("retaining the last valid policy"))
        );
        Ok(())
    }

    #[test]
    fn startup_policy_fails_closed_and_surfaces_malformed_config() -> Result<(), Box<dyn Error>> {
        let paths = isolated_store_paths("invalid-startup-policy")?;
        let store = LocalStore::open(paths.clone())?;
        std::fs::write(
            &store.paths().config_file,
            "credential = \"super-secret-value\"\nnot valid toml = [",
        )?;

        let startup = startup_policy_from_paths(&paths);

        assert_eq!(startup.policy, EffectivePolicy::safe_fallback());
        assert!(
            startup
                .diagnostic
                .as_deref()
                .is_some_and(|diagnostic| diagnostic.contains("is invalid"))
        );
        let app = App::from_startup(startup);
        let details = details_text(&app);
        assert!(details.contains("safe fallback configuration"));
        assert!(!details.contains("super-secret-value"));
        Ok(())
    }

    #[test]
    fn startup_policy_fails_closed_and_surfaces_unreadable_config() -> Result<(), Box<dyn Error>> {
        let paths = isolated_store_paths("unreadable-startup-policy")?;
        let store = LocalStore::open(paths.clone())?;
        std::fs::create_dir(&store.paths().config_file)?;

        let startup = startup_policy_from_paths(&paths);

        assert_eq!(startup.policy, EffectivePolicy::safe_fallback());
        assert!(
            startup
                .diagnostic
                .as_deref()
                .is_some_and(|diagnostic| diagnostic.contains("could not be read"))
        );
        let app = App::from_startup(startup);
        assert!(details_text(&app).contains("safe fallback configuration"));
        Ok(())
    }

    #[test]
    fn startup_fail_closed_mode_recovers_after_config_is_fixed() -> Result<(), Box<dyn Error>> {
        let paths = isolated_store_paths("startup-policy-recovery")?;
        let store = LocalStore::open(paths.clone())?;
        std::fs::write(&store.paths().config_file, "not valid toml = [")?;
        let mut app = App::from_startup(startup_policy_from_paths(&paths));

        assert_eq!(app.policy, EffectivePolicy::safe_fallback());
        assert!(app.branch.is_none());
        assert!(app.config_diagnostic.is_some());

        std::fs::write(
            &store.paths().config_file,
            "schema-version = 1\n[policy]\ndisabled-operations = [\"fetch\", \"stage\", \"unstage\", \"commit\", \"push\", \"pull\", \"checkout\", \"create-branch\", \"merge\", \"rebase\", \"open-pull-request\"]\n[prompt]\nenabled = false\n",
        )?;
        app.reload_policy(&AuditDestination::Paths(paths));

        assert!(app.config_diagnostic.is_none());
        assert!(app.branch.is_some(), "status should reload after recovery");
        assert!(!app.policy.prompt_enabled());
        assert!(app.policy.is_operation_disabled(OperationKind::Fetch));
        assert!(
            !app.policy
                .is_operation_disabled(OperationKind::RefreshStatus)
        );
        Ok(())
    }

    #[test]
    fn runtime_reload_diagnostic_stays_visible_until_valid_recovery() -> Result<(), Box<dyn Error>>
    {
        let paths = isolated_store_paths("runtime-policy-diagnostic")?;
        let store = LocalStore::open(paths.clone())?;
        std::fs::write(
            &store.paths().config_file,
            "schema-version = 1\n[policy]\ndisabled-operations = [\"fetch\"]\n[prompt]\nenabled = false\n",
        )?;
        let mut app = App::from_startup(startup_policy_from_paths(&paths));
        let restrictive_policy = app.policy.clone();
        app.details = "latest operation detail".to_owned();

        std::fs::write(&store.paths().config_file, "not valid toml = [")?;
        app.reload_policy(&AuditDestination::Paths(paths.clone()));

        assert_eq!(app.policy, restrictive_policy);
        let visible_details = details_text(&app);
        assert!(visible_details.contains("Configuration reload error"));
        assert!(visible_details.contains("retaining the last valid policy"));
        assert!(visible_details.contains("latest operation detail"));

        app.branch = None;
        std::fs::write(
            &store.paths().config_file,
            "schema-version = 1\n[policy]\ndisabled-operations = [\"commit\"]\n",
        )?;
        app.reload_policy(&AuditDestination::Paths(paths));

        assert!(app.config_diagnostic.is_none());
        assert!(app.branch.is_some(), "valid recovery should refresh status");
        assert!(app.policy.is_operation_disabled(OperationKind::Commit));
        assert!(!app.policy.is_operation_disabled(OperationKind::Fetch));
        Ok(())
    }

    #[test]
    fn startup_status_honors_escalated_and_blocked_low_risk_policy() {
        for setting in [
            ConfirmationSetting::VisiblePlan,
            ConfirmationSetting::ExplicitConfirmation,
            ConfirmationSetting::Blocked,
        ] {
            let mut config = AppConfig::default();
            config.policy.confirmation.low = setting;

            let app = App::from_startup(StartupPolicy {
                policy: EffectivePolicy::new(&config),
                diagnostic: None,
            });

            assert!(app.branch.is_none());
            assert!(app.details.contains("Operation blocked: refresh status"));
        }
    }

    #[test]
    fn status_selection_diff_honors_blocked_confirmation_and_disabled_policy() {
        let mut config = AppConfig::default();
        config.policy.confirmation.low = ConfirmationSetting::Blocked;
        let mut app = App::with_policy(EffectivePolicy::new(&config));
        app.focus = Focus::Status;
        app.files = ["first.txt", "second.txt"]
            .into_iter()
            .map(|path| FileRow {
                path: PathBuf::from(path),
                pathspecs: vec![PathBuf::from(path)],
                label: path.to_owned(),
                section: FileSection::Unstaged,
            })
            .collect();

        app.move_selection_down();

        assert_eq!(app.selected_file, 1);
        assert_eq!(
            app.details,
            "Operation blocked: view diff is blocked by confirmation policy."
        );

        config.policy.confirmation.low = ConfirmationSetting::NormalSelection;
        config.policy.disabled_operations = vec![OperationFamily::ViewDiff];
        app.policy = EffectivePolicy::new(&config);
        app.selected_file = 0;
        app.move_selection_down();
        assert_eq!(
            app.details,
            "Operation blocked: view diff is disabled by policy."
        );
    }

    #[test]
    fn ordinary_queue_rejects_new_protected_reason_without_requirement_change() {
        let target = HeadTarget {
            oid: Some("1234567890abcdef".to_owned()),
            reference: Some("refs/heads/production".to_owned()),
        };
        let context = ExecutionContext::from_payload(PendingPayload::Commit {
            staged_items: vec!["file.txt".to_owned()],
            staged_tree: "abcdef1234567890".to_owned(),
            target,
        });
        let mut preview = commit_plan("ready", 1);
        apply_policy_to_plan(
            &EffectivePolicy::default(),
            &mut preview,
            Some("production"),
        );
        let mut config = AppConfig::default();
        config.policy.additional_protected_branches = vec!["production".to_owned()];
        let mut app = App::new();
        app.queue_operation(preview.clone(), context);
        app.policy = EffectivePolicy::new(&config);

        assert_eq!(
            preview.confirmation.requirement,
            ConfirmationRequirement::VisiblePlan
        );
        app.handle_key(key(KeyCode::Char('y')));
        assert!(
            app.details.contains("new protected-branch rules"),
            "{}",
            app.details
        );
    }

    #[test]
    fn deferred_prompt_step_reloads_policy_changed_during_prior_step() -> Result<(), Box<dyn Error>>
    {
        let repo = isolated_git_repo("deferred-policy-revalidation")?;
        let remote = isolated_bare_git_repo("deferred-policy-revalidation-remote")?;
        configure_git_identity(&repo)?;
        std::fs::write(repo.join("file.txt"), "base\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "initial"])?;
        let base = git_stdout(&repo, &["branch", "--show-current"])?
            .trim()
            .to_owned();
        git_stdout(&repo, &["checkout", "-b", "feature/policy"])?;
        add_github_remote(&repo, "origin", &remote)?;
        let sequence = OperationPlanner::new(&repo)
            .plan_prompt_sequence(vec![
                OperationRequest::Fetch,
                OperationRequest::Rebase { base },
            ])
            .map_err(std::io::Error::other)?;
        assert_eq!(
            sequence.plan.confirmation.requirement,
            ConfirmationRequirement::ExplicitConfirmation
        );
        let paths = isolated_store_paths("deferred-policy-revalidation-audit")?;
        let store = LocalStore::open(paths.clone())?;
        let ssh_root = isolated_temp_root("deferred-policy-revalidation-ssh")?;
        std::fs::create_dir_all(&ssh_root)?;
        let ssh = ssh_root.join("ssh");
        std::fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nprintf '%s\\n' 'schema-version = 1' '[policy.confirmation]' 'high = \"blocked\"' > '{}'\ntarget=$(git config --get-regexp '^remote\\..*\\.testbare$' | cut -d' ' -f2-)\nexec git-upload-pack \"$target\"\n",
                store.paths().config_file.display()
            ),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&ssh)?.permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&ssh, permissions)?;
        }
        let executor = PromptSequenceExecutor {
            repo_root: repo,
            audit: AuditDestination::Paths(paths),
            github_executable: None,
            policy: EffectivePolicy::default(),
            ssh_executable: Some(ssh),
        };

        let result = executor.execute_confirmed(
            sequence.sequence,
            ConfirmationRequirement::ExplicitConfirmation,
        );

        let message = result.message();
        assert!(
            message.contains("Prompt sequence stopped before step 2 of 2."),
            "{message}"
        );
        assert!(message.contains("new sequence preview"), "{message}");
        Ok(())
    }

    #[test]
    fn deferred_prompt_step_stops_when_operation_is_disabled_during_sequence()
    -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("deferred-disabled-operation")?;
        let remote = isolated_bare_git_repo("deferred-disabled-operation-remote")?;
        configure_git_identity(&repo)?;
        std::fs::write(repo.join("base.txt"), "base\n")?;
        git_stdout(&repo, &["add", "base.txt"])?;
        git_stdout(&repo, &["commit", "-m", "initial"])?;
        std::fs::write(repo.join("next.txt"), "next\n")?;
        git_stdout(&repo, &["add", "next.txt"])?;
        add_github_remote(&repo, "origin", &remote)?;
        let original_head = git_stdout(&repo, &["rev-parse", "HEAD"])?;
        let sequence = OperationPlanner::new(&repo)
            .plan_prompt_sequence(vec![
                OperationRequest::Fetch,
                OperationRequest::Commit {
                    message: "next".to_owned(),
                },
            ])
            .map_err(std::io::Error::other)?;
        let paths = isolated_store_paths("deferred-disabled-operation-audit")?;
        let store = LocalStore::open(paths.clone())?;
        let ssh = operation_disabling_ssh(
            "deferred-disabled-operation",
            &store.paths().config_file,
            "commit",
        )?;
        let executor = PromptSequenceExecutor {
            repo_root: repo.clone(),
            audit: AuditDestination::Paths(paths),
            github_executable: None,
            policy: EffectivePolicy::default(),
            ssh_executable: Some(ssh),
        };

        let result =
            executor.execute_confirmed(sequence.sequence, ConfirmationRequirement::VisiblePlan);

        let message = result.message();
        assert!(
            message.contains("Prompt sequence stopped before step 2 of 2."),
            "{message}"
        );
        assert!(
            message.contains("commit is disabled by policy"),
            "{message}"
        );
        assert_eq!(git_stdout(&repo, &["rev-parse", "HEAD"])?, original_head);
        assert_eq!(
            git_stdout(&repo, &["diff", "--cached", "--name-only"])?.trim(),
            "next.txt"
        );
        Ok(())
    }

    #[test]
    fn app_reloads_policy_before_ordinary_post_operation_refresh_on_success_or_failure()
    -> Result<(), Box<dyn Error>> {
        for (name, remote_exists, expected_outcome) in
            [("success", true, "succeeded"), ("failure", false, "failed")]
        {
            let repo = isolated_git_repo(&format!("post-operation-refresh-policy-{name}"))?;
            let remote = if remote_exists {
                isolated_bare_git_repo(&format!("post-operation-refresh-policy-{name}-remote"))?
            } else {
                repo.join("missing-remote.git")
            };
            add_github_remote(&repo, "origin", &remote)?;
            let paths =
                isolated_store_paths(&format!("post-operation-refresh-policy-{name}-audit"))?;
            let store = LocalStore::open(paths.clone())?;
            let ssh = operation_disabling_ssh(
                &format!("post-operation-refresh-policy-{name}"),
                &store.paths().config_file,
                "refresh-status",
            )?;
            let executor = PlanExecutor::with_audit_paths_and_ssh(&repo, paths, ssh);
            let sentinel = FileRow {
                path: PathBuf::from("keep.txt"),
                pathspecs: vec![PathBuf::from("keep.txt")],
                label: "sentinel".to_owned(),
                section: FileSection::Unstaged,
            };
            let mut app = App::new();
            app.files = vec![sentinel.clone()];

            app.execute_operation_with(&executor, fetch_plan(), ExecutionContext::default());

            assert!(
                app.policy
                    .is_operation_disabled(OperationKind::RefreshStatus)
            );
            assert_eq!(app.files, vec![sentinel]);
            assert!(app.details.contains(expected_outcome), "{}", app.details);
        }
        Ok(())
    }

    #[test]
    fn app_uses_reloaded_policy_for_post_sequence_status_refresh() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("post-sequence-refresh-policy")?;
        let remote = isolated_bare_git_repo("post-sequence-refresh-policy-remote")?;
        add_github_remote(&repo, "origin", &remote)?;
        let sequence = OperationPlanner::new(&repo)
            .plan_prompt_sequence(vec![OperationRequest::Fetch, OperationRequest::Branches])
            .map_err(std::io::Error::other)?;
        let paths = isolated_store_paths("post-sequence-refresh-policy-audit")?;
        let store = LocalStore::open(paths.clone())?;
        let ssh = operation_disabling_ssh(
            "post-sequence-refresh-policy",
            &store.paths().config_file,
            "refresh-status",
        )?;
        let executor = PromptSequenceExecutor {
            repo_root: repo,
            audit: AuditDestination::Paths(paths),
            github_executable: None,
            policy: EffectivePolicy::default(),
            ssh_executable: Some(ssh),
        };
        let sentinel = FileRow {
            path: PathBuf::from("keep.txt"),
            pathspecs: vec![PathBuf::from("keep.txt")],
            label: "sentinel".to_owned(),
            section: FileSection::Unstaged,
        };
        let mut app = App::new();
        app.files = vec![sentinel.clone()];

        app.execute_prompt_sequence_with(
            &executor,
            sequence.sequence,
            ConfirmationRequirement::VisiblePlan,
        );

        assert!(
            app.policy
                .is_operation_disabled(OperationKind::RefreshStatus)
        );
        assert_eq!(app.files, vec![sentinel]);
        assert!(app.details.contains("Prompt sequence completed 2 step(s)."));
        Ok(())
    }

    #[test]
    fn low_risk_sequence_rejects_changed_medium_policy_at_confirmation()
    -> Result<(), Box<dyn Error>> {
        for (name, setting) in [
            ("explicit", "explicit-confirmation"),
            ("blocked", "blocked"),
        ] {
            let repo = isolated_git_repo(&format!("sequence-medium-change-{name}"))?;
            let sequence = OperationPlanner::new(&repo)
                .plan_prompt_sequence(vec![OperationRequest::Fetch, OperationRequest::Branches])
                .map_err(std::io::Error::other)?;
            let paths = isolated_store_paths(&format!("sequence-medium-change-{name}-audit"))?;
            let store = LocalStore::open(paths.clone())?;
            std::fs::write(
                &store.paths().config_file,
                format!("schema-version = 1\n[policy.confirmation]\nmedium = \"{setting}\"\n"),
            )?;

            let result = PromptSequenceExecutor::with_audit_paths(&repo, paths)
                .execute_confirmed(sequence.sequence, ConfirmationRequirement::VisiblePlan);

            let message = result.message();
            assert!(
                message.contains("Prompt sequence stopped before step 1 of 2."),
                "{message}"
            );
            assert!(message.contains("new sequence preview"), "{message}");
        }
        Ok(())
    }

    #[test]
    fn confirmation_preflights_blocked_later_high_risk_step() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("sequence-confirmation-later-high")?;
        configure_git_identity(&repo)?;
        std::fs::write(repo.join("file.txt"), "base\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "initial"])?;
        let original_branch = git_stdout(&repo, &["branch", "--show-current"])?;
        git_stdout(&repo, &["branch", "feature/policy"])?;
        let sequence = OperationPlanner::new(&repo)
            .plan_prompt_sequence(vec![
                OperationRequest::Checkout {
                    branch: "feature/policy".to_owned(),
                },
                OperationRequest::Rebase {
                    base: original_branch.trim().to_owned(),
                },
            ])
            .map_err(std::io::Error::other)?;
        let paths = isolated_store_paths("sequence-confirmation-later-high-audit")?;
        let store = LocalStore::open(paths.clone())?;
        std::fs::write(
            &store.paths().config_file,
            "schema-version = 1\n[policy.confirmation]\nhigh = \"blocked\"\n",
        )?;

        let result = PromptSequenceExecutor::with_audit_paths(&repo, paths).execute_confirmed(
            sequence.sequence,
            ConfirmationRequirement::ExplicitConfirmation,
        );

        let message = result.message();
        assert!(
            message.contains("Prompt sequence stopped before step 1 of 2."),
            "{message}"
        );
        assert!(message.contains("new sequence preview"), "{message}");
        assert_eq!(
            git_stdout(&repo, &["branch", "--show-current"])?,
            original_branch
        );
        Ok(())
    }

    #[test]
    fn confirmation_preflights_new_later_protected_branch_reason() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("sequence-confirmation-later-protected")?;
        configure_git_identity(&repo)?;
        std::fs::write(repo.join("file.txt"), "base\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "initial"])?;
        git_stdout(&repo, &["branch", "production"])?;
        git_stdout(&repo, &["checkout", "-b", "feature/policy"])?;
        let original_branch = git_stdout(&repo, &["branch", "--show-current"])?;
        let sequence = OperationPlanner::new(&repo)
            .plan_prompt_sequence(vec![
                OperationRequest::Checkout {
                    branch: "production".to_owned(),
                },
                OperationRequest::Push,
            ])
            .map_err(std::io::Error::other)?;
        let paths = isolated_store_paths("sequence-confirmation-later-protected-audit")?;
        let store = LocalStore::open(paths.clone())?;
        std::fs::write(
            &store.paths().config_file,
            "schema-version = 1\n[policy]\nadditional-protected-branches = [\"production\"]\n",
        )?;

        let result = PromptSequenceExecutor::with_audit_paths(&repo, paths)
            .execute_confirmed(sequence.sequence, ConfirmationRequirement::VisiblePlan);

        let message = result.message();
        assert!(
            message.contains("Prompt sequence stopped before step 1 of 2."),
            "{message}"
        );
        assert!(message.contains("new sequence preview"), "{message}");
        assert_eq!(
            git_stdout(&repo, &["branch", "--show-current"])?,
            original_branch
        );
        Ok(())
    }

    #[test]
    fn later_low_risk_step_reloads_changed_medium_policy() -> Result<(), Box<dyn Error>> {
        let repo = isolated_git_repo("sequence-medium-change-during-fetch")?;
        let remote = isolated_bare_git_repo("sequence-medium-change-during-fetch-remote")?;
        add_github_remote(&repo, "origin", &remote)?;
        let sequence = OperationPlanner::new(&repo)
            .plan_prompt_sequence(vec![OperationRequest::Fetch, OperationRequest::Branches])
            .map_err(std::io::Error::other)?;
        let paths = isolated_store_paths("sequence-medium-change-during-fetch-audit")?;
        let store = LocalStore::open(paths.clone())?;
        let ssh_root = isolated_temp_root("sequence-medium-change-during-fetch-ssh")?;
        std::fs::create_dir_all(&ssh_root)?;
        let ssh = ssh_root.join("ssh");
        std::fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nprintf '%s\\n' 'schema-version = 1' '[policy.confirmation]' 'medium = \"explicit-confirmation\"' > '{}'\ntarget=$(git config --get-regexp '^remote\\..*\\.testbare$' | cut -d' ' -f2-)\nexec git-upload-pack \"$target\"\n",
                store.paths().config_file.display()
            ),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&ssh)?.permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&ssh, permissions)?;
        }
        let executor = PromptSequenceExecutor {
            repo_root: repo,
            audit: AuditDestination::Paths(paths),
            github_executable: None,
            policy: EffectivePolicy::default(),
            ssh_executable: Some(ssh),
        };

        let result =
            executor.execute_confirmed(sequence.sequence, ConfirmationRequirement::VisiblePlan);

        let message = result.message();
        assert!(
            message.contains("Prompt sequence stopped before step 2 of 2."),
            "{message}"
        );
        assert!(message.contains("new sequence preview"), "{message}");
        Ok(())
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
                    tracking_oid: None,
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
    fn desktop_queue_keeps_confirmation_keys_visible_when_plan_details_wrap()
    -> Result<(), Box<dyn Error>> {
        let backend = TestBackend::new(50, 24);
        let mut terminal = Terminal::new(backend)?;
        let mut app = App::new();
        let mut plan = OperationPlan::new(
            OperationRequest::PromptSequence {
                requests: vec![
                    OperationRequest::Checkout {
                        branch: "feature/a-very-long-branch-name".to_owned(),
                    },
                    OperationRequest::Rebase {
                        base: "origin/a-very-long-base-branch-name".to_owned(),
                    },
                ],
            },
            "Prompt sequence plan",
            vec![
                OperationStep::new(
                    OperationKind::CheckoutBranch,
                    RiskLevel::Medium,
                    "checkout a branch whose preview wraps across multiple queue rows",
                ),
                OperationStep::new(
                    OperationKind::Rebase,
                    RiskLevel::High,
                    "rebase onto a long remote branch after checkout succeeds",
                ),
            ],
            "Explicit confirmation required: press uppercase Y to run 2 prompt steps or n to cancel.",
        );
        plan.confirmation.reason = Some(
            "high risk policy requires explicit confirmation; protected branch production: rebase requires at least explicit confirmation"
                .to_owned(),
        );
        app.operation_queue
            .enqueue(QueuedOperation::new(plan, ExecutionContext::default()));

        terminal.draw(|frame| {
            let viewport = Viewport::split(frame.area());
            render(&app, frame, &viewport);
        })?;

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Accepted keys: uppercase Y confirm; n/Esc cancel"));
        assert!(rendered.contains("protected branch production"));
        assert!(rendered.contains("Confirm: run 2 prompt steps"));
        Ok(())
    }

    #[test]
    fn desktop_queue_shows_controlling_promoted_risk_reason() -> Result<(), Box<dyn Error>> {
        for (name, setting, accepted_keys, controlling_reason) in [
            (
                "explicit",
                ConfirmationSetting::ExplicitConfirmation,
                "Accepted keys: uppercase Y confirm; n/Esc cancel",
                "medium risk policy requires explicit",
            ),
            (
                "blocked",
                ConfirmationSetting::Blocked,
                "Accepted keys: n/Esc dismiss; confirm disabled",
                "medium risk policy requires blocked",
            ),
        ] {
            let repo = isolated_git_repo(&format!("queue-promoted-risk-reason-{name}"))?;
            let mut config = AppConfig::default();
            config.policy.confirmation.medium = setting;
            let sequence = OperationPlanner::with_policy(&repo, EffectivePolicy::new(&config))
                .plan_prompt_sequence(vec![OperationRequest::Fetch, OperationRequest::Branches])
                .map_err(std::io::Error::other)?;
            assert!(policy_reason(&sequence.plan).starts_with(controlling_reason));
            let backend = TestBackend::new(50, 24);
            let mut terminal = Terminal::new(backend)?;
            let mut app = App::new();
            app.operation_queue.enqueue(QueuedOperation::new_sequence(
                sequence.plan,
                sequence.sequence,
            ));

            terminal.draw(|frame| {
                let viewport = Viewport::split(frame.area());
                render(&app, frame, &viewport);
            })?;

            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(rendered.contains(accepted_keys), "{rendered}");
            assert!(
                rendered.contains(&format!("Policy: {controlling_reason}")),
                "{rendered}"
            );
        }
        Ok(())
    }

    #[test]
    fn compact_queue_renders_confirmation_keys_policy_and_action() -> Result<(), Box<dyn Error>> {
        for (name, requirement, reason, keys, policy) in [
            (
                "explicit-protected",
                ConfirmationRequirement::ExplicitConfirmation,
                "protected branch production: rebase requires at least explicit confirmation",
                "Keys: Y confirm; n/Esc cancel",
                "Policy: protected branch production",
            ),
            (
                "blocked-medium",
                ConfirmationRequirement::Blocked,
                "medium risk policy requires blocked confirmation",
                "Keys: n/Esc dismiss; confirm disabled",
                "Policy: medium risk: blocked",
            ),
        ] {
            let mut plan = OperationPlan::new(
                OperationRequest::PromptSequence {
                    requests: vec![OperationRequest::Fetch, OperationRequest::Branches],
                },
                "Prompt sequence plan",
                vec![
                    OperationStep::new(OperationKind::Fetch, RiskLevel::Medium, "fetch"),
                    OperationStep::new(OperationKind::Branches, RiskLevel::Medium, "branches"),
                ],
                "Press y to run 2 prompt steps or n to cancel.",
            );
            plan.confirmation.requirement = requirement;
            plan.confirmation.reason = Some(reason.to_owned());
            plan.confirmation.prompt = confirmation_prompt(&plan, requirement);
            let mut app = App::new();
            app.operation_queue
                .enqueue(QueuedOperation::new(plan, ExecutionContext::default()));
            let backend = TestBackend::new(40, 12);
            let mut terminal = Terminal::new(backend)?;

            terminal.draw(|frame| {
                let viewport = Viewport::split(frame.area());
                render(&app, frame, &viewport);
            })?;

            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(rendered.contains(keys), "{name}: {rendered}");
            assert!(rendered.contains(policy), "{name}: {rendered}");
            assert!(
                rendered.contains("Confirm: run 2 prompt steps"),
                "{name}: {rendered}"
            );
        }
        Ok(())
    }

    #[test]
    fn tiny_queue_renders_keys_policy_reason_and_confirmation_action() -> Result<(), Box<dyn Error>>
    {
        let mut plan = OperationPlan::new(
            OperationRequest::PromptSequence {
                requests: vec![OperationRequest::Fetch, OperationRequest::Branches],
            },
            "Prompt sequence plan",
            vec![OperationStep::new(
                OperationKind::Fetch,
                RiskLevel::High,
                "fetch",
            )],
            "Explicit confirmation required: press uppercase Y to run 2 prompt steps or n to cancel.",
        );
        plan.confirmation.reason = Some(
            "protected branch production: rebase requires at least explicit confirmation"
                .to_owned(),
        );
        let mut app = App::new();
        app.operation_queue
            .enqueue(QueuedOperation::new(plan, ExecutionContext::default()));
        let backend = TestBackend::new(20, 8);
        let mut terminal = Terminal::new(backend)?;

        terminal.draw(|frame| {
            let viewport = Viewport::split(frame.area());
            render(&app, frame, &viewport);
        })?;

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Keys: Y/n/Esc"), "{rendered}");
        assert!(rendered.contains("Protected: product"), "{rendered}");
        assert!(rendered.contains("run 2 prompt steps"), "{rendered}");
        Ok(())
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
        add_github_remote(&repo, "origin", &remote)?;
        git_stdout_with_ssh(
            &repo,
            &["push", "-u", "origin", "feature/open-pr"],
            &test_ssh_command()?,
        )?;
        Ok(repo)
    }

    fn stale_pull_tracking_repo(name: &str) -> Result<(PathBuf, String, String), Box<dyn Error>> {
        let repo = isolated_git_repo(name)?;
        let remote = isolated_bare_git_repo(&format!("{name}-remote"))?;
        let ssh = test_ssh_command()?;
        configure_git_identity(&repo)?;
        std::fs::write(repo.join("file.txt"), "base\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "initial"])?;
        let branch = git_stdout(&repo, &["branch", "--show-current"])?
            .trim()
            .to_owned();
        let original_oid = git_stdout(&repo, &["rev-parse", "HEAD"])?.trim().to_owned();
        add_github_remote(&repo, "origin", &remote)?;
        git_stdout_with_ssh(&repo, &["push", "-u", "origin", &branch], &ssh)?;
        git_stdout_with_ssh(&repo, &["push", "origin", "HEAD:trigger"], &ssh)?;
        std::fs::write(repo.join("file.txt"), "remote update\n")?;
        git_stdout(&repo, &["add", "file.txt"])?;
        git_stdout(&repo, &["commit", "-m", "remote update"])?;
        git_stdout_with_ssh(&repo, &["push", "origin", &branch], &ssh)?;
        git_stdout(&repo, &["switch", "--detach", &original_oid])?;
        git_stdout(&repo, &["branch", "-f", &branch, &original_oid])?;
        git_stdout(&repo, &["switch", &branch])?;
        git_stdout(
            &repo,
            &[
                "update-ref",
                &format!("refs/remotes/origin/{branch}"),
                &original_oid,
            ],
        )?;
        Ok((repo, branch, original_oid))
    }

    fn add_github_remote(
        repo: &std::path::Path,
        name: &str,
        bare_remote: &std::path::Path,
    ) -> Result<(), Box<dyn Error>> {
        let remote_url = "ssh://git@github.com/octo/repo.git";
        let test_bare_key = format!("remote.{name}.testbare");
        git_stdout(repo, &["remote", "add", name, remote_url])?;
        git_stdout(
            repo,
            &[
                "config",
                test_bare_key.as_str(),
                &bare_remote.display().to_string(),
            ],
        )?;
        Ok(())
    }

    fn git_stdout_with_ssh(
        repo: &std::path::Path,
        args: &[&str],
        ssh_executable: &std::path::Path,
    ) -> Result<String, Box<dyn Error>> {
        let output = std::process::Command::new("git")
            .current_dir(repo)
            .env("GIT_SSH_COMMAND", ssh_executable)
            .args(args)
            .output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "git command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
            .into());
        }
        Ok(String::from_utf8(output.stdout)?)
    }

    fn test_ssh_command() -> Result<PathBuf, Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::OnceLock;

        static COMMAND: OnceLock<Result<PathBuf, String>> = OnceLock::new();
        match COMMAND.get_or_init(|| {
            (|| -> Result<PathBuf, Box<dyn Error>> {
                let root = isolated_temp_root("fake-ssh")?;
                std::fs::create_dir_all(&root)?;
                let executable = root.join("ssh");
                std::fs::write(
                    &executable,
                    "#!/bin/sh\ntarget=$(git config --get-regexp '^remote\\..*\\.testbare$' | cut -d' ' -f2-)\ncase \"$*\" in\n*git-receive-pack*) exec git-receive-pack \"$target\" ;;\n*) exec git-upload-pack \"$target\" ;;\nesac\n",
                )?;
                let mut permissions = std::fs::metadata(&executable)?.permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&executable, permissions)?;
                Ok(executable)
            })()
            .map_err(|error| error.to_string())
        }) {
            Ok(command) => Ok(command.clone()),
            Err(error) => Err(std::io::Error::other(error.clone()).into()),
        }
    }

    fn policy_changing_ssh(
        name: &str,
        config_file: &std::path::Path,
        risk: &str,
        setting: &str,
    ) -> Result<PathBuf, Box<dyn Error>> {
        let root = isolated_temp_root(&format!("{name}-ssh"))?;
        std::fs::create_dir_all(&root)?;
        let executable = root.join("ssh");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s\\n' 'schema-version = 1' '[policy.confirmation]' '{risk} = \"{setting}\"' > '{}'\ntarget=$(git config --get-regexp '^remote\\..*\\.testbare$' | cut -d' ' -f2-)\nexec git-upload-pack \"$target\"\n",
                config_file.display()
            ),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&executable)?.permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&executable, permissions)?;
        }
        Ok(executable)
    }

    fn operation_disabling_ssh(
        name: &str,
        config_file: &std::path::Path,
        operation: &str,
    ) -> Result<PathBuf, Box<dyn Error>> {
        let root = isolated_temp_root(&format!("{name}-ssh"))?;
        std::fs::create_dir_all(&root)?;
        let executable = root.join("ssh");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s\\n' 'schema-version = 1' '[policy]' 'disabled-operations = [\"{operation}\"]' > '{}'\ntarget=$(git config --get-regexp '^remote\\..*\\.testbare$' | cut -d' ' -f2-)\nexec git-upload-pack \"$target\"\n",
                config_file.display()
            ),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&executable)?.permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&executable, permissions)?;
        }
        Ok(executable)
    }

    fn fake_gh(name: &str, existing: bool) -> Result<PathBuf, Box<dyn Error>> {
        let pull_requests = if existing {
            "[{\"number\":42,\"html_url\":\"https://github.com/octo/repo/pull/42\",\"title\":\"Existing PR\",\"base\":{\"ref\":\"main\"},\"head\":{\"ref\":\"feature/open-pr\",\"repo\":{\"full_name\":\"octo/repo\"}}}]"
        } else {
            "[]"
        };
        fake_gh_with_pull_requests(name, pull_requests, true)
    }

    fn fake_gh_with_pull_requests(
        name: &str,
        pull_requests: &str,
        create_succeeds: bool,
    ) -> Result<PathBuf, Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let root = isolated_temp_root(&format!("fake-gh-{name}"))?;
        std::fs::create_dir_all(&root)?;
        let executable = root.join("gh");
        let invocations = root.join("invocations");
        let base_missing = root.join("base-missing");
        let head_missing = root.join("head-missing");
        let auth_fails = root.join("auth-fails");
        let create_response = if create_succeeds {
            "printf 'https://%s/%s/pull/43\\n' \"$GH_HOST\" \"$repository\" ;;"
        } else {
            "exit 1 ;;"
        };
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s:%s %s\\n' \"$1\" \"$2\" \"$*\" >> '{}'\nrepository=octo/repo\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    github.com/*/*) repository=${{arg#github.com/}} ;;\n  esac\ndone\nif [ \"$repository\" = octo/old-repo ]; then repository=octo/repo; fi\ncase \"$1:$2\" in\n--version:*) exit 0 ;;\nauth:status) [ ! -f '{}' ] ;;\nrepo:view) case \"$*\" in *--repo*) exit 1 ;; esac; printf '{{\"nameWithOwner\":\"%s\",\"defaultBranchRef\":{{\"name\":\"main\"}}}}\\n' \"$repository\" ;;\napi:--method)\n  case \"$6\" in\n    */git/matching-refs/heads/*)\n      ref=${{6##*/heads/}}\n      ref=$(printf '%s' \"$ref\" | sed 's/%2F/\\//g')\n      if [ \"$ref\" = missing ] || {{ [ \"$ref\" = main ] && [ -f '{}' ]; }} || {{ [ \"$ref\" != main ] && [ \"$ref\" != release ] && [ -f '{}' ]; }}; then\n        printf '%s\\n' '[[]]'\n      else\n        oid=$(git rev-parse HEAD)\n        printf '[[{{\"ref\":\"refs/heads/%s\",\"object\":{{\"sha\":\"%s\"}}}}]]\\n' \"$ref\" \"$oid\"\n      fi ;;\n    */pulls) case \"$*\" in *--head*|*'--limit 0'*) exit 1 ;; esac; printf '%s\\n' '[{}]' ;;\n    *) exit 1 ;;\n  esac ;;\npr:create) {}\n*) exit 1 ;;\nesac\n",
                invocations.display(),
                auth_fails.display(),
                base_missing.display(),
                head_missing.display(),
                pull_requests,
                create_response
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

    fn git_object_exists(repo: &std::path::Path, object: &str) -> Result<bool, Box<dyn Error>> {
        Ok(std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["cat-file", "-e", object])
            .status()?
            .success())
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
