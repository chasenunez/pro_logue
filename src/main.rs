use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::env;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDateTime, Datelike};
use crossterm::event::{self, Event as CEvent, KeyCode, KeyEvent, KeyModifiers};
use git2::{Cred, PushOptions, RemoteCallbacks, Repository, Signature};
use log::{error, info, warn};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame, Terminal,
};
use serde::{Deserialize, Serialize};

/// ---------------------------
///// === CONFIGURATION ===
/// ---------------------------
const GIT_REPO_PATH: &str = "~/Documents/log_cold_storage/";
//const DATA_FILE_NAME: &str = "2026.json"; //dynamically created each year using 'data_file_name' below. 
const COMMIT_AND_PUSH: bool = true;
const AUTHOR_NAME: &str = "Chase Núñez";
const AUTHOR_EMAIL: &str = "chasenunez@gmail.com";
const GIT_REMOTE_NAME: &str = "origin";
const GIT_BRANCH: &str = "main";

const LOGO: &str = r#"┏━┓┏━┓┏━┓   ╻  ┏━┓┏━╸╻ ╻┏━╸
┣━┛┣┳┛┃ ┃   ┃  ┃ ┃┃╺┓┃ ┃┣╸ 
╹  ╹┗╸┗━┛╺━╸┗━╸┗━┛┗━┛┗━┛┗━╸"#;
/// ---------------------------
///// === DATA MODELS ===
/// ---------------------------

/// Normalize a configured repository path string into an absolute PathBuf.
///
/// Behavior:
/// - "~/<rest>" expands to $HOME/<rest>
/// - If the input is already absolute (starts with '/'), it's returned as-is.
/// - If the input starts with "./", the "./" is stripped and a leading '/' is added,
///   so "./Users/..." -> "/Users/...".
/// - If the input doesn't start with '/' or '~', treat it as an absolute path by
///   prefixing a leading '/', so "Users/..." -> "/Users/...".

fn normalize_repo_path<P: AsRef<str>>(s: P) -> PathBuf {
    let s = s.as_ref().trim();
    if s.is_empty() {
        // fallback to current dir (shouldn't happen for your config)
        return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    }

    // Expand leading ~/
    if s.starts_with("~") {
        if s == "~" {
            if let Ok(home) = env::var("HOME") {
                return PathBuf::from(home);
            }
        } else if s.starts_with("~/") {
            if let Ok(home) = env::var("HOME") {
                return PathBuf::from(home).join(&s[2..]);
            }
        }
    }

    let p = PathBuf::from(s);

    // Already absolute: /something
    if p.is_absolute() {
        return p;
    }

    // If user gave "./whatever" -> treat as "/whatever"
    if s.starts_with("./") {
        let trimmed = &s[2..];
        return PathBuf::from("/").join(trimmed);
    }

    // Default: prefix with leading '/' to treat as absolute
    PathBuf::from("/").join(s)
}

fn data_file_name() -> String {
    // produces "2025.json", "2026.json", etc.
    format!("{}.json", Local::now().year())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalEntry {
    time: String, // "HH:MM"
    text: String,
    /// New field: tags. Defaulted so older JSON without this field still deserializes.
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Task {
    id: u64,
    text: String,
    created_at: String, // ISO
    #[serde(default)]
    completed_at: Option<String>,
    /// New field: tags. Defaulted so older JSON without this field still deserializes.
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DataFile {
    /// Default the fields so entire file can be missing them and still load.
    #[serde(default)]
    journal: BTreeMap<String, Vec<JournalEntry>>,
    #[serde(default)]
    tasks: Vec<Task>,
}

/// ---------------------------
///// === APP STATE ===
/// ---------------------------

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Focus {
    Entry,
    Tasks,
    Search,
    Journal,
}

impl Focus {
    /// NEXT follows ENTRY -> TASKS -> SEARCH -> JOURNAL -> ENTRY ...
    fn next(self) -> Focus {
        match self {
            Focus::Entry => Focus::Tasks,
            Focus::Tasks => Focus::Search,
            Focus::Search => Focus::Journal,
            Focus::Journal => Focus::Entry,
        }
    }
    fn prev(self) -> Focus {
        match self {
            Focus::Entry => Focus::Journal,
            Focus::Tasks => Focus::Entry,
            Focus::Search => Focus::Tasks,
            Focus::Journal => Focus::Search,
        }
    }
}

struct AppState {
    data: DataFile,
    focus: Focus,
    entry_buffer: String,
    journal_scroll: u16,
    tasks_scroll: usize,
    search_input: String,
    search_results: Vec<(String, JournalEntry)>,
    search_scroll: usize,
    search_selected_detail: Option<(String, JournalEntry)>,
    next_task_id: u64,
    data_file_path: PathBuf,
    repo_path: PathBuf,
    location: String, // session location, stored as tag only
}

impl AppState {
    fn load(repo_path: impl AsRef<Path>, data_filename: &str, location: String) -> Result<Self> {
        let repo_path = repo_path.as_ref().to_owned();
        let data_file_path = repo_path.join(data_filename);

        fs::create_dir_all(&repo_path)
            .with_context(|| format!("couldn't create repository dir {:?}", repo_path))?;

        // Read file if present, otherwise create default JSON.
        let data: DataFile = if data_file_path.exists() {
            let mut f = File::open(&data_file_path)
                .with_context(|| format!("failed to open data file {:?}", data_file_path))?;
            let mut s = String::new();
            f.read_to_string(&mut s).with_context(|| "failed to read data file")?;
            // serde will supply defaults for missing fields (tags etc.)
            serde_json::from_str(&s).with_context(|| "failed to parse JSON data file")?
        } else {
            let df = DataFile::default();
            let s = serde_json::to_string_pretty(&df).context("serialize default datafile")?;
            let mut f = File::create(&data_file_path)
                .with_context(|| format!("failed to create data file {:?}", data_file_path))?;
            f.write_all(s.as_bytes()).with_context(|| "failed to write default data file")?;
            df
        };

        // compute next_task_id from existing tasks
        let mut next_task_id = 1u64;
        for t in &data.tasks {
            if t.id >= next_task_id {
                next_task_id = t.id + 1;
            }
        }

        Ok(Self {
            data,
            focus: Focus::Entry,
            entry_buffer: String::new(),
            journal_scroll: 0,
            tasks_scroll: 0,
            search_input: String::new(),
            search_results: vec![],
            search_scroll: 0,
            search_selected_detail: None,
            next_task_id,
            data_file_path,
            repo_path,
            location,
        })
    }

    fn persist(&mut self) -> Result<()> {
    // If the year changed since we started, switch to the new year's file.
    let expected = self.repo_path.join(data_file_name());
    if expected != self.data_file_path {
        // ensure repo dir exists
        fs::create_dir_all(&self.repo_path)
            .with_context(|| format!("couldn't create repository dir {:?}", self.repo_path))?;
        // update path — we'll write `self.data` into the new file below
        self.data_file_path = expected;
    }

    // Write to a temp file and atomically rename
    let tmp = self.data_file_path.with_extension("json.tmp");
    let mut f = File::create(&tmp).with_context(|| format!("failed to create temp file {:?}", tmp))?;
    let s = serde_json::to_string_pretty(&self.data).context("failed to serialize data file")?;
    f.write_all(s.as_bytes()).with_context(|| "failed writing serialized data to temp file")?;
    fs::rename(&tmp, &self.data_file_path).with_context(|| "failed to rename temp data file")?;
    Ok(())
}


    /// Add journal entry; tags param used to add tags (e.g. location)
    fn add_entry_for_now(&mut self, text: String, tags: Vec<String>) -> Result<()> {
        let now = Local::now();
        let date_key = now.format("%Y_%m_%d").to_string();
        let time_str = now.format("%H:%M").to_string();
        let entry = JournalEntry {
            time: time_str,
            text,
            tags,
        };
        self.data.journal.entry(date_key).or_default().push(entry);
        Ok(())
    }

    /// Add task (uses next_task_id, increments it)
    fn add_task(&mut self, text: String, tags: Vec<String>) -> Result<()> {
        let now = Local::now();
        let t = Task {
            id: self.next_task_id,
            text,
            created_at: now.format("%Y-%m-%dT%H:%M:%S").to_string(),
            completed_at: None,
            tags,
        };
        self.next_task_id += 1;
        self.data.tasks.push(t);
        Ok(())
    }

    /// Complete a task: set completed_at and add DONE entry with same tags
    /// (does not check idempotence here — that is handled at call site)
    fn complete_task(&mut self, idx: usize) -> Result<()> {
        if idx >= self.data.tasks.len() {
            return Ok(());
        }
        let now = Local::now();
        if let Some(task) = self.data.tasks.get_mut(idx) {
            task.completed_at = Some(now.format("%Y-%m-%dT%H:%M:%S").to_string());
            let done_text = format!("DONE: {}", task.text);
            let tags = task.tags.clone();
            self.add_entry_for_now(done_text, tags)?;
        }
        Ok(())
    }

    /// Ensure there is one Clock-in/Clock-out entry for today.
    /// If a "Clock-in:" entry already exists for today, do nothing.
    /// Otherwise create one using current time as CLOCK_IN and CLOCK_OUT = CLOCK_IN + (9h on Mon/Tue else 8h).
    fn ensure_daily_clock_entry(&mut self) -> Result<()> {
        let today_key = Local::now().format("%Y_%m_%d").to_string();

        if let Some(entries) = self.data.journal.get(&today_key) {
            for e in entries {
                if e.text.starts_with("Clock-in:") {
                    // already present -> nothing to do
                    return Ok(());
                }
            }
        }

        // Not present -> create it now
        let now = Local::now();
        let clock_in = now.format("%H:%M").to_string();
        let weekday = now.weekday();
        let hours_to_add = if weekday == chrono::Weekday::Mon || weekday == chrono::Weekday::Tue {
            9
        } else {
            8
        };
        let clock_out_dt = now + chrono::Duration::hours(hours_to_add);
        let clock_out = clock_out_dt.format("%H:%M").to_string();

        let entry_text = format!("Clock-in: {}, Clock-out: {}", clock_in, clock_out);

        let mut tags = vec![];
        if !self.location.is_empty() {
            tags.push(self.location.clone());
        }

        self.add_entry_for_now(entry_text.clone(), tags)?;
        // persist and git-commit the clock entry immediately
        self.persist()?;
        if let Err(e) = self.git_commit_and_push(&format!("Add clock entry: {}", entry_text)) {
            warn!("git push failed: {:?}", e);
        }
        Ok(())
    }

    /// Remove completed tasks that were completed before today
    fn prune_old_completed_tasks(&mut self) {
        let today = Local::now().date_naive();
        self.data.tasks.retain(|t| match &t.completed_at {
            Some(ts) => {
                if let Ok(ndt) = NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S") {
                    ndt.date() >= today
                } else {
                    true
                }
            }
            None => true,
        });
    }

    /// Search matches date_key, entry.text, OR entry.tags (case-insensitive)
    fn run_search(&mut self) {
        self.search_selected_detail = None;
        let raw = self.search_input.clone();
        let terms: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        let mut results: Vec<(String, JournalEntry)> = Vec::new();
        if terms.is_empty() {
            self.search_results = results;
            self.search_scroll = 0;
            return;
        }

        for (date_key, entries) in &self.data.journal {
            for entry in entries {
                let text_lower = entry.text.to_lowercase();
                let tags_lower: Vec<String> = entry.tags.iter().map(|t| t.to_lowercase()).collect();
                let mut matched = false;
                for term in &terms {
                    if date_key.contains(term) {
                        matched = true;
                        break;
                    }
                    if text_lower.contains(term) {
                        matched = true;
                        break;
                    }
                    for tag in &tags_lower {
                        if tag.contains(term) {
                            matched = true;
                            break;
                        }
                    }
                    if matched {
                        break;
                    }
                }
                if matched {
                    results.push((date_key.clone(), entry.clone()));
                }
            }
        }

        results.sort_by(|(d1, e1), (d2, e2)| {
            let cmp_date = d2.cmp(d1);
            if cmp_date != std::cmp::Ordering::Equal {
                return cmp_date;
            }
            e2.time.cmp(&e1.time)
        });

        self.search_results = results;
        self.search_scroll = 0;
    }

    /// commit & push (best-effort)
    fn git_commit_and_push(&self, message: &str) -> Result<()> {
        if !COMMIT_AND_PUSH {
            info!("COMMIT_AND_PUSH disabled; skipping git operations.");
            return Ok(());
        }

        let repo = match Repository::open(&self.repo_path) {
            Ok(r) => r,
            Err(_) => {
                info!("Repo not found at {:?}; attempting to init.", self.repo_path);
                Repository::init(&self.repo_path).with_context(|| format!("failed to init repo at {:?}", self.repo_path))?
            }
        };

        let mut index = repo.index().with_context(|| "failed to open git index")?;
        let rel_path = if let Ok(rel) = self.data_file_path.strip_prefix(&self.repo_path) {
            rel.to_path_buf()
        } else if let Some(fname) = self.data_file_path.file_name() {
            PathBuf::from(fname)
        } else {
            self.data_file_path.clone()
        };
        index.add_path(rel_path.as_path()).with_context(|| format!("failed to add data file {:?} to index", rel_path))?;
        index.write().with_context(|| "failed to write index")?;

        let tree_oid = index.write_tree().with_context(|| "failed to write tree")?;
        let tree = repo.find_tree(tree_oid).with_context(|| "failed to find tree")?;

        let parent_commit = match repo.head() {
            Ok(head) => {
                let target = head.target().with_context(|| "head has no target")?;
                let commit = repo.find_commit(target).with_context(|| "failed to find HEAD commit")?;
                Some(commit)
            }
            Err(_) => None,
        };

        let sig = Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).with_context(|| "failed to create git signature")?;
        let commit_oid = match &parent_commit {
            Some(parent) => repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[parent]),
            None => repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[]),
        }
        .with_context(|| "failed to create git commit")?;

        info!("Created commit {}", commit_oid);

        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(|_url, username_from_url, allowed_types| {
            if allowed_types.is_user_pass_plaintext() {
                Err(git2::Error::from_str("no plaintext credentials provided"))
            } else if allowed_types.is_ssh_key() {
                Cred::ssh_key_from_agent(username_from_url.unwrap_or(AUTHOR_NAME))
            } else {
                Err(git2::Error::from_str("no supported credential types"))
            }
        });
        let mut push_options = PushOptions::new();
        push_options.remote_callbacks(callbacks);

        let mut remote = match repo.find_remote(GIT_REMOTE_NAME) {
            Ok(r) => r,
            Err(_) => repo
                .remote_anonymous(GIT_REMOTE_NAME)
                .with_context(|| format!("failed to find or create remote '{}'", GIT_REMOTE_NAME))?,
        };

        remote.connect(git2::Direction::Push).with_context(|| "failed to connect to remote for push")?;
        let refspec = format!("refs/heads/{}:refs/heads/{}", GIT_BRANCH, GIT_BRANCH);
        remote.push(&[&refspec], Some(&mut push_options)).with_context(|| "failed to push to remote")?;

        info!("Pushed commit to remote '{}'", GIT_REMOTE_NAME);
        Ok(())
    }
}

/// ---------------------------
///// === UI / Main Loop ===
/// ---------------------------

fn main() -> Result<()> {
    // Prompt location in normal mode
    println!("{}", LOGO);
    print!("LOCATION: ");
    io::Write::flush(&mut io::stdout()).ok();
    let mut location = String::new();
    io::stdin().read_line(&mut location).context("failed to read location")?;
    let location = location.trim().to_string();

    env_logger::Builder::from_default_env().format_timestamp(None).init();

    // Load application state, passing session location
    let repo_path = normalize_repo_path(GIT_REPO_PATH);
    //let mut app = AppState::load(repo_path, DATA_FILE_NAME, location).with_context(|| "failed to load application state")?;
    let data_filename = data_file_name();
    let mut app = AppState::load(repo_path.clone(), &data_filename, location).with_context(|| "failed to load application state")?;


    // Prune older completed tasks
    app.prune_old_completed_tasks();

    // Create a daily clock-in/out entry if needed (idempotent per day)
    if let Err(e) = app.ensure_daily_clock_entry() {
        warn!("Failed to create daily clock entry: {:?}", e);
        // continue running even if clock-entry creation fails
    }

    crossterm::terminal::enable_raw_mode().context("enable raw mode")?;
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    terminal.clear().ok();
    terminal.hide_cursor().ok();

    let res = run_app(&mut terminal, &mut app);

    crossterm::terminal::disable_raw_mode().ok();
    terminal.show_cursor().ok();

    if let Err(e) = res {
        error!("Application error: {:?}", e);
        Err(e)
    } else {
        Ok(())
    }
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut AppState) -> Result<()> {
    loop {
        terminal.draw(|f| ui(f, app)).context("drawing UI frame")?;

        if event::poll(Duration::from_millis(200)).context("poll events")? {
            match event::read().context("read event")? {
                CEvent::Key(key) => {
                    // Ctrl+C quits
                    if let KeyEvent { code: KeyCode::Char('c'), modifiers: KeyModifiers::CONTROL, .. } = key {
                        break;
                    }

                    // Global Tab / BackTab: only these change focus
                    if let KeyEvent { code: KeyCode::Tab, modifiers, .. } = key {
                        if modifiers.contains(KeyModifiers::SHIFT) {
                            app.focus = app.focus.prev();
                        } else {
                            app.focus = app.focus.next();
                        }
                        continue;
                    }
                    if let KeyEvent { code: KeyCode::BackTab, .. } = key {
                        app.focus = app.focus.prev();
                        continue;
                    }

                    // else dispatch to focus handler
                    handle_key_event(app, key)?;
                }
                CEvent::Paste(s) => {
                    // preserve CR/LF
                    match app.focus {
                        Focus::Entry => app.entry_buffer.push_str(&s),
                        Focus::Search => app.search_input.push_str(&s),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Handle key events in each quadrant. Focus changes only via Tab/BackTab in run_app.
fn handle_key_event(app: &mut AppState, key: KeyEvent) -> Result<()> {
    match app.focus {
        Focus::Entry => match key {
            // Shift+Enter => newline
            KeyEvent { code: KeyCode::Enter, modifiers, .. } if modifiers.contains(KeyModifiers::SHIFT) => {
                app.entry_buffer.push('\n');
            }
            // Enter (no shift) => submit
            KeyEvent { code: KeyCode::Enter, modifiers, .. } if !modifiers.contains(KeyModifiers::SHIFT) && !modifiers.contains(KeyModifiers::CONTROL) => {
                // build tags (include session location as a tag if set)
                let mut tags = vec![];
                if !app.location.is_empty() {
                    tags.push(app.location.clone());
                }
                let text = app.entry_buffer.trim().to_string();
                if !text.is_empty() {
                    if text.starts_with('*') {
                        // create task
                        app.add_task(text.trim_start_matches('*').trim().to_string(), tags.clone()).with_context(|| "failed to add task")?;
                        app.persist().with_context(|| "failed to persist after adding task")?;
                        if let Err(e) = app.git_commit_and_push(&format!("Add task")) {
                            warn!("git push failed: {:?}", e);
                        }
                    } else {
                        app.add_entry_for_now(text.clone(), tags.clone()).with_context(|| "failed to add entry")?;
                        app.persist().with_context(|| "failed to persist after adding entry")?;
                        if let Err(e) = app.git_commit_and_push(&format!("Add entry")) {
                            warn!("git push failed: {:?}", e);
                        }
                    }
                }
                app.entry_buffer.clear();
            }
            // Accept chars including capitals (ignore Ctrl combos)
            KeyEvent { code: KeyCode::Char(c), modifiers, .. } => {
                if !modifiers.contains(KeyModifiers::CONTROL) {
                    app.entry_buffer.push(c);
                }
            }
            KeyEvent { code: KeyCode::Backspace, .. } => {
                app.entry_buffer.pop();
            }
            _ => {}
        },
        Focus::Tasks => match key {
            KeyEvent { code: KeyCode::Up, .. } => {
                if app.tasks_scroll > 0 {
                    app.tasks_scroll -= 1;
                }
            }
            KeyEvent { code: KeyCode::Down, .. } => {
                if app.tasks_scroll + 1 < app.data.tasks.len() {
                    app.tasks_scroll += 1;
                }
            }
            // Enter completes selected task; focus remains in TASKS.
            // Now idempotent: if already completed, do nothing.
            KeyEvent { code: KeyCode::Enter, .. } => {
                let idx = app.tasks_scroll;
                if idx < app.data.tasks.len() {
                    // Check idempotence: only complete if not already completed.
                    if app.data.tasks[idx].completed_at.is_none() {
                        app.complete_task(idx).with_context(|| "failed to complete task")?;
                        app.persist().with_context(|| "failed to persist after completing task")?;
                        if let Err(e) = app.git_commit_and_push("Complete task") {
                            warn!("git push failed: {:?}", e);
                        }
                    } else {
                        // already completed: do nothing
                    }
                }
            }
            _ => {}
        },
        Focus::Search => match key {
            // Ctrl+g -> run search
            KeyEvent { code: KeyCode::Char('g'), modifiers, .. } if modifiers.contains(KeyModifiers::CONTROL) => {
                app.run_search();
            }
            // Enter: if input non-empty -> run search (ensures fresh searches after backspacing), else open selected result
            KeyEvent { code: KeyCode::Enter, modifiers, .. } => {
                if !app.search_input.trim().is_empty() {
                    app.run_search();
                } else {
                    if !app.search_results.is_empty() {
                        let sel = app.search_scroll.min(app.search_results.len().saturating_sub(1));
                        if let Some((dk, entry)) = app.search_results.get(sel) {
                            app.search_selected_detail = Some((dk.clone(), entry.clone()));
                        }
                    }
                }
                if modifiers.contains(KeyModifiers::SHIFT) {
                    app.search_input.push('\n');
                }
            }
            KeyEvent { code: KeyCode::Char(c), modifiers, .. } => {
                if !modifiers.contains(KeyModifiers::CONTROL) {
                    app.search_input.push(c);
                }
            }
            KeyEvent { code: KeyCode::Backspace, .. } => {
                if !app.search_input.is_empty() {
                    app.search_input.pop();
                } else {
                    app.search_selected_detail = None;
                }
            }
            KeyEvent { code: KeyCode::Up, .. } => {
                if app.search_results.is_empty() {
                } else if app.search_scroll > 0 {
                    app.search_scroll -= 1;
                }
            }
            KeyEvent { code: KeyCode::Down, .. } => {
                if !app.search_results.is_empty() {
                    let max = app.search_results.len().saturating_sub(1);
                    if app.search_scroll < max {
                        app.search_scroll += 1;
                    }
                }
            }
            _ => {}
        },
        Focus::Journal => match key {
            KeyEvent { code: KeyCode::Up, .. } => {
                if app.journal_scroll > 0 {
                    app.journal_scroll -= 1;
                }
            }
            KeyEvent { code: KeyCode::Down, .. } => {
                app.journal_scroll = app.journal_scroll.saturating_add(1);
            }
            _ => {}
        },
    }
    Ok(())
}

/// Render the UI with 3-line header (logo left, date+location on right bottom header).
/// Scrolling behavior:
/// - TASKS & SEARCH compute a view offset so the selected index is visible.
/// - JOURNAL uses app.journal_scroll as an explicit offset (clamped).
/// Render the UI with 3-line header (logo left, date+location on right bottom header).
/// Selected-item-at-top behavior for SEARCH, TASKS and JOURNAL so selection is always visible.
fn ui<B: Backend>(f: &mut Frame<B>, app: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
        .split(f.size());

    let header_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)].as_ref())
        .split(chunks[0]);

    // Prepare left and right cols for each header row
    let mut left_cols = Vec::new();
    let mut right_cols = Vec::new();
    for row in header_rows.iter() {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
            .split(*row);
        left_cols.push(cols[0]);
        right_cols.push(cols[1]);
    }

    // Render logo lines into the LEFT columns (one line per header row).
    let logo_lines: Vec<&str> = LOGO.lines().collect();
    for (i, col) in left_cols.iter().enumerate() {
        let text = logo_lines.get(i).copied().unwrap_or("");
        let p = Paragraph::new(Line::from(Span::raw(text))).block(Block::default());
        f.render_widget(p, *col);
    }

    // Render date + location into the RIGHT column of the last header row (index 2).
    let date_str = Local::now().format("%A, %e %B").to_string();
    let location_display = if app.location.is_empty() { "Unknown" } else { &app.location };
    let meta = format!("Working from {} on {}", location_display, date_str.trim());
    let meta_para = Paragraph::new(Line::from(Span::styled(
        meta,
        Style::default().add_modifier(Modifier::BOLD),
    )))
    .block(Block::default());
    if right_cols.len() >= 3 {
        f.render_widget(meta_para, right_cols[2]);
    } else if let Some(col) = right_cols.last() {
        f.render_widget(meta_para, *col);
    }

    // Body => two rows of quadrants
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
        .split(chunks[1]);

    let top_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
        .split(rows[0]);

    let bottom_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
        .split(rows[1]);

    // -----------------------
    // TASKS
    // -----------------------
    let tasks_block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled("TASKS", Style::default().add_modifier(Modifier::BOLD)))
        .border_style(if app.focus == Focus::Tasks { Style::default().fg(Color::Magenta) } else { Style::default() });

    let mut tasks_lines: Vec<Line> = Vec::new();
    if app.data.tasks.is_empty() {
        tasks_lines.push(Line::from(Span::raw("(no tasks)")));
    } else {
        let today = Local::now().date_naive();
        for (i, t) in app.data.tasks.iter().enumerate() {
            let mut s = t.text.clone();
            let mut style = Style::default();
            if let Some(ca) = &t.completed_at {
                if let Ok(ndt) = NaiveDateTime::parse_from_str(ca, "%Y-%m-%dT%H:%M:%S") {
                    if ndt.date() == today {
                        style = style.add_modifier(Modifier::DIM);
                        s = format!("✓ {}", s);
                    }
                }
            }
            if i == app.tasks_scroll && app.focus == Focus::Tasks {
                style = style.fg(Color::Yellow).add_modifier(Modifier::BOLD);
            }
            tasks_lines.push(Line::from(Span::styled(s, style)));
        }
    }
    f.render_widget(Paragraph::new(tasks_lines).block(tasks_block).wrap(Wrap { trim: false }), top_cols[0]);

    // -----------------------
    // ENTRY (top-right)
    // -----------------------
    let entry_block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled("NEW ENTRY", Style::default().add_modifier(Modifier::BOLD)))
        .border_style(if app.focus == Focus::Entry {
            Style::default().fg(Color::Green)
        } else {
            Style::default()
        });
    f.render_widget(
        Paragraph::new(Line::from(app.entry_buffer.clone()))
            .block(entry_block)
            .wrap(Wrap { trim: false }),
        top_cols[1],
    );

    // -----------------------
    // SEARCH (bottom-left) — selected result shown at top (keeps selection visible)
    // -----------------------
    let search_block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled("SEARCH", Style::default().add_modifier(Modifier::BOLD)))
        .border_style(if app.focus == Focus::Search {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        });

    let mut search_lines: Vec<Line> = Vec::new();
    // input line + spacer
    search_lines.push(Line::from(Span::styled(
        format!("> {}", app.search_input),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    search_lines.push(Line::from(Span::raw("")));
    if let Some((dk, entry)) = &app.search_selected_detail {
        // explicit detail open — show it
        search_lines.push(Line::from(Span::styled(
            format!("{} {}", dk, entry.time),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        search_lines.push(Line::from(Span::raw(&entry.text)));
        if !entry.tags.is_empty() {
            search_lines.push(Line::from(Span::raw(format!("tags: {}", entry.tags.join(", ")))));
        }
        search_lines.push(Line::from(Span::raw("")));
        search_lines.push(Line::from(Span::raw("(Press Backspace when search input empty to clear detail)")));
    } else {
        // no detail open — show selected result top + list below
        if app.search_results.is_empty() {
            search_lines.push(Line::from(Span::raw("(no results)")));
        } else {
            let sel = app.search_scroll.min(app.search_results.len() - 1);
            let (sel_date, sel_entry) = &app.search_results[sel];
            // header + full text for selected
            search_lines.push(Line::from(Span::styled(
                format!("{} {}", sel_date, sel_entry.time),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            search_lines.push(Line::from(Span::styled(
                sel_entry.text.clone(),
                Style::default().add_modifier(Modifier::UNDERLINED),
            )));
            if !sel_entry.tags.is_empty() {
                search_lines.push(Line::from(Span::raw(format!("tags: {}", sel_entry.tags.join(", ")))));
            }
            search_lines.push(Line::from(Span::raw("")));
            // full list (selected summary highlighted)
            for (i, (date, entry)) in app.search_results.iter().enumerate() {
                let mut style = Style::default();
                if app.focus == Focus::Search && i == sel {
                    style = style.fg(Color::Yellow).add_modifier(Modifier::BOLD);
                }
                search_lines.push(Line::from(Span::styled(format!("{} {} — {}", date, entry.time, entry.text), style)));
            }
        }
    }
    f.render_widget(
        Paragraph::new(search_lines).block(search_block).wrap(Wrap { trim: false }),
        bottom_cols[0],
    );

        // -----------------------
    // JOURNAL (bottom-right) — selected entry shown at top (selected index from app.journal_scroll)
    // -----------------------
    let journal_block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled("CATALOGUE", Style::default().add_modifier(Modifier::BOLD)))
        .border_style(if app.focus == Focus::Journal {
            Style::default().fg(Color::Blue)
        } else {
            Style::default()
        });

    let mut journal_lines: Vec<Line> = Vec::new();
    // Gather today's entries (reverse chronological)
    let today_key = Local::now().format("%Y_%m_%d").to_string();
    if let Some(entries) = app.data.journal.get(&today_key) {
        if entries.is_empty() {
            journal_lines.push(Line::from(Span::raw("(no entries today)")));
        } else {
            // clone entries into a local vec we can index
            let mut rev = entries.clone();
            rev.reverse();

            // selected index (safe clamp) - convert u16 -> usize for indexing
            let sel = (app.journal_scroll as usize).min(rev.len() - 1);

            // clone the selected entry so we don't borrow from `rev`
            let sel_entry = rev[sel].clone();

            // selected entry first (owned strings)
            journal_lines.push(Line::from(Span::styled(
                format!("{} {}", today_key, sel_entry.time.clone()),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            journal_lines.push(Line::from(Span::raw(sel_entry.text.clone())));
            if !sel_entry.tags.is_empty() {
                journal_lines.push(Line::from(Span::raw(format!("tags: {}", sel_entry.tags.join(", ")))));
            }
            journal_lines.push(Line::from(Span::raw("")));

            // then full list, with selected one highlighted
            for (i, e) in rev.iter().enumerate() {
                let mut style = Style::default();
                if app.focus == Focus::Journal && i == sel {
                    style = style.fg(Color::Yellow).add_modifier(Modifier::BOLD);
                }
                journal_lines.push(Line::from(Span::styled(format!("{} {}", e.time, e.text), style)));
            }
        }
    } else {
        journal_lines.push(Line::from(Span::raw("(no entries today)")));
    }

    f.render_widget(
        Paragraph::new(journal_lines).block(journal_block).wrap(Wrap { trim: false }),
        bottom_cols[1],
    );

}
