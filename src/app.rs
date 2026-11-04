//! Application state, keyboard handling and the event loop. Database work runs
//! on background tasks and reports back as `Msg`s, so the UI never waits.

use std::{
    collections::HashMap,
    io,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::{
    cursor::Show,
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::{
    agent::{
        self,
        mask::{Masker, Visibility},
    },
    complete,
    connect::{Source, Target, discover},
    db::{
        Filter, Link, Relation, RelationKind, Row, Schema, TableInfo, View, ident, looks_like_condition,
        outside_quotes,
        postgres::{ConnectError, Outcome, QueryError, RunError, Session, search_condition, value_condition},
    },
    scratch, sql,
    ui::{
        self,
        ask::AskPrompt,
        connect::{ConnectPicker, PasswordPrompt},
        editor::{self, Editor},
        grid::{self, Grid, PAGE_SIZE},
        input::Input,
        menu::Menu,
        palette::{Action, Command, Palette},
        privacy::PrivacyPicker,
        row::{Field, RowView},
        text,
        tree::Tree,
        value::ValueView,
    },
    update,
    vimkeys::{Motion, VimKeys},
};

/// How long the cursor rests on a table before it is previewed.
const PREVIEW_DELAY: Duration = Duration::from_millis(90);
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_INTERVAL: Duration = Duration::from_millis(80);
const TOAST_DURATION: Duration = Duration::from_millis(2500);
/// Tables up to this size on disk get an exact row count; larger ones keep the estimate.
const COUNT_LIMIT_BYTES: u64 = 256 * 1024 * 1024;
/// How long typing in the filter bar rests before a search runs.
const SEARCH_DELAY: Duration = Duration::from_millis(250);
/// Page queries running longer than this are cancelled when the user moves on.
const CANCEL_AFTER: Duration = Duration::from_millis(250);
/// Tables kept to go back to after following links.
const HISTORY_LIMIT: usize = 50;

pub enum Msg {
    Discovered(Vec<Target>),
    DiscoveryDone,
    Connected {
        id: u64,
        target: Target,
        session: Arc<Session>,
    },
    ConnectFailed {
        id: u64,
        target: Target,
        error: ConnectError,
    },
    ConnectionLost {
        session: u64,
        error: String,
    },
    Catalog {
        session: u64,
        result: Result<Vec<Schema>, String>,
    },
    Databases {
        session: u64,
        names: Vec<String>,
    },
    Described {
        generation: u64,
        result: Result<TableInfo, String>,
    },
    Page {
        generation: u64,
        page: usize,
        result: Result<Vec<Row>, QueryError>,
    },
    Counted {
        generation: u64,
        result: Result<u64, String>,
    },
    VimKeys(Option<VimKeys>),
    Columns {
        session: u64,
        columns: HashMap<(String, String), Vec<(String, String)>>,
    },
    QueryDone {
        session: u64,
        id: u64,
        sql: String,
        result: Result<Outcome, RunError>,
    },
    Agent {
        session: u64,
        id: u64,
        event: agent::Event,
    },
    /// A newer release exists.
    UpdateAvailable(String),
}

/// How the app ended.
pub enum Exit {
    Quit,
    /// Updated: start this program again.
    Relaunch(std::path::PathBuf),
}

/// How long the update notice stays in the corner.
const UPDATE_NOTICE: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Grid,
    Editor,
}

/// Browsing tables, or writing SQL with its result below.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Browse,
    Query,
}

/// Where the editor's last statement stands.
pub enum RunState {
    Idle,
    Running {
        id: u64,
    },
    /// The agent is working on a question.
    Asking {
        id: u64,
        question: String,
        activity: String,
    },
    /// Finished without rows to show, e.g. "3 rows changed".
    Done(String),
    Failed(String),
    /// The statement changes data; running it again allows that once.
    NeedsWrite(String),
}

pub enum Overlay {
    Connect(ConnectPicker),
    Password(PasswordPrompt),
    Ask(AskPrompt),
    Privacy(PrivacyPicker),
    Palette(Palette),
    Value(ValueView),
    Row(RowView),
    Menu(Menu),
    Help,
}

pub enum ToastKind {
    Info,
    Error,
}

pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    until: Instant,
}

pub struct Connecting {
    pub id: u64,
    pub target: Target,
}

pub struct Workspace {
    pub id: u64,
    pub session: Arc<Session>,
    pub target: Target,
    pub tree: Tree,
    /// The table being browsed.
    pub table: Option<Grid>,
    pub focus: Focus,
    pub databases: Vec<String>,
    pub catalog_pending: bool,
    preview_at: Option<Instant>,
    /// Tables left by following links, most recent last.
    history: Vec<Grid>,
    /// When the search being typed in the filter bar runs.
    search_at: Option<Instant>,
    pub mode: Mode,
    pub editor: Option<Editor>,
    /// The editor's last result.
    pub result: Option<Grid>,
    pub run: RunState,
    last_sql: Option<String>,
    /// Every column by (schema, table), for completion.
    pub columns: HashMap<(String, String), Vec<(String, String)>>,
    /// The agent answering the latest question, while it runs.
    agent: Option<tokio::task::JoinHandle<()>>,
    /// The conversation follow-up questions continue.
    conversation: Option<String>,
    /// Stand-ins for masked values, kept for the whole conversation.
    masker: Arc<std::sync::Mutex<Masker>>,
    /// The latest question, and what it sent to the agent.
    asked: u64,
    sent: Vec<(String, String)>,
}

impl Workspace {
    /// The grid being worked with: the table, or in query mode the result.
    pub fn grid(&self) -> Option<&Grid> {
        match self.mode {
            Mode::Browse => self.table.as_ref(),
            Mode::Query => self.result.as_ref(),
        }
    }
}

/// `Workspace::grid` for mutation, borrowing only the two grid fields.
fn active<'a>(mode: Mode, table: &'a mut Option<Grid>, result: &'a mut Option<Grid>) -> Option<&'a mut Grid> {
    match mode {
        Mode::Browse => table.as_mut(),
        Mode::Query => result.as_mut(),
    }
}

pub struct App {
    tx: UnboundedSender<Msg>,
    pub quit: bool,
    pub targets: Vec<Target>,
    pub discovering: bool,
    pub workspace: Option<Workspace>,
    pub overlay: Option<Overlay>,
    pub connecting: Option<Connecting>,
    pub toast: Option<Toast>,
    /// Motions from the user's own vim/neovim config, once loaded.
    pub vim: Option<VimKeys>,
    /// Text to hand to $EDITOR; the event loop does it, as it owns the terminal.
    external_edit: Option<String>,
    next_id: u64,
    pending_g: bool,
    started: Instant,
    /// A newer release, and until when the notice about it shows.
    pub update: Option<(String, Instant)>,
    /// Set to update; the event loop does it, as it owns the terminal.
    update_now: bool,
}

pub async fn run(terminal: &mut DefaultTerminal, initial: Option<Target>) -> Result<Exit> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, initial);
    let mut events = EventStream::new();
    loop {
        terminal.draw(|frame| ui::render(frame, &mut app))?;
        app.request_pages();
        let deadline = app.next_deadline();
        let sleep = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now).into());
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(event)) => app.on_event(event),
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(Exit::Quit),
            },
            Some(msg) = rx.recv() => app.on_msg(msg),
            () = sleep, if deadline.is_some() => app.on_tick(),
        }
        // Apply everything else that is ready so a burst renders once.
        while let Ok(msg) = rx.try_recv() {
            app.on_msg(msg);
        }
        if let Some(text) = app.external_edit.take() {
            // The editor needs the terminal and stdin to itself.
            drop(events);
            let edited = suspended(terminal, || editor_command(&text))?;
            events = EventStream::new();
            app.external_edit_done(edited);
        }
        if std::mem::take(&mut app.update_now) {
            drop(events);
            let method = update::Method::detect();
            if suspended(terminal, || update::run(&method))? {
                app.save_scratch();
                return Ok(Exit::Relaunch(method.program().to_path_buf()));
            }
            events = EventStream::new();
            app.toast(ToastKind::Error, "The update didn't finish");
        }
        if app.quit {
            app.save_scratch();
            return Ok(Exit::Quit);
        }
    }
}

/// Runs `f` with the terminal handed back to the shell, then takes it again.
fn suspended<T>(terminal: &mut DefaultTerminal, f: impl FnOnce() -> T) -> Result<T> {
    let mut stdout = io::stdout();
    execute!(stdout, DisableBracketedPaste, LeaveAlternateScreen, Show)?;
    disable_raw_mode()?;
    let result = f();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, Clear(ClearType::All), EnableBracketedPaste)?;
    // A blank frame brings ratatui's picture of the screen in line with the
    // cleared one, so the next draw repaints everything. (Terminal::clear would
    // ask the terminal for its cursor position, which can time out here.)
    terminal.draw(|_| {})?;
    Ok(result)
}

/// Edits `text` in $VISUAL / $EDITOR (nvim or vi when unset) and returns the result.
fn editor_command(text: &str) -> std::result::Result<String, String> {
    let path = std::env::temp_dir().join(format!("scarlet-{}.sql", std::process::id()));
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    let command = configured_editor().unwrap_or_else(|| {
        let has_nvim = std::process::Command::new("nvim").arg("--version").output().is_ok();
        if has_nvim { "nvim".into() } else { "vi".into() }
    });
    let mut parts: Vec<&str> = command.split_whitespace().collect();
    let program = parts.first().copied().unwrap_or("vi");
    // Windowed editors return at once unless told to wait, and the edit would be lost.
    let name = program.rsplit('/').next().unwrap_or(program);
    let waits = parts.iter().any(|p| matches!(*p, "-w" | "--wait"));
    if !waits && matches!(name, "code" | "cursor" | "windsurf" | "zed" | "subl") {
        parts.push("--wait");
    }
    let status = std::process::Command::new(program).args(&parts[1..]).arg(&path).status();
    let result = match status {
        Ok(s) if s.success() => std::fs::read_to_string(&path).map_err(|e| e.to_string()),
        Ok(s) => Err(format!("{program} exited with {s}")),
        Err(e) => Err(format!("Could not start {program}: {e}")),
    };
    let _ = std::fs::remove_file(&path);
    result
}

enum Nav {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    HalfUp,
    HalfDown,
    Top,
    Bottom,
    FirstColumn,
    LastColumn,
    FocusLeft,
    FocusRight,
}

impl From<Motion> for Nav {
    fn from(motion: Motion) -> Nav {
        match motion {
            Motion::Up => Nav::Up,
            Motion::Down => Nav::Down,
            Motion::Left => Nav::Left,
            Motion::Right => Nav::Right,
            Motion::PageUp => Nav::PageUp,
            Motion::PageDown => Nav::PageDown,
            Motion::HalfUp => Nav::HalfUp,
            Motion::HalfDown => Nav::HalfDown,
            Motion::Top => Nav::Top,
            Motion::Bottom => Nav::Bottom,
            Motion::FirstColumn => Nav::FirstColumn,
            Motion::LastColumn => Nav::LastColumn,
            Motion::FocusLeft => Nav::FocusLeft,
            Motion::FocusRight => Nav::FocusRight,
        }
    }
}

/// Movement keys: the user's own vim mappings first, then arrows and the usual vim keys.
fn nav(vim: Option<&VimKeys>, key: KeyEvent, pending_g: bool) -> Option<Nav> {
    if let Some(motion) = vim.and_then(|v| v.motion(&key)) {
        return Some(motion.into());
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    Some(match key.code {
        KeyCode::Up => Nav::Up,
        KeyCode::Down => Nav::Down,
        KeyCode::Left => Nav::Left,
        KeyCode::Right => Nav::Right,
        KeyCode::PageUp => Nav::PageUp,
        KeyCode::PageDown => Nav::PageDown,
        KeyCode::Home => Nav::Top,
        KeyCode::End => Nav::Bottom,
        KeyCode::Char('u') if ctrl => Nav::HalfUp,
        KeyCode::Char('d') if ctrl => Nav::HalfDown,
        KeyCode::Char('b') if ctrl => Nav::PageUp,
        KeyCode::Char('f') if ctrl => Nav::PageDown,
        _ if ctrl => return None,
        KeyCode::Char('k') => Nav::Up,
        KeyCode::Char('j') => Nav::Down,
        KeyCode::Char('h') => Nav::Left,
        KeyCode::Char('l') => Nav::Right,
        KeyCode::Char('g') if pending_g => Nav::Top,
        KeyCode::Char('G') => Nav::Bottom,
        KeyCode::Char('0' | '^') => Nav::FirstColumn,
        KeyCode::Char('$') => Nav::LastColumn,
        _ => return None,
    })
}

fn is_ctrl(key: KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(c)
}

impl App {
    fn new(tx: UnboundedSender<Msg>, initial: Option<Target>) -> App {
        let mut app = App {
            tx,
            quit: false,
            targets: Vec::new(),
            discovering: true,
            workspace: None,
            overlay: None,
            connecting: None,
            toast: None,
            vim: None,
            external_edit: None,
            next_id: 1,
            pending_g: false,
            started: Instant::now(),
            update: None,
            update_now: false,
        };
        let tx = app.tx.clone();
        tokio::spawn(async move {
            if let Some(version) = update::newer_release().await {
                let _ = tx.send(Msg::UpdateAvailable(version));
            }
        });
        let cwd = std::env::current_dir().unwrap_or_default();
        let dotenv = discover::dotenv_targets(&cwd);
        let env = discover::env_target();
        app.add_targets(initial.iter().chain(&dotenv).chain(&env).cloned().collect());
        discover::spawn(app.tx.clone());
        let tx = app.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(Msg::VimKeys(VimKeys::load().await));
        });

        // Connect straight away when there is one obvious choice.
        let mut seen = std::collections::HashSet::new();
        let mut unique_dotenv: Vec<Target> = dotenv.iter().filter(|t| seen.insert(t.key())).cloned().collect();
        let automatic = initial
            .or_else(|| (unique_dotenv.len() == 1).then(|| unique_dotenv.remove(0)))
            .or(if dotenv.is_empty() { env } else { None });
        match automatic {
            Some(target) => app.connect(target),
            None => app.open_picker(None, None),
        }
        app
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub fn spinner(&self) -> &'static str {
        let tick = self.started.elapsed().as_millis() / SPINNER_INTERVAL.as_millis();
        SPINNER[tick as usize % SPINNER.len()]
    }

    fn animating(&self) -> bool {
        let picker_open = matches!(self.overlay, Some(Overlay::Connect(_)));
        self.connecting.is_some()
            || (self.discovering && picker_open)
            || self.workspace.as_ref().is_some_and(|ws| {
                ws.catalog_pending
                    || !ws.tree.loaded
                    || ws.table.as_ref().is_some_and(Grid::is_busy)
                    || matches!(ws.run, RunState::Running { .. } | RunState::Asking { .. })
            })
    }

    fn next_deadline(&self) -> Option<Instant> {
        let candidates = [
            self.workspace.as_ref().and_then(|ws| ws.preview_at),
            self.workspace.as_ref().and_then(|ws| ws.search_at),
            self.toast.as_ref().map(|t| t.until),
            self.update.as_ref().map(|(_, until)| *until).filter(|until| *until > Instant::now()),
            self.animating().then(|| Instant::now() + SPINNER_INTERVAL),
        ];
        candidates.into_iter().flatten().min()
    }

    fn on_tick(&mut self) {
        let now = Instant::now();
        if self.toast.as_ref().is_some_and(|t| t.until <= now) {
            self.toast = None;
        }
        let Some(ws) = &mut self.workspace else { return };
        if ws.search_at.is_some_and(|at| at <= now) {
            ws.search_at = None;
            let typed = ws.grid().and_then(|g| Some((g.filter_input.as_ref()?.0.text.trim().to_owned(), g)));
            if let Some((filter, grid)) = typed
                && filter != grid.view.filter
            {
                let view = View { filter, ..grid.view.clone() };
                self.apply_view(view);
            }
            return;
        }
        if ws.preview_at.is_some_and(|at| at <= now) {
            ws.preview_at = None;
            if ws.mode == Mode::Query {
                return;
            }
            if let Some(relation) = ws.tree.selected_relation().cloned() {
                self.open_relation(relation);
            }
        }
    }

    fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toast = Some(Toast { text: text.into(), kind, until: Instant::now() + TOAST_DURATION });
    }

    // ---- connections -------------------------------------------------------

    fn add_targets(&mut self, targets: Vec<Target>) {
        let selected = match &self.overlay {
            Some(Overlay::Connect(picker)) => picker.list.selected().map(|i| self.targets[i].key()),
            _ => None,
        };
        for target in targets {
            if !self.targets.iter().any(|t| t.key() == target.key()) {
                self.targets.push(target);
            }
        }
        self.targets.sort_by_key(Target::sort_key);
        if let Some(Overlay::Connect(picker)) = &mut self.overlay {
            let keep = selected.and_then(|key| self.targets.iter().position(|t| t.key() == key));
            picker.refilter(&self.targets, keep);
        }
    }

    /// Shows the connection picker, with `select` (if known) under the cursor.
    fn open_picker(&mut self, error: Option<String>, select: Option<&Target>) {
        let keep = select.and_then(|t| self.targets.iter().position(|other| other.key() == t.key()));
        let mut picker = ConnectPicker::default();
        picker.refilter(&self.targets, keep);
        picker.error = error;
        self.overlay = Some(Overlay::Connect(picker));
    }

    fn connect(&mut self, target: Target) {
        let id = self.next_id();
        self.connecting = Some(Connecting { id, target: target.clone() });
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let lost = tx.clone();
            let on_lost = move |error| {
                let _ = lost.send(Msg::ConnectionLost { session: id, error });
            };
            let msg = match Session::connect(&target, on_lost).await {
                Ok(session) => Msg::Connected { id, target, session: Arc::new(session) },
                Err(error) => Msg::ConnectFailed { id, target, error },
            };
            let _ = tx.send(msg);
        });
    }

    fn load_catalog(&mut self) {
        let Some(ws) = &mut self.workspace else { return };
        ws.catalog_pending = true;
        let (session, id, tx) = (ws.session.clone(), ws.id, self.tx.clone());
        tokio::spawn(async move {
            let result = session.catalog().await;
            let _ = tx.send(Msg::Catalog { session: id, result });
        });
    }

    fn load_databases(&mut self) {
        let Some(ws) = &self.workspace else { return };
        let (session, id, tx) = (ws.session.clone(), ws.id, self.tx.clone());
        tokio::spawn(async move {
            if let Ok(names) = session.databases().await {
                let _ = tx.send(Msg::Databases { session: id, names });
            }
        });
    }

    // ---- tables -------------------------------------------------------------

    /// Opens a table from the sidebar or palette: a fresh start, no history.
    fn open_relation(&mut self, relation: Relation) {
        let generation = self.next_id();
        let Some(ws) = &mut self.workspace else { return };
        ws.preview_at = None;
        if ws.table.as_ref().is_some_and(|g| g.relation.same(&relation) && ws.history.is_empty()) {
            return;
        }
        Self::cancel_slow_queries(ws);
        ws.history.clear();
        ws.table = Some(Grid::new(relation, generation, View::default()));
        self.describe();
    }

    /// Opens `relation` showing `view`, remembering the current table to come back to.
    fn open_linked(&mut self, relation: Relation, view: View) {
        let generation = self.next_id();
        let Some(ws) = &mut self.workspace else { return };
        Self::cancel_slow_queries(ws);
        let came_from = ws.table.as_ref().map(|g| g.relation.name.clone());
        if let Some(mut current) = ws.table.take() {
            current.suspend();
            ws.history.push(current);
            if ws.history.len() > HISTORY_LIMIT {
                ws.history.remove(0);
            }
        }
        ws.tree.reveal(&relation);
        let mut grid = Grid::new(relation, generation, view);
        grid.came_from = came_from;
        ws.table = Some(grid);
        ws.focus = Focus::Grid;
        self.describe();
    }

    /// Returns to the table before the last followed link. False when there is none.
    fn go_back(&mut self) -> bool {
        let Some(ws) = &mut self.workspace else { return false };
        let Some(previous) = ws.history.pop() else { return false };
        Self::cancel_slow_queries(ws);
        ws.tree.reveal(&previous.relation);
        let needs_describe = matches!(previous.status, grid::Status::Describing);
        ws.table = Some(previous);
        if needs_describe {
            self.describe();
        }
        true
    }

    /// Shows different rows of the current table.
    fn apply_view(&mut self, view: View) {
        let generation = self.next_id();
        let Some(ws) = &mut self.workspace else { return };
        Self::cancel_slow_queries(ws);
        let Some(grid) = active(ws.mode, &mut ws.table, &mut ws.result) else { return };
        grid.set_view(view, generation);
        if grid.columns.is_empty() {
            self.describe();
        } else if grid.wants_count(COUNT_LIMIT_BYTES) {
            self.count_rows();
        }
    }

    /// A query that has been running a while would hold up the next one on the
    /// same connection, so it is cancelled. Quick ones are left to finish.
    fn cancel_slow_queries(ws: &Workspace) {
        let slow = ws.table.as_ref().and_then(|g| g.busy_since).is_some_and(|since| since.elapsed() > CANCEL_AFTER);
        if slow {
            let session = ws.session.clone();
            tokio::spawn(async move { session.cancel().await });
        }
    }

    fn reload_relation(&mut self) {
        let generation = self.next_id();
        let Some(ws) = &mut self.workspace else { return };
        Self::cancel_slow_queries(ws);
        let Some(grid) = &mut ws.table else { return };
        grid.reload(generation);
        self.describe();
    }

    fn describe(&self) {
        let Some(ws) = &self.workspace else { return };
        let Some(grid) = &ws.table else { return };
        let (session, relation, generation, tx) =
            (ws.session.clone(), grid.relation.clone(), grid.generation, self.tx.clone());
        tokio::spawn(async move {
            let result = session.describe(&relation).await;
            let _ = tx.send(Msg::Described { generation, result });
        });
    }

    fn request_pages(&mut self) {
        let Some(ws) = &mut self.workspace else { return };
        let Some(grid) = &mut ws.table else { return };
        for page in grid.wanted_pages() {
            let (session, tx) = (ws.session.clone(), self.tx.clone());
            let (relation, alias, view, order_by, generation) =
                (grid.relation.clone(), grid.alias.clone(), grid.view.clone(), grid.order_by.clone(), grid.generation);
            tokio::spawn(async move {
                let result = session.page(&relation, &alias, &view, &order_by, page * PAGE_SIZE, PAGE_SIZE).await;
                let _ = tx.send(Msg::Page { generation, page, result });
            });
        }
    }

    fn current_grid(&mut self, generation: u64) -> Option<&mut Grid> {
        self.workspace.as_mut()?.table.as_mut().filter(|g| g.generation == generation)
    }

    // ---- messages -----------------------------------------------------------

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Discovered(targets) => self.add_targets(targets),
            Msg::DiscoveryDone => self.discovering = false,
            Msg::Connected { id, target, session } => {
                if self.connecting.as_ref().is_none_or(|c| c.id != id) {
                    return;
                }
                self.connecting = None;
                // The workspace is about to be replaced; keep its SQL.
                self.save_scratch();
                if matches!(self.overlay, Some(Overlay::Connect(_) | Overlay::Password(_))) {
                    self.overlay = None;
                }
                // Pasted connections stay in the list for the rest of the session.
                self.add_targets(vec![target.clone()]);
                self.workspace = Some(Workspace {
                    id,
                    session,
                    target,
                    tree: Tree::default(),
                    table: None,
                    focus: Focus::Tree,
                    databases: Vec::new(),
                    catalog_pending: false,
                    preview_at: None,
                    history: Vec::new(),
                    search_at: None,
                    mode: Mode::Browse,
                    editor: None,
                    result: None,
                    run: RunState::Idle,
                    last_sql: None,
                    columns: HashMap::new(),
                    agent: None,
                    conversation: None,
                    masker: Arc::default(),
                    asked: 0,
                    sent: Vec::new(),
                });
                self.load_catalog();
                self.load_databases();
            }
            Msg::ConnectFailed { id, target, error } => {
                if self.connecting.as_ref().is_none_or(|c| c.id != id) {
                    return;
                }
                self.connecting = None;
                if error.needs_password {
                    // A wrong password deserves a message; a missing one just needs asking.
                    let message = target.has_password().then(|| error.message.clone());
                    self.overlay =
                        Some(Overlay::Password(PasswordPrompt { target, input: Input::default(), error: message }));
                    return;
                }
                match &mut self.overlay {
                    Some(Overlay::Connect(picker)) => picker.error = Some(error.message),
                    _ if self.workspace.is_some() => self.toast(ToastKind::Error, error.message),
                    _ => self.open_picker(Some(error.message), Some(&target)),
                }
            }
            Msg::ConnectionLost { session, error } => {
                if self.workspace.as_ref().is_some_and(|ws| ws.id == session) {
                    self.save_scratch();
                }
                if let Some(ws) = self.workspace.take_if(|ws| ws.id == session) {
                    self.open_picker(Some(format!("Connection lost: {error}")), Some(&ws.target));
                }
            }
            Msg::Catalog { session, result } => {
                let Some(ws) = self.workspace.as_mut().filter(|ws| ws.id == session) else { return };
                ws.catalog_pending = false;
                match result {
                    Ok(schemas) => {
                        ws.tree.set_catalog(schemas);
                        let (session, id, tx) = (ws.session.clone(), ws.id, self.tx.clone());
                        tokio::spawn(async move {
                            if let Ok(columns) = session.all_columns().await {
                                let _ = tx.send(Msg::Columns { session: id, columns });
                            }
                        });
                        if ws.table.is_none()
                            && let Some(relation) = ws.tree.selected_relation().cloned()
                        {
                            self.open_relation(relation);
                        }
                    }
                    Err(error) => {
                        ws.tree.loaded = true;
                        ws.tree.error = Some(error);
                    }
                }
            }
            Msg::Databases { session, names } => {
                if let Some(ws) = self.workspace.as_mut().filter(|ws| ws.id == session) {
                    ws.databases = names;
                }
            }
            Msg::Described { generation, result } => {
                let Some(grid) = self.current_grid(generation) else { return };
                match result {
                    Ok(info) => {
                        grid.described(info);
                        if grid.wants_count(COUNT_LIMIT_BYTES) {
                            self.count_rows();
                        }
                    }
                    Err(error) => grid.failed(error),
                }
            }
            Msg::Page { generation, page, result } => {
                let Some(grid) = self.current_grid(generation) else { return };
                match result {
                    Ok(rows) => grid.page_loaded(page, rows),
                    // Our own cancel can land on the next query; just ask again.
                    Err(QueryError { canceled: true, .. }) => grid.retry(page),
                    Err(error) => grid.failed(error.message),
                }
            }
            Msg::VimKeys(keys) => self.vim = keys,
            Msg::Columns { session, columns } => {
                if let Some(ws) = self.workspace.as_mut().filter(|ws| ws.id == session) {
                    ws.columns = columns;
                }
            }
            Msg::QueryDone { session, id, sql, result } => self.query_done(session, id, sql, result),
            Msg::Agent { session, id, event } => self.agent_event(session, id, event),
            Msg::UpdateAvailable(version) => self.update = Some((version, Instant::now() + UPDATE_NOTICE)),
            Msg::Counted { generation, result } => {
                // A failed count (timeout) just leaves the estimate in place.
                if let (Some(grid), Ok(n)) = (self.current_grid(generation), result) {
                    grid.set_count(n);
                }
            }
        }
    }

    fn count_rows(&self) {
        let Some(ws) = &self.workspace else { return };
        let Some(grid) = &ws.table else { return };
        let (session, tx) = (ws.session.clone(), self.tx.clone());
        let (relation, alias, view, generation) =
            (grid.relation.clone(), grid.alias.clone(), grid.view.clone(), grid.generation);
        tokio::spawn(async move {
            let result = session.count(&relation, &alias, &view).await;
            let _ = tx.send(Msg::Counted { generation, result });
        });
    }

    // ---- input --------------------------------------------------------------

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Paste(text) => self.on_paste(&text),
            _ => {}
        }
    }

    fn on_paste(&mut self, text: &str) {
        // SQL keeps its layout; single-line fields get the text trimmed.
        if self.overlay.is_none()
            && let Some(ws) = &mut self.workspace
            && ws.focus == Focus::Editor
            && let Some(editor) = &mut ws.editor
        {
            editor.popup = None;
            editor.insert_str(text);
            return;
        }
        let text = text.trim();
        match &mut self.overlay {
            Some(Overlay::Connect(picker)) => {
                picker.list.input.insert_str(text);
                picker.error = None;
                picker.refilter(&self.targets, None);
            }
            Some(Overlay::Password(prompt)) => prompt.input.insert_str(text),
            Some(Overlay::Ask(prompt)) => prompt.input.insert_str(text),
            Some(Overlay::Privacy(_)) => {}
            Some(Overlay::Palette(palette)) => {
                palette.list.input.insert_str(text);
                palette.refilter();
            }
            Some(_) => {}
            None => {
                if let Some(ws) = &mut self.workspace
                    && let Some(grid) = active(ws.mode, &mut ws.table, &mut ws.result)
                    && let Some((input, _)) = &mut grid.filter_input
                {
                    input.insert_str(text);
                    ws.search_at = (!looks_like_condition(&input.text)).then(|| Instant::now() + SEARCH_DELAY);
                } else if let Some(ws) = &mut self.workspace
                    && ws.tree.filtering
                {
                    ws.tree.filter.insert_str(text);
                    ws.tree.filter_changed();
                    ws.preview_at = Some(Instant::now() + PREVIEW_DELAY);
                }
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if is_ctrl(key, 'c') {
            self.quit = true;
            return;
        }
        let pending_g = std::mem::take(&mut self.pending_g);
        if let Some(overlay) = self.overlay.take() {
            let keep = self.overlay_key(overlay, key, pending_g);
            if self.overlay.is_none() {
                self.overlay = keep;
            }
            return;
        }
        let Some(ws) = &mut self.workspace else {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                self.quit = true;
            }
            return;
        };
        if is_ctrl(key, 'e') {
            self.toggle_query();
            return;
        }
        if key.code == KeyCode::Esc && matches!(ws.run, RunState::Asking { .. }) {
            self.stop_asking();
            return;
        }
        if ws.focus == Focus::Editor {
            self.editor_key(key);
            return;
        }
        if ws.tree.filtering && ws.focus == Focus::Tree {
            self.filter_key(key);
            return;
        }
        if ws.focus == Focus::Grid && ws.grid().is_some_and(|g| g.filter_input.is_some()) {
            self.grid_filter_key(key);
            return;
        }
        if self.is_leader(key) {
            self.open_menu();
            return;
        }
        let Some(ws) = &mut self.workspace else { return };
        match key.code {
            _ if is_ctrl(key, 'k') => self.open_palette(),
            KeyCode::Char('?') => self.overlay = Some(Overlay::Help),
            KeyCode::Char('a') => self.open_ask(),
            KeyCode::Char('w') if !ws.sent.is_empty() => self.show_sent(),
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Tab | KeyCode::BackTab => {
                // Tables, then the editor in query mode, then the data.
                let has_grid = ws.grid().is_some();
                ws.focus = match (ws.mode, ws.focus) {
                    (Mode::Query, Focus::Tree) => Focus::Editor,
                    (Mode::Query, Focus::Editor) if has_grid => Focus::Grid,
                    (Mode::Browse, Focus::Tree) if has_grid => Focus::Grid,
                    _ => Focus::Tree,
                };
            }
            KeyCode::Char('g') if !pending_g && key.modifiers.is_empty() => self.pending_g = true,
            _ => match ws.focus {
                Focus::Tree => self.tree_key(key, pending_g),
                Focus::Grid => self.grid_key(key, pending_g),
                Focus::Editor => {}
            },
        }
    }

    fn overlay_key(&mut self, overlay: Overlay, key: KeyEvent, pending_g: bool) -> Option<Overlay> {
        match overlay {
            Overlay::Help => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | '?') => None,
                _ => Some(Overlay::Help),
            },
            Overlay::Menu(_) => {
                // The menu only shows the way; the key itself does the thing.
                if key.code != KeyCode::Esc && !self.is_leader(key) {
                    self.on_key(key);
                }
                None
            }
            Overlay::Row(mut view) => {
                match (key.code, nav(self.vim.as_ref(), key, pending_g)) {
                    (KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | 'v'), _) => return None,
                    (KeyCode::Char('Y' | 'c' | 'y'), _) => {
                        if let Some(json) = view.json.clone() {
                            self.copy(&json, "the row as JSON");
                        }
                    }
                    (KeyCode::Char('g'), None) if key.modifiers.is_empty() => self.pending_g = true,
                    (_, Some(Nav::Up)) => view.scroll_by(-1),
                    (_, Some(Nav::Down)) => view.scroll_by(1),
                    (_, Some(Nav::PageUp)) => view.page(-1, false),
                    (_, Some(Nav::PageDown)) => view.page(1, false),
                    (_, Some(Nav::HalfUp)) => view.page(-1, true),
                    (_, Some(Nav::HalfDown)) => view.page(1, true),
                    (_, Some(Nav::Top)) => view.top(),
                    (_, Some(Nav::Bottom)) => view.bottom(),
                    _ => {}
                }
                Some(Overlay::Row(view))
            }
            Overlay::Value(mut view) => {
                match (key.code, nav(self.vim.as_ref(), key, pending_g)) {
                    (KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q'), _) => return None,
                    (KeyCode::Char('c' | 'y'), _) => {
                        if let Some(value) = &view.value {
                            self.copy(value, &view.column);
                        }
                    }
                    (KeyCode::Char('g'), None) if key.modifiers.is_empty() => self.pending_g = true,
                    (_, Some(Nav::Up)) => view.scroll_by(-1),
                    (_, Some(Nav::Down)) => view.scroll_by(1),
                    (_, Some(Nav::PageUp)) => view.page(-1, false),
                    (_, Some(Nav::PageDown)) => view.page(1, false),
                    (_, Some(Nav::HalfUp)) => view.page(-1, true),
                    (_, Some(Nav::HalfDown)) => view.page(1, true),
                    (_, Some(Nav::Top)) => view.top(),
                    (_, Some(Nav::Bottom)) => view.bottom(),
                    _ => {}
                }
                Some(Overlay::Value(view))
            }
            Overlay::Palette(mut palette) => {
                match key.code {
                    KeyCode::Esc => return None,
                    _ if is_ctrl(key, 'k') => return None,
                    KeyCode::Enter => {
                        if let Some(action) = palette.take_selected() {
                            self.run_action(action);
                        }
                        return None;
                    }
                    KeyCode::Up => palette.list.move_by(-1),
                    KeyCode::Down => palette.list.move_by(1),
                    _ if is_ctrl(key, 'p') => palette.list.move_by(-1),
                    _ if is_ctrl(key, 'n') => palette.list.move_by(1),
                    KeyCode::PageUp => palette.list.page(-1),
                    KeyCode::PageDown => palette.list.page(1),
                    _ => {
                        if palette.list.input.handle(key) {
                            palette.refilter();
                        }
                    }
                }
                Some(Overlay::Palette(palette))
            }
            Overlay::Connect(mut picker) => {
                match key.code {
                    KeyCode::Esc if self.workspace.is_some() => {
                        // Closing the picker abandons a connection still in progress.
                        self.connecting = None;
                        return None;
                    }
                    KeyCode::Esc => {
                        self.quit = true;
                        return None;
                    }
                    KeyCode::Enter => {
                        picker.error = None;
                        if let Some(url) = picker.url() {
                            match Target::parse(url, Source::Pasted) {
                                Ok(target) => self.connect(target),
                                Err(error) => picker.error = Some(error),
                            }
                        } else if let Some(i) = picker.list.selected() {
                            self.connect(self.targets[i].clone());
                        }
                    }
                    KeyCode::Up => picker.list.move_by(-1),
                    KeyCode::Down => picker.list.move_by(1),
                    _ if is_ctrl(key, 'p') => picker.list.move_by(-1),
                    _ if is_ctrl(key, 'n') => picker.list.move_by(1),
                    KeyCode::PageUp => picker.list.page(-1),
                    KeyCode::PageDown => picker.list.page(1),
                    _ => {
                        if picker.list.input.handle(key) {
                            picker.error = None;
                            picker.refilter(&self.targets, None);
                        }
                    }
                }
                Some(Overlay::Connect(picker))
            }
            Overlay::Ask(mut prompt) => {
                match key.code {
                    KeyCode::Esc => return None,
                    KeyCode::Enter => {
                        let question = prompt.input.text.trim().to_owned();
                        if !question.is_empty() {
                            self.ask(question);
                            return None;
                        }
                    }
                    _ if is_ctrl(key, 'n') && prompt.follow_up => {
                        if let Some(ws) = &mut self.workspace {
                            ws.conversation = None;
                            ws.masker = Arc::default();
                        }
                        prompt.follow_up = false;
                    }
                    _ if is_ctrl(key, 'p') => {
                        let (database, current) = (prompt.database.clone(), prompt.visibility);
                        return Some(Overlay::Privacy(PrivacyPicker::new(&database, Some(current), Some(prompt))));
                    }
                    _ => {
                        prompt.input.handle(key);
                    }
                }
                Some(Overlay::Ask(prompt))
            }
            Overlay::Privacy(mut picker) => {
                match (key.code, nav(self.vim.as_ref(), key, pending_g)) {
                    (KeyCode::Esc, _) => return picker.then.filter(|_| picker.had_choice).map(Overlay::Ask),
                    (KeyCode::Enter, _) => {
                        let choice = picker.choice();
                        self.set_visibility(choice);
                        return picker.then.map(|mut prompt| {
                            prompt.visibility = choice;
                            prompt.follow_up = self.workspace.as_ref().is_some_and(|ws| ws.conversation.is_some());
                            Overlay::Ask(prompt)
                        });
                    }
                    (_, Some(Nav::Up)) => picker.move_by(-1),
                    (_, Some(Nav::Down)) => picker.move_by(1),
                    _ if is_ctrl(key, 'p') => picker.move_by(-1),
                    _ if is_ctrl(key, 'n') => picker.move_by(1),
                    _ => {}
                }
                Some(Overlay::Privacy(picker))
            }
            Overlay::Password(mut prompt) => {
                match key.code {
                    KeyCode::Esc => {
                        self.connecting = None;
                        if self.workspace.is_none() {
                            self.open_picker(None, Some(&prompt.target));
                        }
                        return None;
                    }
                    KeyCode::Enter if self.connecting.is_none() => {
                        prompt.error = None;
                        self.connect(prompt.target.with_password(&prompt.input.text));
                    }
                    _ if self.connecting.is_none() => {
                        prompt.input.handle(key);
                    }
                    _ => {}
                }
                Some(Overlay::Password(prompt))
            }
        }
    }

    fn filter_key(&mut self, key: KeyEvent) {
        let Some(ws) = &mut self.workspace else { return };
        let tree = &mut ws.tree;
        match key.code {
            KeyCode::Esc => tree.clear_filter(),
            KeyCode::Enter | KeyCode::Tab => {
                tree.filtering = false;
                if let Some(relation) = tree.selected_relation().cloned() {
                    ws.focus = Focus::Grid;
                    self.open_relation(relation);
                }
                return;
            }
            KeyCode::Up => tree.move_by(-1),
            KeyCode::Down => tree.move_by(1),
            _ if is_ctrl(key, 'p') => tree.move_by(-1),
            _ if is_ctrl(key, 'n') => tree.move_by(1),
            _ => {
                if tree.filter.handle(key) {
                    tree.filter_changed();
                }
            }
        }
        ws.preview_at = Some(Instant::now() + PREVIEW_DELAY);
    }

    fn tree_key(&mut self, key: KeyEvent, pending_g: bool) {
        let Some(ws) = &mut self.workspace else { return };
        let tree = &mut ws.tree;
        match (key.code, nav(self.vim.as_ref(), key, pending_g)) {
            (_, Some(Nav::FocusRight)) => {
                if ws.grid().is_some() {
                    ws.focus = Focus::Grid;
                }
                return;
            }
            (KeyCode::Enter, _) | (_, Some(Nav::Right)) => {
                if let Some(relation) = tree.selected_relation().cloned() {
                    ws.focus = Focus::Grid;
                    ws.mode = Mode::Browse;
                    self.open_relation(relation);
                    return;
                }
                if key.code == KeyCode::Enter { tree.toggle() } else { tree.expand() }
            }
            (_, Some(Nav::Left)) => tree.collapse(),
            (_, Some(Nav::Up)) => tree.move_by(-1),
            (_, Some(Nav::Down)) => tree.move_by(1),
            (_, Some(Nav::PageUp)) => tree.page(-1, false),
            (_, Some(Nav::PageDown)) => tree.page(1, false),
            (_, Some(Nav::HalfUp)) => tree.page(-1, true),
            (_, Some(Nav::HalfDown)) => tree.page(1, true),
            (_, Some(Nav::Top)) => tree.top(),
            (_, Some(Nav::Bottom)) => tree.bottom(),
            (KeyCode::Char('/'), _) => tree.filtering = true,
            (KeyCode::Esc, _) if !tree.filter.is_empty() => tree.clear_filter(),
            (KeyCode::Char('r'), _) => {
                self.load_catalog();
                return;
            }
            _ => return,
        }
        ws.preview_at = Some(Instant::now() + PREVIEW_DELAY);
    }

    fn grid_key(&mut self, key: KeyEvent, pending_g: bool) {
        let Some(ws) = &mut self.workspace else { return };
        let Some(grid) = active(ws.mode, &mut ws.table, &mut ws.result) else {
            ws.focus = Focus::Tree;
            return;
        };
        match (key.code, nav(self.vim.as_ref(), key, pending_g)) {
            (KeyCode::Esc, _) => self.grid_escape(),
            (KeyCode::Backspace, _) => {
                self.go_back();
            }
            _ if is_ctrl(key, 'o') => {
                self.go_back();
            }
            (_, Some(Nav::FocusLeft)) => ws.focus = Focus::Tree,
            (_, Some(Nav::Left)) if grid.col == 0 => ws.focus = Focus::Tree,
            (_, Some(Nav::Left)) => grid.move_by(0, -1),
            (_, Some(Nav::Right)) => grid.move_by(0, 1),
            (_, Some(Nav::Up)) => grid.move_by(-1, 0),
            (_, Some(Nav::Down)) => grid.move_by(1, 0),
            (_, Some(Nav::PageUp)) => grid.page(-1, false),
            (_, Some(Nav::PageDown)) => grid.page(1, false),
            (_, Some(Nav::HalfUp)) => grid.page(-1, true),
            (_, Some(Nav::HalfDown)) => grid.page(1, true),
            (_, Some(Nav::Top)) => grid.top(),
            (_, Some(Nav::Bottom)) => grid.bottom(),
            (_, Some(Nav::FirstColumn)) => grid.first_column(),
            (_, Some(Nav::LastColumn)) => grid.last_column(),
            (KeyCode::Enter, _) => self.grid_enter(),
            (KeyCode::Char('s'), _) => {
                if let Some(view) = grid.next_sort() {
                    self.apply_view(view);
                }
            }
            (KeyCode::Char('/'), _) => {
                let mut input = Input::default();
                input.set(&grid.view.filter);
                grid.filter_input = Some((input, grid.view.filter.clone()));
            }
            (KeyCode::Char('='), _) => self.filter_by_value(false),
            (KeyCode::Char('!'), _) => self.filter_by_value(true),
            (KeyCode::Char('v'), _) => self.open_row_view(),
            (KeyCode::Char('Y'), _) => {
                if let Some(json) = grid.row_json() {
                    self.copy(&json, "the row as JSON");
                }
            }
            (KeyCode::Char('c' | 'y'), _) => match grid.current() {
                Some((column, Some(value))) => {
                    let (text, name) = (grid::display(value, column.kind).into_owned(), column.name.clone());
                    self.copy(&text, &name);
                }
                Some((_, None)) => self.toast(ToastKind::Info, "Nothing to copy, the value is null"),
                None => {}
            },
            (KeyCode::Char('r'), _) if ws.mode == Mode::Query => self.rerun(),
            (KeyCode::Char('r'), _) => self.reload_relation(),
            _ => {}
        }
    }

    /// Esc peels one layer: back along followed links, then the filter, then to the tables.
    fn grid_escape(&mut self) {
        if self.workspace.as_ref().is_some_and(|ws| ws.mode == Mode::Browse) && self.go_back() {
            return;
        }
        let Some(ws) = &mut self.workspace else { return };
        let Some(grid) = ws.grid() else { return };
        if grid.view.filter.is_empty() {
            ws.focus = if ws.mode == Mode::Query { Focus::Editor } else { Focus::Tree };
        } else {
            let view = View { filter: String::new(), ..grid.view.clone() };
            self.apply_view(view);
        }
    }

    fn grid_filter_key(&mut self, key: KeyEvent) {
        let Some(ws) = &mut self.workspace else { return };
        let Some(grid) = active(ws.mode, &mut ws.table, &mut ws.result) else { return };
        let Some((input, original)) = &mut grid.filter_input else { return };
        match key.code {
            KeyCode::Esc => {
                let original = std::mem::take(original);
                grid.filter_input = None;
                ws.search_at = None;
                if grid.view.filter != original {
                    let view = View { filter: original, ..grid.view.clone() };
                    self.apply_view(view);
                }
            }
            KeyCode::Enter => {
                let filter = input.text.trim().to_owned();
                if grid.is_local() && looks_like_condition(&filter) {
                    let message = "In results, / searches text. Use a WHERE in the query";
                    return self.toast(ToastKind::Info, message);
                }
                grid.filter_input = None;
                ws.search_at = None;
                if grid.view.filter != filter {
                    let view = View { filter, ..grid.view.clone() };
                    self.apply_view(view);
                }
            }
            _ => {
                if input.handle(key) {
                    // Searches run as you type; SQL conditions wait for enter.
                    ws.search_at = (!looks_like_condition(&input.text)).then(|| Instant::now() + SEARCH_DELAY);
                }
            }
        }
    }

    /// Enter on a cell: follow its foreign key, show the rows that point at it,
    /// or look at the value.
    fn grid_enter(&mut self) {
        let Some(grid) = self.workspace.as_ref().and_then(Workspace::grid) else { return };
        if let Some(link) = grid.link_out().cloned() {
            self.follow(&link);
            return;
        }
        let incoming: Vec<Link> = grid.links_in().into_iter().cloned().collect();
        if !incoming.is_empty() {
            self.show_references(incoming);
            return;
        }
        if let Some((column, value)) = grid.current() {
            let value = value.map(|v| grid::full_value(v, column.kind));
            self.overlay = Some(Overlay::Value(ValueView::new(column.name.clone(), column.type_name.clone(), value)));
        }
    }

    /// A condition selecting rows whose `columns_there` equal this row's
    /// `columns_here`. `Err(Some(..))` carries a message for the user.
    fn link_condition(&self, columns_here: &[String], columns_there: &[String]) -> Result<String, Option<String>> {
        let grid = self.workspace.as_ref().and_then(Workspace::grid).ok_or(None)?;
        let mut conditions = Vec::new();
        for (here, there) in columns_here.iter().zip(columns_there) {
            let value = grid.value_of(here).ok_or(None)?;
            let Some(value) = value else {
                return Err(Some(format!("{here} is null, so there is nothing to follow")));
            };
            let column = grid.column(here).ok_or(None)?;
            conditions.push(value_condition(there, column, Some(value), false));
        }
        Ok(conditions.join(" AND "))
    }

    fn follow(&mut self, link: &Link) {
        match self.link_condition(&link.columns, &link.other_columns) {
            Ok(filter) => {
                let relation = self.find_relation(&link.schema, &link.table);
                self.open_linked(relation, View { filter, sort: None });
            }
            Err(Some(message)) => self.toast(ToastKind::Info, message),
            Err(None) => {}
        }
    }

    /// The rows elsewhere that point at the current cell: straight there when
    /// one table does, a choice when several do.
    fn show_references(&mut self, links: Vec<Link>) {
        let mut choices = Vec::new();
        for link in &links {
            match self.link_condition(&link.columns, &link.other_columns) {
                Ok(filter) => {
                    let relation = self.find_relation(&link.schema, &link.table);
                    choices.push((link, relation, View { filter, sort: None }));
                }
                Err(Some(message)) => return self.toast(ToastKind::Info, message),
                Err(None) => return,
            }
        }
        if choices.len() == 1 {
            let (_, relation, view) = choices.remove(0);
            return self.open_linked(relation, view);
        }
        let commands = choices
            .into_iter()
            .map(|(link, relation, view)| Command {
                label: link.table.clone(),
                tag: format!("by {}", link.other_columns.join(", ")),
                action: Action::Follow(relation, view),
            })
            .collect();
        self.overlay = Some(Overlay::Palette(Palette::with_placeholder(commands, "Rows that point here")));
    }

    fn find_relation(&self, schema: &str, name: &str) -> Relation {
        self.workspace
            .as_ref()
            .and_then(|ws| ws.tree.schemas.iter().find(|s| s.name == schema))
            .and_then(|s| s.relations.iter().find(|r| r.name == name))
            .cloned()
            .unwrap_or_else(|| Relation {
                schema: schema.to_owned(),
                name: name.to_owned(),
                kind: RelationKind::Table,
                estimate: None,
            })
    }

    /// Narrows the rows to those with (or without) the current cell's value.
    fn filter_by_value(&mut self, exclude: bool) {
        let Some(grid) = self.workspace.as_ref().and_then(Workspace::grid) else { return };
        if grid.is_local() {
            let message = "In results, filter with a WHERE in the query";
            return self.toast(ToastKind::Info, message);
        }
        let Some((column, value)) = grid.current() else { return };
        let condition = value_condition(&column.name, column, value, exclude);
        let filter = and_filter(&grid.view, &grid.alias, &condition);
        let view = View { filter, ..grid.view.clone() };
        self.apply_view(view);
    }

    fn open_row_view(&mut self) {
        let Some(grid) = self.workspace.as_ref().and_then(Workspace::grid) else { return };
        let Some(row) = grid.current_row() else { return };
        let fields = row
            .into_iter()
            .map(|(column, value)| Field {
                name: column.name.clone(),
                value: value.map(|v| grid::full_value(v, column.kind)),
            })
            .collect();
        let title = format!("{} · row {}", grid.relation.name, text::thousands(grid.row as u64 + 1));
        self.overlay = Some(Overlay::Row(RowView::new(title, fields, grid.row_json())));
    }

    fn is_leader(&self, key: KeyEvent) -> bool {
        let KeyCode::Char(c) = key.code else { return false };
        let leader = self.vim.as_ref().and_then(|v| v.leader.as_deref()).and_then(|l| {
            let mut chars = l.chars();
            chars.next().filter(|_| chars.next().is_none())
        });
        !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) && (c == ' ' || Some(c) == leader)
    }

    fn leader_label(&self) -> String {
        match self.vim.as_ref().and_then(|v| v.leader.as_deref()) {
            Some(" ") | None => "space".into(),
            Some(other) => other.into(),
        }
    }

    fn open_menu(&mut self) {
        let Some(ws) = &self.workspace else { return };
        let mut items: Vec<(String, String)> = match (ws.focus, ws.grid()) {
            (Focus::Grid, Some(grid)) => vec![
                ("enter".into(), self.enter_label(grid)),
                ("s".into(), "sort by this column".into()),
                ("/".into(), "filter rows".into()),
                ("=".into(), "only rows with this value".into()),
                ("!".into(), "rows without this value".into()),
                ("v".into(), "the whole row".into()),
                ("c".into(), "copy the value".into()),
                ("Y".into(), "copy the row as JSON".into()),
                ("r".into(), "reload".into()),
                ("esc".into(), self.escape_label(ws, grid).into()),
            ],
            _ => vec![
                ("enter".into(), "open".into()),
                ("/".into(), "filter tables".into()),
                ("r".into(), "reload the list".into()),
            ],
        };
        items.push(("a".into(), "ask about the data".into()));
        if !ws.sent.is_empty() {
            items.push(("w".into(), "what was sent to Claude".into()));
        }
        items.push(("ctrl+k".into(), "jump anywhere".into()));
        items.push(("?".into(), "every key".into()));
        let title = match (ws.focus, ws.grid()) {
            (Focus::Grid, Some(grid)) => grid.columns.get(grid.col).map_or_else(String::new, |c| c.name.clone()),
            _ => ws.target.database().to_owned(),
        };
        self.overlay = Some(Overlay::Menu(Menu { title, items }));
    }

    fn enter_label(&self, grid: &Grid) -> String {
        if let Some(link) = grid.link_out() {
            return format!("→ {}", link.table);
        }
        let incoming = grid.links_in();
        match incoming.len() {
            0 => "view".into(),
            1 => format!("← {}", incoming[0].table),
            n => format!("← {n} tables"),
        }
    }

    fn escape_label(&self, ws: &Workspace, grid: &Grid) -> &'static str {
        if ws.mode == Mode::Query {
            if grid.view.filter.is_empty() { "editor" } else { "clear filter" }
        } else if !ws.history.is_empty() {
            "back"
        } else if !grid.view.filter.is_empty() {
            "clear filter"
        } else {
            "tables"
        }
    }

    // ---- SQL editor ---------------------------------------------------------

    /// Switches between browsing and the SQL editor. An empty editor starts
    /// with the table being browsed, as SQL, filter and sort included.
    fn toggle_query(&mut self) {
        let Some(ws) = &mut self.workspace else { return };
        match ws.mode {
            Mode::Browse => {
                let target = &ws.target;
                let editor = ws.editor.get_or_insert_with(|| Editor::new(&scratch::load(target)));
                if editor.is_blank()
                    && let Some(table) = &ws.table
                {
                    editor.set_text(&starter_query(table));
                }
                ws.mode = Mode::Query;
                ws.focus = Focus::Editor;
            }
            Mode::Query => {
                self.save_scratch();
                let Some(ws) = &mut self.workspace else { return };
                ws.mode = Mode::Browse;
                ws.focus = if ws.table.is_some() { Focus::Grid } else { Focus::Tree };
            }
        }
    }

    fn editor_key(&mut self, key: KeyEvent) {
        let Some(ws) = &mut self.workspace else { return };
        let Some(editor) = &mut ws.editor else { return };
        if let Some(popup) = &mut editor.popup {
            match key.code {
                KeyCode::Up => return popup.selected = popup.selected.checked_sub(1).unwrap_or(popup.items.len() - 1),
                KeyCode::Down => return popup.selected = (popup.selected + 1) % popup.items.len(),
                KeyCode::Tab | KeyCode::Enter => return self.accept_completion(),
                KeyCode::Esc => return editor.popup = None,
                _ => {}
            }
        }
        match key.code {
            _ if is_ctrl(key, 'r') => self.run_query(),
            _ if is_ctrl(key, 'k') => self.open_palette(),
            _ if is_ctrl(key, 'o') => {
                self.save_scratch();
                let Some(editor) = self.workspace.as_ref().and_then(|ws| ws.editor.as_ref()) else { return };
                self.external_edit = Some(editor.text());
            }
            KeyCode::Char(' ') if key.modifiers.contains(KeyModifiers::CONTROL) => self.update_completion(true),
            KeyCode::Esc => match ws.run {
                RunState::Running { .. } => self.cancel_query(),
                RunState::Asking { .. } => self.stop_asking(),
                RunState::NeedsWrite(_) => ws.run = RunState::Idle,
                _ => ws.focus = if ws.result.is_some() { Focus::Grid } else { Focus::Tree },
            },
            _ => match editor.handle(key) {
                editor::Outcome::Edited => self.update_completion(false),
                editor::Outcome::Moved => editor.popup = None,
                editor::Outcome::Ignored => {}
            },
        }
    }

    fn update_completion(&mut self, force: bool) {
        let Some(ws) = &mut self.workspace else { return };
        let Some(editor) = &mut ws.editor else { return };
        editor.popup = complete::suggest(&editor.text(), editor.cursor(), force, &ws.tree.schemas, &ws.columns);
    }

    fn accept_completion(&mut self) {
        let Some(editor) = self.workspace.as_mut().and_then(|ws| ws.editor.as_mut()) else { return };
        let Some(popup) = editor.popup.take() else { return };
        let Some(choice) = popup.items.get(popup.selected) else { return };
        let cursor = editor.cursor();
        editor.replace(popup.start.min(cursor)..cursor, &choice.text);
    }

    fn run_query(&mut self) {
        let Some(ws) = &mut self.workspace else { return };
        let Some(editor) = &mut ws.editor else { return };
        editor.popup = None;
        if matches!(ws.run, RunState::Running { .. }) {
            return self.toast(ToastKind::Info, "A query is running. esc stops it");
        }
        let text = editor.text();
        let Some(range) = sql::statement_at(&text, editor.cursor()) else {
            return self.toast(ToastKind::Info, "Nothing to run yet");
        };
        let statement = text[range].to_owned();
        self.execute(statement);
    }

    /// Runs the last statement again (`r` on a result).
    fn rerun(&mut self) {
        if let Some(sql) = self.workspace.as_ref().and_then(|ws| ws.last_sql.clone()) {
            self.execute(sql);
        }
    }

    fn execute(&mut self, sql: String) {
        let id = self.next_id();
        self.save_scratch();
        let Some(ws) = &mut self.workspace else { return };
        // Running the same statement right after it asked for writes is the yes.
        let allow_writes = matches!(&ws.run, RunState::NeedsWrite(pending) if *pending == sql);
        ws.run = RunState::Running { id };
        ws.last_sql = Some(sql.clone());
        let (session, session_id, tx) = (ws.session.clone(), ws.id, self.tx.clone());
        tokio::spawn(async move {
            let result = session.run(&sql, allow_writes).await;
            let _ = tx.send(Msg::QueryDone { session: session_id, id, sql, result });
        });
    }

    fn cancel_query(&mut self) {
        let Some(ws) = &self.workspace else { return };
        let session = ws.session.clone();
        tokio::spawn(async move { session.cancel_editor().await });
    }

    // ---- asking -------------------------------------------------------------

    /// The question box; the first time on a database, the choice of what Claude may see comes first.
    fn open_ask(&mut self) {
        let Some(ws) = &self.workspace else { return };
        let saved = agent::mask::saved(&ws.target);
        let prompt = AskPrompt {
            input: Input::default(),
            follow_up: ws.conversation.is_some(),
            database: ws.target.database().to_owned(),
            visibility: saved.unwrap_or(Visibility::Masked),
        };
        self.overlay = Some(match saved {
            Some(_) => Overlay::Ask(prompt),
            None => Overlay::Privacy(PrivacyPicker::new(ws.target.database(), None, Some(prompt))),
        });
    }

    fn open_privacy(&mut self) {
        let Some(ws) = &self.workspace else { return };
        let saved = agent::mask::saved(&ws.target);
        self.overlay = Some(Overlay::Privacy(PrivacyPicker::new(ws.target.database(), saved, None)));
    }

    /// Saves what Claude may see of this database. A change starts a new
    /// conversation, so nothing carries over from the old setting.
    fn set_visibility(&mut self, visibility: Visibility) {
        let Some(ws) = &mut self.workspace else { return };
        if agent::mask::saved(&ws.target) != Some(visibility) {
            agent::mask::save(&ws.target, visibility);
            ws.conversation = None;
            ws.masker = Arc::default();
        }
        self.toast(ToastKind::Info, format!("Ask: {}", visibility.label().to_lowercase()));
    }

    fn show_sent(&mut self) {
        let Some(ws) = &self.workspace else { return };
        if ws.sent.is_empty() {
            return self.toast(ToastKind::Info, "Nothing has been sent to Claude yet");
        }
        let text: String = ws.sent.iter().map(|(heading, text)| format!("── {heading}\n\n{text}\n\n")).collect();
        self.overlay = Some(Overlay::Value(ValueView::new(
            "Sent to Claude".into(),
            "your last question".into(),
            Some(text.trim_end().to_owned()),
        )));
    }

    /// Hands `question` to the agent. Its answer lands in the editor with its rows below.
    fn ask(&mut self, question: String) {
        let id = self.next_id();
        let Some(visibility) = self.workspace.as_ref().and_then(|ws| agent::mask::saved(&ws.target)) else {
            return self.open_ask();
        };
        let context = self.ask_context(visibility);
        let Some(ws) = &mut self.workspace else { return };
        ws.asked = id;
        ws.sent.clear();
        if let Some(previous) = ws.agent.take() {
            previous.abort();
        }
        let target = &ws.target;
        ws.editor.get_or_insert_with(|| Editor::new(&scratch::load(target)));
        ws.mode = Mode::Query;
        ws.focus = Focus::Editor;
        ws.run = RunState::Asking { id, question: question.clone(), activity: "Starting".into() };
        let request = agent::Request {
            question,
            context,
            conversation: ws.conversation.clone(),
            database: ws.target.database().to_owned(),
            visibility,
            masker: ws.masker.clone(),
        };
        let (tx, workspace) = (self.tx.clone(), ws.id);
        let (handle, conversation) = agent::spawn(ws.session.clone(), request, move |event| {
            let _ = tx.send(Msg::Agent { session: workspace, id, event });
        });
        ws.agent = Some(handle);
        ws.conversation = Some(conversation);
    }

    /// What the user is looking at, in words, so a question can say "these".
    /// The row under the cursor is included only when values may be seen, masked if asked.
    fn ask_context(&self, visibility: Visibility) -> Option<String> {
        let ws = self.workspace.as_ref()?;
        let grid = ws.grid()?;
        let mut context = match ws.mode {
            Mode::Browse => {
                let mut text = format!("The user is looking at {}.{}", grid.relation.schema, grid.relation.name);
                match grid.view.filter() {
                    crate::db::Filter::All => {}
                    crate::db::Filter::Search(words) => text.push_str(&format!(", searching for \"{words}\"")),
                    crate::db::Filter::Condition(condition) => text.push_str(&format!(" where {condition}")),
                }
                if let Some(sort) = &grid.view.sort {
                    let direction = if sort.descending { "descending" } else { "ascending" };
                    text.push_str(&format!(", sorted by {} {direction}", sort.column));
                }
                text.push('.');
                text
            }
            Mode::Query => format!("The user is looking at the result of:\n{}", ws.last_sql.as_deref()?),
        };
        // A filter or query can quote a value, such as an email address.
        if visibility != Visibility::Full {
            context = ws.masker.lock().unwrap().text(&context);
        }
        let row = grid.row_json().and_then(|row| match visibility {
            Visibility::Schema => None,
            Visibility::Masked => {
                let json = serde_json::from_str(&row).ok()?;
                Some(ws.masker.lock().unwrap().json(json).to_string())
            }
            Visibility::Full => Some(row),
        });
        if let Some(row) = row {
            let row: String = row.chars().take(2000).collect();
            context.push_str(&format!("\nThe row under their cursor: {row}"));
        }
        Some(context)
    }

    fn stop_asking(&mut self) {
        let Some(ws) = &mut self.workspace else { return };
        if let Some(agent) = ws.agent.take() {
            agent.abort();
        }
        ws.run = RunState::Done("Stopped".into());
    }

    fn agent_event(&mut self, session: u64, id: u64, event: agent::Event) {
        let generation = self.next_id();
        let Some(ws) = self.workspace.as_mut().filter(|ws| ws.id == session) else { return };
        let asking = matches!(&ws.run, RunState::Asking { id: asking, .. } if *asking == id);
        match event {
            agent::Event::Sent(heading, text) => {
                if ws.asked == id {
                    ws.sent.push((heading, text));
                }
            }
            agent::Event::Activity(text) => {
                if let RunState::Asking { id: asking, activity, .. } = &mut ws.run
                    && *asking == id
                {
                    *activity = text;
                }
            }
            agent::Event::Answer { sql, title, outcome } => {
                let RunState::Asking { question, .. } = &ws.run else { return };
                if !asking {
                    return;
                }
                let comment: String = question.lines().map(|l| format!("-- {l}\n")).collect();
                if let Some(editor) = &mut ws.editor {
                    let text = editor.text();
                    let end = text.len();
                    let gap = if editor.is_blank() || text.ends_with("\n\n") {
                        ""
                    } else if text.ends_with('\n') {
                        "\n"
                    } else {
                        "\n\n"
                    };
                    let start = if editor.is_blank() { 0 } else { end };
                    editor.replace(start..end, &format!("{gap}{comment}{sql};"));
                }
                let count = outcome.rows.len() as u64;
                let rows = if count == 1 { "row" } else { "rows" };
                let size = if outcome.truncated {
                    format!("first {} {rows}", text::thousands(count))
                } else {
                    format!("{} {rows}", text::thousands(count))
                };
                let note = if title.is_empty() { size } else { format!("{title} · {size}") };
                let columns = outcome.columns.unwrap_or_default();
                ws.result = Some(Grid::from_rows(columns, outcome.rows, note, generation));
                ws.last_sql = Some(sql);
                ws.run = RunState::Idle;
                ws.focus = Focus::Grid;
                self.save_scratch();
            }
            agent::Event::Finished { text } => {
                ws.agent = None;
                if asking {
                    // No rows: the agent explains why.
                    let text = if text.is_empty() { "No answer came back.".to_owned() } else { text };
                    ws.run = RunState::Done(text);
                }
            }
            agent::Event::Failed(error) => {
                ws.agent = None;
                if asking {
                    ws.run = RunState::Failed(error);
                } else if !error.is_empty() {
                    self.toast(ToastKind::Error, error);
                }
            }
        }
    }

    fn query_done(&mut self, session: u64, id: u64, sql: String, result: std::result::Result<Outcome, RunError>) {
        let generation = self.next_id();
        let Some(ws) = self.workspace.as_mut().filter(|ws| ws.id == session) else { return };
        if !matches!(ws.run, RunState::Running { id: running } if running == id) {
            return;
        }
        let verb = sql.split_whitespace().next().unwrap_or_default().to_uppercase();
        match result {
            Ok(outcome) => {
                let took = duration(outcome.elapsed);
                if let Some(columns) = outcome.columns {
                    let count = outcome.rows.len() as u64;
                    let rows = if count == 1 { "row" } else { "rows" };
                    let note = if outcome.truncated {
                        format!("first {} {rows} · {took}", text::thousands(count))
                    } else {
                        format!("{} {rows} · {took}", text::thousands(count))
                    };
                    ws.result = Some(Grid::from_rows(columns, outcome.rows, note, generation));
                    ws.run = RunState::Idle;
                } else {
                    let changed = matches!(verb.as_str(), "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "COPY");
                    let message = if changed {
                        let rows = if outcome.affected == 1 { "row" } else { "rows" };
                        format!("{} {rows} changed · {took}", text::thousands(outcome.affected))
                    } else {
                        format!("Done · {took}")
                    };
                    ws.result = None;
                    ws.run = RunState::Done(message);
                    // What the browser shows may have changed underneath it.
                    if changed {
                        self.reload_relation();
                    } else if matches!(verb.as_str(), "CREATE" | "DROP" | "ALTER" | "COMMENT" | "TRUNCATE") {
                        self.load_catalog();
                        self.reload_relation();
                    }
                }
            }
            Err(error) if error.needs_write => ws.run = RunState::NeedsWrite(sql),
            Err(error) if error.canceled => ws.run = RunState::Done("Stopped".into()),
            Err(error) => ws.run = RunState::Failed(error.message),
        }
    }

    fn external_edit_done(&mut self, edited: std::result::Result<String, String>) {
        match edited {
            Ok(text) => {
                if let Some(editor) = self.workspace.as_mut().and_then(|ws| ws.editor.as_mut()) {
                    editor.set_text(text.trim_end_matches('\n'));
                }
                self.save_scratch();
            }
            Err(error) => self.toast(ToastKind::Error, error),
        }
    }

    pub fn save_scratch(&self) {
        if let Some(ws) = &self.workspace
            && let Some(editor) = &ws.editor
        {
            scratch::save(&ws.target, &editor.text());
        }
    }

    fn copy(&mut self, text: &str, what: &str) {
        match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text)) {
            Ok(()) => self.toast(ToastKind::Info, format!("Copied {what}")),
            Err(e) => self.toast(ToastKind::Error, format!("Could not copy: {e}")),
        }
    }

    fn open_palette(&mut self) {
        let Some(ws) = &self.workspace else { return };
        let several_schemas = ws.tree.schemas.iter().filter(|s| !s.relations.is_empty()).count() > 1;
        let mut commands: Vec<Command> = ws
            .tree
            .schemas
            .iter()
            .flat_map(|s| &s.relations)
            .map(|relation| Command {
                label: if several_schemas {
                    format!("{}.{}", relation.schema, relation.name)
                } else {
                    relation.name.clone()
                },
                tag: match relation.kind {
                    RelationKind::Table => String::new(),
                    kind => kind.label().to_owned(),
                },
                action: Action::Open(relation.clone()),
            })
            .collect();
        for database in ws.databases.iter().filter(|d| d.as_str() != ws.target.database()) {
            commands.push(Command {
                label: database.clone(),
                tag: "database".into(),
                action: Action::Database(database.clone()),
            });
        }
        let actions = [
            ("Ask about the data", "a", Action::Ask),
            ("What Ask can see", "", Action::AskPrivacy),
            ("SQL editor", "ctrl+e", Action::Query),
            ("Switch connection", "", Action::SwitchConnection),
            ("Reload tables", "", Action::Reload),
            ("Keyboard shortcuts", "?", Action::Help),
            ("Quit", "q", Action::Quit),
        ];
        for (label, tag, action) in actions {
            commands.push(Command { label: label.into(), tag: tag.into(), action });
        }
        if !ws.sent.is_empty() {
            commands.push(Command {
                label: "What was sent to Claude".into(),
                tag: "w".into(),
                action: Action::AskSent,
            });
        }
        if let Some((version, _)) = &self.update {
            let label = format!("Update Scarlet to {version}");
            commands.insert(0, Command { label, tag: "new".into(), action: Action::Update });
        }
        self.overlay = Some(Overlay::Palette(Palette::new(commands)));
    }

    fn run_action(&mut self, action: Action) {
        match action {
            Action::Open(relation) => {
                let Some(ws) = &mut self.workspace else { return };
                ws.tree.reveal(&relation);
                ws.focus = Focus::Grid;
                ws.mode = Mode::Browse;
                self.open_relation(relation);
            }
            Action::Database(name) => {
                let Some(ws) = &self.workspace else { return };
                let target = ws.target.with_database(&name);
                self.connect(target);
            }
            Action::SwitchConnection => {
                let current = self.workspace.as_ref().map(|ws| ws.target.clone());
                self.open_picker(None, current.as_ref());
            }
            Action::Follow(relation, view) => self.open_linked(relation, view),
            Action::Query => self.toggle_query(),
            Action::Ask => self.open_ask(),
            Action::AskPrivacy => self.open_privacy(),
            Action::AskSent => self.show_sent(),
            Action::Update => self.update_now = true,
            Action::Reload => self.load_catalog(),
            Action::Help => self.overlay = Some(Overlay::Help),
            Action::Quit => self.quit = true,
        }
    }

    /// Keys worth showing right now, most useful first.
    pub fn hints(&self) -> Vec<(String, String)> {
        let pairs = |items: &[(&str, &str)]| -> Vec<(String, String)> {
            items.iter().map(|(k, l)| ((*k).into(), (*l).into())).collect()
        };
        match &self.overlay {
            Some(Overlay::Connect(_)) => {
                let close = if self.workspace.is_some() { "close" } else { "quit" };
                return pairs(&[("↑↓", "select"), ("enter", "connect"), ("esc", close)]);
            }
            Some(Overlay::Password(_)) => return pairs(&[("enter", "connect"), ("esc", "back")]),
            Some(Overlay::Ask(prompt)) if prompt.follow_up => {
                return pairs(&[
                    ("enter", "ask"),
                    ("ctrl+n", "new conversation"),
                    ("ctrl+p", "what Claude sees"),
                    ("esc", "close"),
                ]);
            }
            Some(Overlay::Ask(_)) => {
                return pairs(&[("enter", "ask"), ("ctrl+p", "what Claude sees"), ("esc", "close")]);
            }
            Some(Overlay::Privacy(_)) => return pairs(&[("↑↓", "choose"), ("enter", "use this"), ("esc", "cancel")]),
            Some(Overlay::Palette(_)) => return pairs(&[("↑↓", "select"), ("enter", "go"), ("esc", "close")]),
            Some(Overlay::Value(_)) => return pairs(&[("↑↓", "scroll"), ("c", "copy"), ("esc", "close")]),
            Some(Overlay::Row(_)) => return pairs(&[("↑↓", "scroll"), ("Y", "copy as JSON"), ("esc", "close")]),
            Some(Overlay::Menu(_) | Overlay::Help) => return pairs(&[("esc", "close")]),
            None => {}
        }
        let Some(ws) = &self.workspace else { return pairs(&[("q", "quit")]) };
        let leader = self.leader_label();
        if matches!(ws.run, RunState::Asking { .. }) {
            return pairs(&[("esc", "stop")]);
        }
        if ws.focus == Focus::Editor {
            let popup = ws.editor.as_ref().is_some_and(|e| e.popup.is_some());
            return match &ws.run {
                _ if popup => pairs(&[("↑↓", "choose"), ("tab", "insert"), ("esc", "close")]),
                RunState::Running { .. } => pairs(&[("esc", "stop the query")]),
                RunState::NeedsWrite(_) => pairs(&[("ctrl+r", "run with writes"), ("esc", "don't")]),
                _ => {
                    let mut hints = pairs(&[("ctrl+r", "run"), ("ctrl+space", "complete")]);
                    hints.push(("ctrl+o".into(), editor_name()));
                    hints.extend(pairs(&[("esc", "results"), ("ctrl+e", "tables")]));
                    hints
                }
            };
        }
        match (ws.focus, ws.grid()) {
            (Focus::Tree, _) if ws.tree.filtering => pairs(&[("↑↓", "select"), ("enter", "open"), ("esc", "clear")]),
            (Focus::Grid, Some(grid)) if grid.filter_input.is_some() => pairs(&[("enter", "apply"), ("esc", "cancel")]),
            (Focus::Grid, Some(grid)) => {
                let mut hints = pairs(&[("←↑↓→", "move")]);
                hints.push(("enter".into(), self.enter_label(grid)));
                hints.extend(pairs(&[("/", "filter"), ("s", "sort"), ("a", "ask")]));
                hints.push((leader, "more".into()));
                hints.push(("esc".into(), self.escape_label(ws, grid).into()));
                hints
            }
            _ => {
                let mut hints = pairs(&[
                    ("↑↓", "move"),
                    ("→", "open"),
                    ("/", "filter"),
                    ("a", "ask"),
                    ("ctrl+e", "sql"),
                    ("ctrl+k", "jump"),
                ]);
                hints.push((leader, "more".into()));
                hints.extend(pairs(&[("q", "quit")]));
                hints
            }
        }
    }
}

/// Adds `condition` to the view's filter. A search is spelled out in SQL so
/// both apply at once.
fn and_filter(view: &View, alias: &str, condition: &str) -> String {
    match view.filter() {
        Filter::All => condition.to_owned(),
        Filter::Search(words) => format!("{} AND {condition}", search_condition(alias, words)),
        Filter::Condition(sql) if outside_quotes(&sql.to_lowercase()).contains(" or ") => {
            format!("({sql}) AND {condition}")
        }
        Filter::Condition(sql) => format!("{sql} AND {condition}"),
    }
}

/// The browsed table as SQL: its filter and sort carried over.
fn starter_query(table: &Grid) -> String {
    let relation = &table.relation;
    let name = if relation.schema == "public" {
        ident(&relation.name)
    } else {
        format!("{}.{}", ident(&relation.schema), ident(&relation.name))
    };
    let alias = if table.alias.is_empty() { relation.name.clone() } else { table.alias.clone() };
    let mut sql = format!("SELECT *\nFROM {name}");
    if alias != relation.name {
        sql.push_str(&format!(" AS {}", ident(&alias)));
    }
    match table.view.filter() {
        Filter::All => {}
        Filter::Search(words) => sql.push_str(&format!("\nWHERE {}", search_condition(&alias, words))),
        Filter::Condition(condition) => sql.push_str(&format!("\nWHERE {condition}")),
    }
    if let Some(sort) = &table.view.sort {
        sql.push_str(&format!("\nORDER BY {} {}", ident(&sort.column), if sort.descending { "DESC" } else { "ASC" }));
    }
    sql.push_str("\nLIMIT 100;");
    sql
}

/// 12 ms, 1.4 s
fn duration(elapsed: Duration) -> String {
    let ms = elapsed.as_millis();
    if ms < 1000 { format!("{ms} ms") } else { format!("{:.1} s", elapsed.as_secs_f64()) }
}

/// $SCARLET_EDITOR, $VISUAL or $EDITOR, whichever is set first.
fn configured_editor() -> Option<String> {
    ["SCARLET_EDITOR", "VISUAL", "EDITOR"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|v| !v.trim().is_empty()))
}

/// The program `ctrl+o` opens, for the footer.
fn editor_name() -> String {
    configured_editor()
        .and_then(|e| e.split_whitespace().next().map(|p| p.rsplit('/').next().unwrap_or(p).to_owned()))
        .unwrap_or_else(|| "nvim".into())
}
