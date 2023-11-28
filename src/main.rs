use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode, KeyEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use directories::BaseDirs;
use parking_lot::{Mutex, RwLock};
use std::{
    collections::{HashSet, VecDeque},
    error::Error,
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant}, io,
};
use strum::EnumIter;
use strum::IntoEnumIterator;
use tokio::sync::Notify;
use tracing::{Level, error};
use tracing::{debug, info, warn};
use tracing_subscriber::{filter::LevelFilter, layer::SubscriberExt, registry::Registry, Layer};
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Span, Spans, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};
use winreg::{enums::*, RegKey};

const DEBOUNCE: Duration = Duration::from_millis(100);
const EVENT_POLL_TIMEOUT: Duration = Duration::from_millis(200);
const SELECTION_COLOUR: Color = Color::Cyan;

static KEY_COUNT: AtomicUsize = AtomicUsize::new(0);
static VALUE_COUNT: AtomicUsize = AtomicUsize::new(0);
static HKLM: RegKey = RegKey::predef(HKEY_LOCAL_MACHINE);

const REGEDIT_OUTPUT_FOR_BLANK_NAMES: bool = true;

pub struct WorkerManager {
    threads: usize,
    search_terms: Vec<String>,
    key_queue: Arc<Mutex<VecDeque<String>>>,
    work_ready_for_processing: Arc<Notify>,
    threads_waiting_for_work: Arc<AtomicUsize>,
    no_work_left: Arc<Notify>,
    pub results: Arc<Mutex<HashSet<String>>>,
    pub errors: Arc<Mutex<HashSet<String>>>,
}

impl WorkerManager {
    pub fn new(search_terms: Vec<String>, threads_to_use: usize) -> Self {
        Self {
            threads: threads_to_use,
            search_terms: search_terms
                .into_iter()
                .map(|term| term.to_lowercase())
                .collect(),
            key_queue: Arc::new(Mutex::new(VecDeque::new())),
            work_ready_for_processing: Arc::new(Notify::new()),
            threads_waiting_for_work: Arc::new(AtomicUsize::new(0)),

            no_work_left: Arc::new(Notify::new()),

            results: Arc::new(Mutex::new(HashSet::new())),
            errors: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn feed_queue_and_process_values(&self, key_path: &str) -> Result<(), Box<dyn Error>> {
        if self.string_matches(key_path) {
            self.results
                .lock()
                .insert(format!("HKEY_LOCAL_MACHINE\\{}", key_path));
        }
        let registry_key = HKLM.open_subkey_with_flags(key_path, KEY_READ)?;
        {
            let mut key_paths = Vec::new();
            for key_result in registry_key.enum_keys() {
                KEY_COUNT.fetch_add(1, Ordering::SeqCst);
                match key_result {
                    Ok(key_name) => {
                        let key_path = format!("{}\\{}", key_path, key_name);
                        key_paths.push(key_path);
                    }
                    Err(err) => {
                        self.errors
                            .lock()
                            .insert(format!("{}, Subkey error: \"{}\"", key_path, err));
                    }
                }
            }
            self.feed_queue(key_paths);
            self.work_ready_for_processing.notify_waiters();
        }

        for value_result in registry_key.enum_values() {
            VALUE_COUNT.fetch_add(1, Ordering::SeqCst);
            match value_result {
                Ok((value_name, reg_value)) => {
                    let data = reg_value.to_string();
                    if self.any_string_matches(&value_name, &data) {
                        let value_name = if value_name.is_empty() {
                            if REGEDIT_OUTPUT_FOR_BLANK_NAMES {
                                "(Default)".to_string()
                            } else {
                                value_name
                            }
                        } else {
                            value_name
                        };
                        self.results.lock().insert(format!(
                            "HKEY_LOCAL_MACHINE\\{}\\{} = \"{}\" ({:?})",
                            key_path, value_name, data, reg_value.vtype,
                        ));
                    }
                }
                Err(err) => {
                    self.errors
                        .lock()
                        .insert(format!("{}, Value error: \"{}\"", key_path, err));
                }
            }
        }
        Ok(())
    }

    pub async fn get_work(&self) -> Option<String> {
        loop {
            let work = self.key_queue.lock().pop_front();
            if let Some(key) = work {
                return Some(key);
            } else {
                self.threads_waiting_for_work.fetch_add(1, Ordering::SeqCst);
                tokio::select! {
                    _ = self.work_ready_for_processing.notified() => {},
                    _ = self.no_work_left.notified() => return None,
                }
                self.threads_waiting_for_work.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    pub fn feed_queue(&self, keys: Vec<String>) {
        let mut lock = self.key_queue.lock();
        lock.extend(keys);
    }

    pub fn any_string_matches(&self, string: &str, string2: &str) -> bool {
        let string_lowercase = string.to_lowercase();
        let string2_lowercase = string2.to_lowercase();
        for term in self.search_terms.iter() {
            if string_lowercase.contains(term) || string2_lowercase.contains(term) {
                return true;
            }
        }
        false
    }

    pub fn string_matches(&self, string: &str) -> bool {
        let string_lowercase = string.to_lowercase();
        for term in self.search_terms.iter() {
            if string_lowercase.contains(term) {
                return true;
            }
        }
        false
    }

    pub async fn run(&self, worker_manager: Arc<WorkerManager>) {
        for _ in 0..worker_manager.threads {
            let worker_manager = worker_manager.to_owned();
            tokio::spawn(run_thread(worker_manager));
        }
        self.work_ready_for_processing.notify_waiters();
        loop {
            if worker_manager
                .threads_waiting_for_work
                .load(Ordering::SeqCst)
                == worker_manager.threads
            {
                if self.key_queue.lock().len() == 0 {
                    self.no_work_left.notify_waiters();
                    break;
                } else {
                    self.work_ready_for_processing.notify_waiters();
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

async fn run_thread(worker_manager: Arc<WorkerManager>) {
    loop {
        let key_path = match worker_manager.get_work().await {
            Some(key_path) => key_path,
            None => break,
        };
        if let Err(err) = worker_manager.feed_queue_and_process_values(&key_path) {
            worker_manager
                .errors
                .lock()
                .insert(format!("{}, Key error: \"{}\"", key_path, err));
        }
    }
}

#[derive(EnumIter, Copy, Clone)]
pub enum Root {
    HkeyClassesRoot = 0,
    HkeyCurrentUser = 1,
    HkeyLocalMachine = 2,
    HkeyUsers = 3,
    HkeyCurrentConfig = 4,
}

impl fmt::Display for Root {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::HkeyClassesRoot => "HKEY_CLASSES_ROOT",
                Self::HkeyCurrentUser => "HKEY_CURRENT_USER",
                Self::HkeyLocalMachine => "HKEY_LOCAL_MACHINE",
                Self::HkeyUsers => "HKEY_USERS",
                Self::HkeyCurrentConfig => "HKEY_CURRENT_CONFIG",
            }
        )
    }
}

impl Root {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Root::HkeyClassesRoot),
            1 => Some(Root::HkeyCurrentUser),
            2 => Some(Root::HkeyLocalMachine),
            3 => Some(Root::HkeyUsers),
            4 => Some(Root::HkeyCurrentConfig),
            _ => None,
        }
    }
}

pub struct SelectedRoots {
    classes_root: bool,
    current_user: bool,
    local_machine: bool,
    users: bool,
    current_config: bool,
}

impl Default for SelectedRoots {
    fn default() -> Self {
        Self {
            classes_root: false,
            current_user: false,
            local_machine: true,
            users: true,
            current_config: false,
        }
    }
}

impl SelectedRoots {
    pub fn export_roots(&self) -> Vec<Root> {
        let mut selected_roots = Vec::new();

        if self.classes_root {
            selected_roots.push(Root::HkeyClassesRoot);
        }
        if self.current_user {
            selected_roots.push(Root::HkeyCurrentUser);
        }
        if self.local_machine {
            selected_roots.push(Root::HkeyLocalMachine);
        }
        if self.users {
            selected_roots.push(Root::HkeyUsers);
        }
        if self.current_config {
            selected_roots.push(Root::HkeyCurrentConfig);
        }

        selected_roots
    }

    pub fn is_enabled(&self, root: &Root) -> bool {
        match root {
            Root::HkeyClassesRoot => self.classes_root,
            Root::HkeyCurrentUser => self.current_user,
            Root::HkeyLocalMachine => self.local_machine,
            Root::HkeyUsers => self.users,
            Root::HkeyCurrentConfig => self.current_config,
        }
    }

    pub fn toggle(&mut self, root: &Root) {
        match root {
            Root::HkeyClassesRoot => self.classes_root = !self.classes_root,
            Root::HkeyCurrentUser => self.current_user = !self.current_user,
            Root::HkeyLocalMachine => self.local_machine = !self.local_machine,
            Root::HkeyUsers => self.users = !self.users,
            Root::HkeyCurrentConfig => self.current_config = !self.current_config,
        }
    }
}

pub struct StaticSelection {
    pane_selected: Arc<AtomicU8>,           //horizontal
    pane_last_changed: Arc<Mutex<Instant>>, //horizontal
    search_term_selected: Arc<AtomicUsize>,
    search_term_last_changed: Arc<Mutex<Instant>>,

    root_selected: Arc<AtomicU8>,
    root_selection_last_changed: Arc<Mutex<Instant>>,

    selected_roots: Arc<RwLock<SelectedRoots>>,
    search_terms: Arc<RwLock<HashSet<String>>>,

    running: Arc<AtomicBool>,
    run_control_temporarily_disabled: Arc<AtomicBool>, //running thread resets this once closed
    stop: Arc<AtomicBool>,                             //running thread resets this once closed
}

impl Default for StaticSelection {
    fn default() -> Self {
        Self {
            pane_selected: Arc::new(AtomicU8::new(0)),
            pane_last_changed: Arc::new(Mutex::new(Instant::now())),
            search_term_selected: Arc::new(AtomicUsize::new(0)),
            search_term_last_changed: Arc::new(Mutex::new(Instant::now())),
            root_selected: Arc::new(AtomicU8::new(0)),
            root_selection_last_changed: Arc::new(Mutex::new(Instant::now())),
            selected_roots: Arc::new(RwLock::new(SelectedRoots::default())),
            search_terms: Arc::new(RwLock::new(HashSet::new())),
            running: Arc::new(AtomicBool::new(false)),
            run_control_temporarily_disabled: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl StaticSelection {
    pub fn generate_root_list(&self) -> Vec<Spans<'static>> {
        let root_selected = self.root_selected.load(Ordering::SeqCst);
        let pane_selected = self.pane_selected.load(Ordering::SeqCst) == 0;
        Root::iter()
            .map(|root| {
                let root_enabled = self.selected_roots.read().is_enabled(&root);
                Spans::from(vec![
                    Span::styled(
                        format!("{:25}", root.to_string(),),
                        Style::default().fg(if pane_selected && root as u8 == root_selected {
                            SELECTION_COLOUR
                        } else {
                            Color::White
                        }),
                    ),
                    Span::styled(
                        if root_enabled { "Enabled" } else { "Disabled" },
                        Style::default().fg(if root_enabled {
                            Color::Green
                        } else {
                            Color::White
                        }),
                    ),
                ])
            })
            .collect::<Vec<Spans>>()
    }

    pub fn render_search_terms(&self) -> Vec<Spans<'static>> {
        let search_term_selected = self.search_term_selected.load(Ordering::SeqCst);
        let pane_selected = self.pane_selected.load(Ordering::SeqCst) == 1;
        self.search_terms.read().iter().enumerate()
            .map(|(index, term)| {
                Spans::from(vec![
                    Span::styled(
                        format!("{:25}", term.to_string(),),
                        Style::default().fg(if pane_selected && index == search_term_selected {
                            SELECTION_COLOUR
                        } else {
                            Color::White
                        }),
                    ),
                ])
            })
            .collect::<Vec<Spans>>()
    }

    pub fn pane_left(&self) {
        if self.pane_last_changed.lock().elapsed() < DEBOUNCE {
            return;
        }
        let new_value = match self.pane_selected.load(Ordering::SeqCst) {
            0 => 2,
            1 => 0,
            2 => 1,
            _ => return,
        };
        self.pane_selected.store(new_value, Ordering::SeqCst);
        *self.pane_last_changed.lock() = Instant::now();
    }

    pub fn pane_right(&self) {
        if self.pane_last_changed.lock().elapsed() < DEBOUNCE {
            return;
        }
        let new_value = match self.pane_selected.load(Ordering::SeqCst) {
            0 => 1,
            1 => 2,
            2 => 0,
            _ => return,
        };
        self.pane_selected.store(new_value, Ordering::SeqCst);
        *self.pane_last_changed.lock() = Instant::now();
    }

    pub fn root_up(&self) {
        if self.root_selection_last_changed.lock().elapsed() < DEBOUNCE {
            return;
        }
        let new_value = match self.root_selected.load(Ordering::SeqCst) {
            0 => 4,
            1 => 0,
            2 => 1,
            3 => 2,
            4 => 3,
            _ => return,
        };
        self.root_selected.store(new_value, Ordering::SeqCst);
        *self.root_selection_last_changed.lock() = Instant::now();
    }

    pub fn root_down(&self) {
        if self.root_selection_last_changed.lock().elapsed() < DEBOUNCE {
            return;
        }
        let new_value = match self.root_selected.load(Ordering::SeqCst) {
            0 => 1,
            1 => 2,
            2 => 3,
            3 => 4,
            4 => 0,
            _ => return,
        };
        self.root_selected.store(new_value, Ordering::SeqCst);
        *self.root_selection_last_changed.lock() = Instant::now();
    }

    pub fn search_term_up(&self) {

    }

    pub fn search_term_down(&self) {
        if self.search_term_last_changed.lock().elapsed() < DEBOUNCE {
            return;
        }
        let search_terms_len = self.search_terms.read().len();
        if search_terms_len == 0 {
            return;
        }
        let max_index: usize = if search_terms_len > 1 {
            search_terms_len - 1
        } else {
            search_terms_len
        };
        let current = self.search_term_selected.load(Ordering::SeqCst);
        self.search_term_selected.store(if current + 1 <= max_index {
            current + 1
        } else {
            0
        }, Ordering::SeqCst);
        *self.search_term_last_changed.lock() = Instant::now();
    }

    pub fn root_toggle(&self) {
        let selected = self.root_selected.load(Ordering::SeqCst);
        if let Some(root) = Root::from_u8(selected) {
            self.selected_roots.write().toggle(&root);
        }
    }

    pub fn toggle_running(&self) {
        if self.running.load(Ordering::SeqCst) {
            self.run_control_temporarily_disabled
                .store(true, Ordering::SeqCst);
            self.stop.store(true, Ordering::SeqCst);
        } else {
            //TODO: Start the running thing
            //dummy runtime running for 5 seconds
            self.run_control_temporarily_disabled
                .store(true, Ordering::SeqCst);
            let stop = self.stop.to_owned();
            let run_control_temporarily_disabled = self.run_control_temporarily_disabled.to_owned();
            let running = self.running.to_owned();
            let _ = thread::spawn(move || {
                running.store(true, Ordering::SeqCst);
                run_control_temporarily_disabled.store(false, Ordering::SeqCst);
                thread::sleep(Duration::from_secs(5));
                stop.store(false, Ordering::SeqCst);
                running.store(false, Ordering::SeqCst);
                run_control_temporarily_disabled.store(false, Ordering::SeqCst);
            });
        }
    }
}

#[derive(Debug, Clone)]
pub enum EditorMode {
    Add,
    Edit(String),
}

#[derive(Debug, Clone)]
pub struct SearchEditor {
    mode: EditorMode,
    state: String,
}

impl SearchEditor {
    pub fn new_add() -> Self {
        Self {
            mode: EditorMode::Add,
            state: String::new(),
        }
    }
    pub fn new_edit(original: String) -> Self {
        Self {
            mode: EditorMode::Edit(original.to_owned()),
            state: original,
        }
    }
    pub fn add_char(&mut self, ch: char) {
        self.state.push(ch);
    }
    pub fn backspace(&mut self) {
        let _ = self.state.pop();
    }
    pub fn resolve(self) -> (EditorMode, String) {
        (self.mode, self.state)
    }

    pub fn render(&self) -> Spans<'static> {
        Spans::from(vec![
            Span::styled(
                format!("{}", self.state),
                Style::default().fg(Color::White),
            )
        ])
    }
}

#[derive(Debug, Clone)]
pub enum Focus {
    Main,
    SearchAdd(Arc<RwLock<Option<SearchEditor>>>),
    SearchEdit(Arc<RwLock<Option<SearchEditor>>>),
    Help,
    ConfirmClose,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let base_directories = BaseDirs::new().expect("Base directories not found");
    let log_path = base_directories
        .config_dir()
        .join("windows_registry_search/logs/");
    let file = tracing_appender::rolling::daily(log_path, format!("log"));
    let (file_writer, _guard) = tracing_appender::non_blocking(file);
    let level_filter = LevelFilter::from_level(Level::DEBUG);
    let logfile_layer = tracing_subscriber::fmt::layer()
        .with_line_number(true)
        .with_writer(file_writer)
        .with_filter(level_filter);
    let subscriber = Registry::default().with(logfile_layer);
    tracing::subscriber::set_global_default(subscriber).unwrap();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let focus: Arc<RwLock<Focus>> = Arc::new(RwLock::new(Focus::Main));

    let static_menu_selection: Arc<StaticSelection> = Arc::new(StaticSelection::default());
    let static_menu_selection_event_receiver = static_menu_selection.to_owned();
    let stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let stop_ = stop.to_owned();
    let focus_ = focus.to_owned();
    thread::spawn(move || loop {
        if event::poll(EVENT_POLL_TIMEOUT).unwrap() {
            if let Ok(CEvent::Key(key)) = event::read() {
                if let KeyEventKind::Press = key.kind {
                    let focus = focus_.read().to_owned();
                    match focus {
                        Focus::Main => match key.code {
                            KeyCode::Char('n') => *focus_.write() = Focus::SearchAdd(Arc::new(RwLock::new(Some(SearchEditor::new_add())))),
                            KeyCode::Char('h') => *focus_.write() = Focus::Help,
                            KeyCode::Char('q') | KeyCode::Esc => *focus_.write() = Focus::ConfirmClose,
                            KeyCode::Left => static_menu_selection_event_receiver.pane_left(),
                            KeyCode::Right => static_menu_selection_event_receiver.pane_right(),
                            KeyCode::Up => match static_menu_selection_event_receiver
                                .pane_selected
                                .load(Ordering::SeqCst)
                            {
                                0 => static_menu_selection_event_receiver.root_up(),
                                1 => static_menu_selection_event_receiver.search_term_up(),
                                2 => {}
                                _ => {}
                            },
                            KeyCode::Down => match static_menu_selection_event_receiver
                                .pane_selected
                                .load(Ordering::SeqCst)
                            {
                                0 => static_menu_selection_event_receiver.root_down(),
                                1 => static_menu_selection_event_receiver.search_term_down(),
                                2 => {}
                                _ => {}
                            },
                            KeyCode::Enter => match static_menu_selection_event_receiver
                                .pane_selected
                                .load(Ordering::SeqCst)
                            {
                                0 => static_menu_selection_event_receiver.root_toggle(),
                                1 => {}
                                2 => {}
                                _ => {}
                            },
                            KeyCode::F(5) => static_menu_selection_event_receiver.toggle_running(),
                            _ => {}
                        },
                        Focus::SearchAdd(search_editor) => match key.code {
                            KeyCode::Backspace => search_editor.write().as_mut().unwrap().backspace(),
                            KeyCode::Char(ch) => search_editor.write().as_mut().unwrap().add_char(ch),
                            KeyCode::Esc => *focus_.write() = Focus::Main,
                            KeyCode::Enter => {
                                let mut focus_lock = focus_.write();//this lock must be held until the end of this scope
                                let mut search_editor_lock = search_editor.write();//it is imperitive that nothing tries to read this lock after this write cycle, it should be safe
                                let probably_search_editor = search_editor_lock.take();
                                *focus_lock = Focus::Main;
                                let search_editor = match probably_search_editor {
                                    Some(search_editor) => search_editor,
                                    None => {
                                        error!("Write proper error here, this shouldn't be possible as this loop runthrough is the only place that can both run a write lock on search_editor or focus.");
                                        continue;
                                    }
                                };
                                let (editor_mode, state) = search_editor.resolve();
                                let mut search_terms_lock = static_menu_selection_event_receiver.search_terms.write();
                                match editor_mode {
                                    EditorMode::Add => {
                                        let _ = search_terms_lock.insert(state);
                                    },
                                    EditorMode::Edit(original) => {
                                        search_terms_lock.remove(&original);
                                        let _ = search_terms_lock.insert(state);
                                    },
                                }
                            },
                            _ => {},
                        }
                        Focus::SearchEdit(search_editor) => {}
                        Focus::Help => match key.code {
                            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('h') => *focus_.write() = Focus::Main,
                            _ => {}
                        },
                        Focus::ConfirmClose => match key.code {
                            KeyCode::Esc | KeyCode::Char('n') => {
                                *focus_.write() = Focus::Main;
                            }
                            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('q') => {
                                stop_.store(true, Ordering::SeqCst);
                                break;
                            }
                            _ => {}
                        },
                    }
                }
            }
        } else {
        }
    });

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Max(100)].as_ref())
                .split(f.size());
            let top_paragraph = Paragraph::new(
                vec![
                    "H for the Help menu",
                    "Arrow keys for navigation",
                    "Enter to select/toggle",
                    "Page up/down for first/last element",
                    "F5 Start/Stop",
                ]
                .iter()
                .map(|&tip| format!("[{}] ", tip))
                .collect::<String>(),
            )
            .block(Block::default())
            .wrap(Wrap { trim: true });
            f.render_widget(top_paragraph, chunks[0]);
            let bottom_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .margin(1)
                .constraints(
                    [
                        Constraint::Percentage(20), // Selection
                        Constraint::Percentage(20), // Controls
                        Constraint::Percentage(60), // Results
                    ]
                    .as_ref(),
                )
                .split(chunks[1]);

            let pane_selected = static_menu_selection.pane_selected.load(Ordering::SeqCst);

            let left_paragraph = Paragraph::new(static_menu_selection.generate_root_list()).block(
                Block::default()
                    .title(Span::styled(
                        "Root Selection",
                        Style::default().fg(Color::White),
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if pane_selected == 0 {
                        SELECTION_COLOUR
                    } else {
                        Color::White
                    })),
            );
            f.render_widget(left_paragraph, bottom_chunks[0]);

            let mut controls: Vec<Spans> = Vec::new();
            let running = static_menu_selection.running.load(Ordering::SeqCst);
            let run_control_disabled = static_menu_selection
                .run_control_temporarily_disabled
                .load(Ordering::SeqCst);
            controls.push(Spans::from(Span::styled(
                if running {
                    if running && run_control_disabled {
                        "Stopping"
                    } else {
                        "Stop"
                    }
                } else {
                    "Start"
                },
                Style::default().fg(if running && !run_control_disabled {
                    Color::Green
                } else if running && run_control_disabled {
                    Color::Red
                } else {
                    Color::White
                }),
            )));

            let middle_column = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(92), Constraint::Percentage(8)].as_ref())
                .split(bottom_chunks[1]);

            let search_terms_paragraph = Paragraph::new(static_menu_selection.render_search_terms())
                .block(
                    Block::default()
                        .title(Span::styled(
                            "Search Terms",
                            Style::default().fg(Color::White),
                        ))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(if pane_selected == 1 {
                            SELECTION_COLOUR
                        } else {
                            Color::White
                        })),
                )
                .wrap(Wrap { trim: true });
            f.render_widget(search_terms_paragraph, middle_column[0]);
            let controls_paragraph = Paragraph::new(controls)
                .block(
                    Block::default()
                        .title(Span::styled("Controls", Style::default().fg(Color::White)))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::White)),
                )
                .wrap(Wrap { trim: true });
            f.render_widget(controls_paragraph, middle_column[1]);

            let right_text = Text::from("Results will be shown here.");
            let right_paragraph = Paragraph::new(right_text).block(
                Block::default()
                    .title(Span::styled("Results", Style::default().fg(Color::White)))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if pane_selected == 2 {
                        SELECTION_COLOUR
                    } else {
                        Color::White
                    })),
            );
            f.render_widget(right_paragraph, bottom_chunks[2]);

            //Renders overlay
            let focus = focus.read().to_owned();
            match focus {
                Focus::Main => {}
                _ => {
                    let vertical_split = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints(
                            [
                                Constraint::Ratio(1, 3),
                                Constraint::Ratio(1, 3),
                                Constraint::Ratio(1, 3),
                            ]
                            .as_ref(),
                        )
                        .split(f.size());
                    let horizontal_split = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints(
                            [
                                Constraint::Ratio(1, 3),
                                Constraint::Ratio(1, 3),
                                Constraint::Ratio(1, 3),
                            ]
                            .as_ref(),
                        )
                        .split(vertical_split[1]);
                    let middle_pane = horizontal_split[1];
                    let paragraph = match focus {
                        Focus::ConfirmClose => Paragraph::new("Y/N").block(
                            Block::default()
                                .title(Span::styled(
                                    "Confirm Close",
                                    Style::default().fg(Color::White),
                                ))
                                .style(Style::default().bg(Color::DarkGray))
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::White)),
                        ),
                        Focus::Help => Paragraph::new("Placeholder").block(
                            Block::default()
                                .title(Span::styled(
                                    "Help/Controls",
                                    Style::default().fg(Color::White),
                                ))
                                .style(Style::default().bg(Color::DarkGray))
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::White)),
                        ),
                        Focus::SearchAdd(search_editor) => Paragraph::new(search_editor.read().as_ref().unwrap().render()).block(
                            Block::default()
                                .title(Span::styled(
                                    "Search Add/Update",
                                    Style::default().fg(Color::White),
                                ))
                                .style(Style::default().bg(Color::DarkGray))
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::White)),
                        ),
                        Focus::SearchEdit(search_editor) => Paragraph::new(search_editor.read().as_ref().unwrap().render()).block(
                            Block::default()
                                .title(Span::styled(
                                    "Search Add/Update",
                                    Style::default().fg(Color::White),
                                ))
                                .style(Style::default().bg(Color::DarkGray))
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::White)),
                        ),
                        Focus::Main => panic!(), //this case will never run
                    };
                    f.render_widget(paragraph, middle_pane);
                }
            }
        })?;
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    /* let worker_manager = Arc::new(WorkerManager::new(vec!["Google Chrome".to_string(), "7-Zip".to_string()], num_cpus::get()));

    worker_manager.feed_queue(vec!["Software".to_string()]);
    let start_time = Instant::now();
    worker_manager.run(worker_manager.to_owned()).await;

    eprintln!("Errors:");
    for error in worker_manager.errors.lock().iter() {
        eprintln!("{}", error);
    }

    println!("\nResults:");
    for result in worker_manager.results.lock().iter() {
        println!("{}", result);
    }
    println!("Key count: {}, Value count: {}", KEY_COUNT.load(Ordering::SeqCst), VALUE_COUNT.load(Ordering::SeqCst));
    println!("Completed in {}ms.", start_time.elapsed().as_millis()); */
    Ok(())
}
