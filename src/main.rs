// src/main.rs
// Pro_logue — split SEARCH/RESULTS focus so Enter semantics are unambiguous

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
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap, List, ListItem, ListState},
    Frame, Terminal,
};
use serde::{Deserialize, Serialize};

/// ---------------------------
///// === CONFIGURATION ===
/// ---------------------------
const GIT_REPO_PATH: &str = "~/Documents/log_cold_storage/";
const COMMIT_AND_PUSH: bool = true;
const AUTHOR_NAME: &str = "Chase Núñez";
const AUTHOR_EMAIL: &str = "chasenunez@gmail.com";
const GIT_REMOTE_NAME: &str = "origin";
const GIT_BRANCH: &str = "main";
const LOG_COLD_STORAGE_NAME: &str = "deep_logue";

const LOGO: &str = r#"┏━┓┏━┓┏━┓   ╻  ┏━┓┏━╸╻ ╻┏━╸
┣━┛┣┳┛┃ ┃   ┃  ┃ ┃┃╺┓┃ ┃┣╸ 
╹  ╹┗╸┗━┛╺━╸┗━╸┗━┛┗━┛┗━┛┗━╸"#;

/// ---------------------------
///// === LAYOUT CONFIG (fractions) ===
/// ---------------------------
const TOP_ROW_FRAC: f32 = 0.5;
const BOTTOM_ROW_FRAC: f32 = 0.5;
const LEFT_COL_FRAC: f32 = 0.5;
const RIGHT_COL_FRAC: f32 = 0.5;
const HEADER_HEIGHT_LINES: u16 = 3;

fn frac_to_pct(f: f32) -> Constraint {
    let f = if f.is_finite() { f.clamp(0.0, 1.0) } else { 0.0 };
    let pct = (f * 100.0).round() as u16;
    Constraint::Percentage(pct)
}

/// ---------------------------
///// === DATA MODELS & HELPERS ===
/// ---------------------------

fn normalize_repo_path<P: AsRef<str>>(s: P) -> PathBuf {
    let s = s.as_ref().trim();
    if s.is_empty() {
        return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    }
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
    if p.is_absolute() {
        return p;
    }
    if s.starts_with("./") {
        let trimmed = &s[2..];
        return PathBuf::from("/").join(trimmed);
    }
    PathBuf::from("/").join(s)
}

fn data_file_name() -> String {
    format!("{}.json", LOG_COLD_STORAGE_NAME)//Local::now().year())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalEntry {
    time: String, // "HH:MM"
    text: String,
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
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DataFile {
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
    Search,  // input box
    Results, // results list (separate focus now)
    Journal,
}

impl Focus {
    // Next cycles: Entry -> Tasks -> Search -> Results -> Journal -> Entry ...
    fn next(self) -> Focus {
        match self {
            Focus::Entry => Focus::Tasks,
            Focus::Tasks => Focus::Search,
            Focus::Search => Focus::Results,
            Focus::Results => Focus::Journal,
            Focus::Journal => Focus::Entry,
        }
    }
    fn prev(self) -> Focus {
        match self {
            Focus::Entry => Focus::Journal,
            Focus::Tasks => Focus::Entry,
            Focus::Search => Focus::Tasks,
            Focus::Results => Focus::Search,
            Focus::Journal => Focus::Results,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum SelectionSource {
    Search,
    Journal,
}

struct AppState {
    data: DataFile,
    focus: Focus,
    entry_buffer: String,
    entry_scroll: u16,
    journal_scroll: usize,
    tasks_scroll: usize,
    search_input: String,
    search_results: Vec<(String, JournalEntry)>,
    search_scroll: usize,
    search_selected_detail: Option<(String, JournalEntry)>,
    next_task_id: u64,
    data_file_path: PathBuf,
    repo_path: PathBuf,
    location: String,

    tasks_state: ListState,
    search_state: ListState,  // state for results list
    journal_state: ListState,

    // popup
    show_selection: bool,
    selection_source: Option<SelectionSource>,
    selection_items: Vec<(String, JournalEntry)>,
    selection_state: ListState,
}

impl AppState {
    fn load(repo_path: impl AsRef<Path>, data_filename: &str, location: String) -> Result<Self> {
        let repo_path = repo_path.as_ref().to_owned();
        let data_file_path = repo_path.join(data_filename);

        fs::create_dir_all(&repo_path)
            .with_context(|| format!("couldn't create repository dir {:?}", repo_path))?;

        let data: DataFile = if data_file_path.exists() {
            let mut f = File::open(&data_file_path)
                .with_context(|| format!("failed to open data file {:?}", data_file_path))?;
            let mut s = String::new();
            f.read_to_string(&mut s).with_context(|| "failed to read data file")?;
            serde_json::from_str(&s).with_context(|| "failed to parse JSON data file")?
        } else {
            let df = DataFile::default();
            let s = serde_json::to_string_pretty(&df).context("serialize default datafile")?;
            let mut f = File::create(&data_file_path)
                .with_context(|| format!("failed to create data file {:?}", data_file_path))?;
            f.write_all(s.as_bytes()).with_context(|| "failed to write default data file")?;
            df
        };

        let mut next_task_id = 1u64;
        for t in &data.tasks {
            if t.id >= next_task_id {
                next_task_id = t.id + 1;
            }
        }

        let mut tasks_state = ListState::default();
        if !data.tasks.is_empty() {
            tasks_state.select(Some(0));
        }
        let search_state = ListState::default();
        let journal_state = ListState::default();
        let selection_state = ListState::default();

        Ok(Self {
            data,
            focus: Focus::Entry,
            entry_buffer: String::new(),
            entry_scroll: 0,
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
            tasks_state,
            search_state,
            journal_state,
            show_selection: false,
            selection_source: None,
            selection_items: vec![],
            selection_state,
        })
    }

    fn persist(&mut self) -> Result<()> {
        let expected = self.repo_path.join(data_file_name());
        if expected != self.data_file_path {
            fs::create_dir_all(&self.repo_path)
                .with_context(|| format!("couldn't create repository dir {:?}", self.repo_path))?;
            self.data_file_path = expected;
        }

        let tmp = self.data_file_path.with_extension("json.tmp");
        let mut f = File::create(&tmp).with_context(|| format!("failed to create temp file {:?}", tmp))?;
        let s = serde_json::to_string_pretty(&self.data).context("failed to serialize data file")?;
        f.write_all(s.as_bytes()).with_context(|| "failed writing serialized data to temp file")?;
        fs::rename(&tmp, &self.data_file_path).with_context(|| "failed to rename temp data file")?;
        Ok(())
    }

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
        if self.tasks_scroll >= self.data.tasks.len() {
            self.tasks_scroll = self.data.tasks.len().saturating_sub(1);
        }
        Ok(())
    }

    fn complete_task(&mut self, idx: usize) -> Result<()> {
        if idx >= self.data.tasks.len() {
            return Ok(());
        }
        let now = Local::now();
        if let Some(task) = self.data.tasks.get_mut(idx) {
            if task.completed_at.is_none() {
                task.completed_at = Some(now.format("%Y-%m-%dT%H:%M:%S").to_string());
                let done_text = format!("DONE: {}", task.text);
                let tags = task.tags.clone();
                self.add_entry_for_now(done_text, tags)?;
            }
        }
        Ok(())
    }

    fn ensure_daily_clock_entry(&mut self) -> Result<()> {
        let today_key = Local::now().format("%Y_%m_%d").to_string();

        if let Some(entries) = self.data.journal.get(&today_key) {
            for e in entries {
                if e.text.starts_with("Clock-in:") {
                    return Ok(());
                }
            }
        }

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
        self.persist()?;
        if let Err(e) = self.git_commit_and_push(&format!("Add clock entry: {}", entry_text)) {
            warn!("git push failed: {:?}", e);
        }
        Ok(())
    }

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
        if self.tasks_scroll >= self.data.tasks.len() {
            self.tasks_scroll = self.data.tasks.len().saturating_sub(1);
        }
    }

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
            self.search_state.select(None);
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
        if !self.search_results.is_empty() {
            self.search_state.select(Some(0));
        } else {
            self.search_state.select(None);
        }
    }

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

    /// Open popup from search results and sync selection.
    fn open_selection_from_search(&mut self) {
        self.show_selection = true;
        self.selection_source = Some(SelectionSource::Search);
        self.selection_items = self.search_results.clone();
        let sel = self.search_scroll.min(self.selection_items.len().saturating_sub(1));
        if !self.selection_items.is_empty() {
            self.selection_state.select(Some(sel));
            self.search_state.select(Some(self.search_scroll));
        } else {
            self.selection_state.select(None);
        }
    }

    /// Open popup from today's journal entries and sync selection.
    fn open_selection_from_journal(&mut self) {
        self.show_selection = true;
        self.selection_source = Some(SelectionSource::Journal);
        let today_key = Local::now().format("%Y_%m_%d").to_string();
        let mut items: Vec<(String, JournalEntry)> = Vec::new();
        if let Some(entries) = self.data.journal.get(&today_key) {
            let mut rev = entries.clone();
            rev.reverse();
            for e in rev {
                items.push((today_key.clone(), e));
            }
        }
        self.selection_items = items;
        let sel = self.journal_scroll.min(self.selection_items.len().saturating_sub(1));
        if !self.selection_items.is_empty() {
            self.selection_state.select(Some(sel));
            self.journal_state.select(Some(self.journal_scroll));
        } else {
            self.selection_state.select(None);
        }
    }

    fn close_selection(&mut self) {
        self.show_selection = false;
        self.selection_source = None;
        self.selection_state.select(None);
    }

    fn popup_move_selection(&mut self, delta: isize) {
        if self.selection_items.is_empty() {
            self.selection_state.select(None);
            return;
        }
        let len = self.selection_items.len() as isize;
        let cur = self.selection_state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, len - 1) as usize;
        self.selection_state.select(Some(next));
        match self.selection_source {
            Some(SelectionSource::Search) => {
                self.search_scroll = next;
                self.search_state.select(Some(self.search_scroll));
            }
            Some(SelectionSource::Journal) => {
                self.journal_scroll = next;
                self.journal_state.select(Some(self.journal_scroll));
            }
            None => {}
        }
    }
}

/// ---------------------------
///// === UI / Main Loop ===
/// ---------------------------

fn main() -> Result<()> {
    println!("{}", LOGO);
    print!("LOCATION: ");
    io::Write::flush(&mut io::stdout()).ok();
    let mut location = String::new();
    io::stdin().read_line(&mut location).context("failed to read location")?;
    let location = location.trim().to_string();

    env_logger::Builder::from_default_env().format_timestamp(None).init();

    let repo_path = normalize_repo_path(GIT_REPO_PATH);
    let data_filename = data_file_name();
    let mut app = AppState::load(repo_path.clone(), &data_filename, location).with_context(|| "failed to load application state")?;

    app.prune_old_completed_tasks();
    if let Err(e) = app.ensure_daily_clock_entry() {
        warn!("Failed to create daily clock entry: {:?}", e);
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
                    if let KeyEvent { code: KeyCode::Char('c'), modifiers: KeyModifiers::CONTROL, .. } = key {
                        break;
                    }

                    // If popup open, route keys to handler (Tab/BackTab should not change focus)
                    if app.show_selection {
                        handle_key_event(app, key)?;
                        continue;
                    }

                    // Tab/backtab change focus only when popup closed
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

                    handle_key_event(app, key)?;
                }
                CEvent::Paste(s) => {
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

/// Handle key events. If popup open, popup keys handled first (Up/Down/Esc).
fn handle_key_event(app: &mut AppState, key: KeyEvent) -> Result<()> {
    if app.show_selection {
        match key {
            KeyEvent { code: KeyCode::Esc, .. } => {
                app.close_selection();
            }
            KeyEvent { code: KeyCode::Up, .. } => {
                app.popup_move_selection(-1);
            }
            KeyEvent { code: KeyCode::Down, .. } => {
                app.popup_move_selection(1);
            }
            _ => {}
        }
        return Ok(());
    }

    match app.focus {
        Focus::Entry => match key {
            KeyEvent { code: KeyCode::Enter, modifiers, .. } if modifiers.contains(KeyModifiers::SHIFT) => {
                app.entry_buffer.push('\n');
            }
            KeyEvent { code: KeyCode::Enter, modifiers, .. } if !modifiers.contains(KeyModifiers::SHIFT) && !modifiers.contains(KeyModifiers::CONTROL) => {
                let mut tags = vec![];
                if !app.location.is_empty() {
                    tags.push(app.location.clone());
                }
                let text = app.entry_buffer.trim().to_string();
                if !text.is_empty() {
                    if text.starts_with('*') {
                        app.add_task(text.trim_start_matches('*').trim().to_string(), tags.clone()).with_context(|| "failed to add task")?;
                        app.persist().with_context(|| "failed to persist after adding task")?;
                        if let Err(e) = app.git_commit_and_push(&format!("Add task")) {
                            warn!("git push failed: {:?}", e);
                        }
                        app.tasks_scroll = app.data.tasks.len().saturating_sub(1);
                        app.tasks_state.select(Some(app.tasks_scroll));
                    } else {
                        app.add_entry_for_now(text.clone(), tags.clone()).with_context(|| "failed to add entry")?;
                        app.persist().with_context(|| "failed to persist after adding entry")?;
                        if let Err(e) = app.git_commit_and_push(&format!("Add entry")) {
                            warn!("git push failed: {:?}", e);
                        }
                    }
                }
                app.entry_buffer.clear();
                app.entry_scroll = 0;
            }
            KeyEvent { code: KeyCode::Char(c), modifiers, .. } => {
                if !modifiers.contains(KeyModifiers::CONTROL) {
                    app.entry_buffer.push(c);
                }
            }
            KeyEvent { code: KeyCode::Backspace, .. } => {
                app.entry_buffer.pop();
            }
            KeyEvent { code: KeyCode::Up, .. } => {
                if app.entry_scroll > 0 {
                    app.entry_scroll -= 1;
                }
            }
            KeyEvent { code: KeyCode::Down, .. } => {
                app.entry_scroll = app.entry_scroll.saturating_add(1).min(10_000);
            }
            _ => {}
        },
        Focus::Tasks => match key {
            KeyEvent { code: KeyCode::Up, .. } => {
                if app.tasks_scroll > 0 {
                    app.tasks_scroll -= 1;
                }
                app.tasks_state.select(Some(app.tasks_scroll));
            }
            KeyEvent { code: KeyCode::Down, .. } => {
                if app.tasks_scroll + 1 < app.data.tasks.len() {
                    app.tasks_scroll += 1;
                }
                app.tasks_state.select(Some(app.tasks_scroll));
            }
            KeyEvent { code: KeyCode::Enter, .. } => {
                let idx = app.tasks_scroll;
                if idx < app.data.tasks.len() {
                    if app.data.tasks[idx].completed_at.is_none() {
                        app.complete_task(idx).with_context(|| "failed to complete task")?;
                        app.persist().with_context(|| "failed to persist after completing task")?;
                        if let Err(e) = app.git_commit_and_push("Complete task") {
                            warn!("git push failed: {:?}", e);
                        }
                    }
                }
            }
            _ => {}
        },
        // SEARCH: input box. Enter runs a search when there is input. Focus remains on Search.
        Focus::Search => match key {
            KeyEvent { code: KeyCode::Char('g'), modifiers, .. } if modifiers.contains(KeyModifiers::CONTROL) => {
                app.run_search();
            }
            KeyEvent { code: KeyCode::Enter, modifiers, .. } => {
                if !app.search_input.trim().is_empty() {
                    app.run_search();
                    // keep focus in Search — user presses Tab to go to Results
                } else {
                    // no-op when no input (to avoid accidentally opening popup)
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
                if !app.search_results.is_empty() {
                    app.search_state.select(Some(app.search_scroll));
                }
            }
            KeyEvent { code: KeyCode::Down, .. } => {
                if !app.search_results.is_empty() {
                    let max = app.search_results.len().saturating_sub(1);
                    if app.search_scroll < max {
                        app.search_scroll += 1;
                    }
                }
                if !app.search_results.is_empty() {
                    app.search_state.select(Some(app.search_scroll));
                }
            }
            _ => {}
        },
        // RESULTS: navigation and Enter opens selection popup
        Focus::Results => match key {
            KeyEvent { code: KeyCode::Up, .. } => {
                if app.search_results.is_empty() {
                } else if app.search_scroll > 0 {
                    app.search_scroll -= 1;
                }
                if !app.search_results.is_empty() {
                    app.search_state.select(Some(app.search_scroll));
                }
            }
            KeyEvent { code: KeyCode::Down, .. } => {
                if !app.search_results.is_empty() {
                    let max = app.search_results.len().saturating_sub(1);
                    if app.search_scroll < max {
                        app.search_scroll += 1;
                    }
                }
                if !app.search_results.is_empty() {
                    app.search_state.select(Some(app.search_scroll));
                }
            }
            KeyEvent { code: KeyCode::Enter, .. } => {
                // open popup from results (only when there are results)
                if !app.search_results.is_empty() {
                    app.open_selection_from_search();
                }
            }
            KeyEvent { code: KeyCode::Char('g'), modifiers, .. } if modifiers.contains(KeyModifiers::CONTROL) => {
                // allow Ctrl+g here as well to rerun the search results (useful)
                app.run_search();
            }
            _ => {}
        },
        Focus::Journal => match key {
            KeyEvent { code: KeyCode::Up, .. } => {
                if app.journal_scroll > 0 {
                    app.journal_scroll -= 1;
                }
                app.journal_state.select(Some(app.journal_scroll));
            }
            KeyEvent { code: KeyCode::Down, .. } => {
                app.journal_scroll = app.journal_scroll.saturating_add(1);
                app.journal_state.select(Some(app.journal_scroll));
            }
            KeyEvent { code: KeyCode::Enter, .. } => {
                app.open_selection_from_journal();
            }
            _ => {}
        },
    }
    Ok(())
}

/// Render UI; ui mutably borrows app because we update/ensure ListState selections.
fn ui<B: Backend>(f: &mut Frame<B>, app: &mut AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(HEADER_HEIGHT_LINES), Constraint::Min(0)].as_ref())
        .split(f.size());

    let header_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)].as_ref())
        .split(chunks[0]);

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

    // Render logo (left header)
    let logo_lines: Vec<&str> = LOGO.lines().collect();
    for (i, col) in left_cols.iter().enumerate() {
        let text = logo_lines.get(i).copied().unwrap_or("");
        let p = Paragraph::new(Line::from(Span::raw(text))).block(Block::default());
        f.render_widget(p, *col);
    }

    // Date + location (right header, bottom header row)
    let date_str = Local::now().format("%A,%e %B").to_string();
    let location_display = if app.location.is_empty() { "Unknown" } else { &app.location };
    let meta = format!("Working from {} on {}", location_display, date_str.trim());
    let meta_para = Paragraph::new(Line::from(Span::styled(meta, Style::default().add_modifier(Modifier::BOLD))))
        .block(Block::default());
    if right_cols.len() >= 3 {
        f.render_widget(meta_para, right_cols[2]);
    } else if let Some(col) = right_cols.last() {
        f.render_widget(meta_para, *col);
    }

    // Body grid
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([frac_to_pct(TOP_ROW_FRAC), frac_to_pct(BOTTOM_ROW_FRAC)].as_ref())
        .split(chunks[1]);

    let top_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([frac_to_pct(LEFT_COL_FRAC), frac_to_pct(RIGHT_COL_FRAC)].as_ref())
        .split(rows[0]);

    let bottom_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([frac_to_pct(LEFT_COL_FRAC), frac_to_pct(RIGHT_COL_FRAC)].as_ref())
        .split(rows[1]);

    // TASKS (left top)
    let tasks_count = app.data.tasks.len();
    let tasks_title = format!("TASKS ({}/{})", app.tasks_scroll.saturating_add(1), tasks_count.max(1));
    let tasks_block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(tasks_title, Style::default().add_modifier(Modifier::BOLD)))
        .border_style(if app.focus == Focus::Tasks { Style::default().fg(Color::Magenta) } else { Style::default() });

    let mut task_items: Vec<ListItem> = Vec::new();
    if app.data.tasks.is_empty() {
        task_items.push(ListItem::new(Span::raw("(no tasks)")));
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
            task_items.push(ListItem::new(Span::styled(s, style)));
        }
    }

    let tasks_list = List::new(task_items).block(tasks_block).highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
    if !app.data.tasks.is_empty() {
        app.tasks_state.select(Some(app.tasks_scroll));
    } else {
        app.tasks_state.select(None);
    }
    f.render_stateful_widget(tasks_list, top_cols[0], &mut app.tasks_state);

    // ENTRY (right top)
    let entry_block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled("NEW ENTRY", Style::default().add_modifier(Modifier::BOLD)))
        .border_style(if app.focus == Focus::Entry { Style::default().fg(Color::Green) } else { Style::default() });

    let entry_para = Paragraph::new(Line::from(app.entry_buffer.clone()))
        .block(entry_block)
        .wrap(Wrap { trim: false })
        .scroll((app.entry_scroll, 0));
    f.render_widget(entry_para, top_cols[1]);

    // SEARCH (left bottom)
    let search_cols = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
        .split(bottom_cols[0]);

    // Make input block highlight when Search focus is active
    let search_block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            format!("SEARCH (results: {})", app.search_results.len()),
            Style::default().add_modifier(Modifier::BOLD),
        ))
        .border_style(if app.focus == Focus::Search { Style::default().fg(Color::Cyan) } else { Style::default() });

    let mut input_lines: Vec<Line> = Vec::new();
    if let Some((dk, entry)) = &app.search_selected_detail {
        input_lines.push(Line::from(Span::styled(format!("{} {}", dk, entry.time), Style::default().add_modifier(Modifier::BOLD))));
        input_lines.push(Line::from(Span::raw(&entry.text)));
        if !entry.tags.is_empty() {
            input_lines.push(Line::from(Span::raw(format!("tags: {}", entry.tags.join(", ")))));
        }
        input_lines.push(Line::from(Span::raw("(Backspace (when input empty) clears detail)")));
    } else {
        input_lines.push(Line::from(Span::styled(format!("> {}", app.search_input), Style::default().add_modifier(Modifier::BOLD))));
        input_lines.push(Line::from(Span::raw("Press Enter (when input present) to run search. Tab -> RESULTS to open selected result with Enter")));
        input_lines.push(Line::from(Span::raw("")));
    }
    let input_para = Paragraph::new(input_lines).block(search_block.clone());
    f.render_widget(input_para, search_cols[0]);

    // RESULTS list (now its own focus). Border highlights when Focus::Results.
    let results_block = Block::default()
        .borders(Borders::ALL)
        .title("RESULTS")
        .border_style(if app.focus == Focus::Results { Style::default().fg(Color::Cyan) } else { Style::default() });

    let mut search_items: Vec<ListItem> = Vec::new();
    if app.search_selected_detail.is_some() {
        search_items.push(ListItem::new(Span::raw("(detail open)")));
    } else if app.search_results.is_empty() {
        search_items.push(ListItem::new(Span::raw("(no results)")));
    } else {
        for (i, (date, entry)) in app.search_results.iter().enumerate() {
            let mut style = Style::default();
            // highlight when either Search or Results is the active focus (so user sees selection)
            if (app.focus == Focus::Search || app.focus == Focus::Results) && app.search_scroll == i {
                style = style.fg(Color::Yellow).add_modifier(Modifier::BOLD);
            }
            let text = format!("{} {} — {}", date, entry.time, entry.text);
            search_items.push(ListItem::new(Span::styled(text, style)));
        }
    }
    let search_list = List::new(search_items).block(results_block).highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
    if !app.search_results.is_empty() {
        app.search_state.select(Some(app.search_scroll));
    } else {
        app.search_state.select(None);
    }
    f.render_stateful_widget(search_list, search_cols[1], &mut app.search_state);

    // JOURNAL (right bottom)
    let journal_block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled("CATALOGUE", Style::default().add_modifier(Modifier::BOLD)))
        .border_style(if app.focus == Focus::Journal { Style::default().fg(Color::Blue) } else { Style::default() });

    let today_key = Local::now().format("%Y_%m_%d").to_string();
    let mut journal_items: Vec<ListItem> = Vec::new();
    if let Some(entries) = app.data.journal.get(&today_key) {
        if entries.is_empty() {
            journal_items.push(ListItem::new(Span::raw("(no entries today)")));
        } else {
            let mut rev = entries.clone();
            rev.reverse();
            for e in &rev {
                let text = format!("{} {}", e.time, e.text);
                journal_items.push(ListItem::new(Span::raw(text)));
            }
        }
    } else {
        journal_items.push(ListItem::new(Span::raw("(no entries today)")));
    }

    let journal_list = List::new(journal_items).block(journal_block).highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
    let journal_len = if let Some(entries) = app.data.journal.get(&today_key) { entries.len() } else { 0 };
    if journal_len == 0 {
        app.journal_state.select(None);
    } else {
        if app.journal_scroll >= journal_len {
            app.journal_scroll = journal_len.saturating_sub(1);
        }
        app.journal_state.select(Some(app.journal_scroll));
    }
    f.render_stateful_widget(journal_list, bottom_cols[1], &mut app.journal_state);

    // -----------------------
    // SELECTION POPUP: covers the ENTIRE body area (all four quadrants).
    // Popup is a two-column layout: left = list of items (stateful), right = wrapped Paragraph
    // showing the currently selected item in full (wrapped).
    // -----------------------
    if app.show_selection {
        // compute popup rect to cover the whole body area (no padding left/right)
        let area = f.size();
        let body_y = chunks[0].height; // header height lines
        // full coverage from x=0 to width, and y=body_y to bottom
        let popup_rect = Rect::new(
            0,
            body_y,
            area.width,
            area.height.saturating_sub(body_y),
        );

        // Clear background under popup (obstruct everything in body)
        f.render_widget(Clear, popup_rect);

        // Popup outer block
        let popup_block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled("SELECTION", Style::default().add_modifier(Modifier::BOLD)))
            .border_style(Style::default().fg(Color::LightBlue));
        f.render_widget(popup_block, popup_rect);

        // inner area inset by 1 for border
        let inner = Rect {
            x: popup_rect.x + 1,
            y: popup_rect.y + 1,
            width: popup_rect.width.saturating_sub(2),
            height: popup_rect.height.saturating_sub(2),
        };

        // Split inner horizontally: left list (~35%), right full-text (~65%)
        let inner_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)].as_ref())
            .split(inner);

        // Left: list of items (short preview). Use selection_state for this.
        let mut sel_items: Vec<ListItem> = Vec::new();
        if app.selection_items.is_empty() {
            sel_items.push(ListItem::new(Span::raw("(no items)")));
            app.selection_state.select(None);
        } else {
            for (i, (date, entry)) in app.selection_items.iter().enumerate() {
                let preview = format!("{} {} — {}", date, entry.time, entry.text);
                if let Some(sel) = app.selection_state.selected() {
                    if sel == i {
                        sel_items.push(ListItem::new(Span::styled(preview, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))));
                        continue;
                    }
                }
                sel_items.push(ListItem::new(Span::raw(preview)));
            }
        }

        let sel_list = List::new(sel_items)
            .block(Block::default().borders(Borders::ALL).title("Items"))
            .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));

        // ensure selection state valid
        if app.selection_items.is_empty() {
            app.selection_state.select(None);
        } else if app.selection_state.selected().is_none() {
            match app.selection_source {
                Some(SelectionSource::Search) => {
                    let idx = app.search_scroll.min(app.selection_items.len().saturating_sub(1));
                    app.selection_state.select(Some(idx));
                }
                Some(SelectionSource::Journal) => {
                    let idx = app.journal_scroll.min(app.selection_items.len().saturating_sub(1));
                    app.selection_state.select(Some(idx));
                }
                None => {
                    app.selection_state.select(Some(0));
                }
            }
        }

        f.render_stateful_widget(sel_list, inner_chunks[0], &mut app.selection_state);

        // Right: full wrapped text of the currently selected item
        let selected_idx = app.selection_state.selected().unwrap_or(0);
        let right_para_text = if app.selection_items.is_empty() {
            "(no item selected)".to_string()
        } else {
            let (_date, entry) = &app.selection_items[selected_idx];
            // compose full display text — multi-line allowed and will wrap
            let mut s = String::new();
            s.push_str(&format!("{} {}\n\n", entry.time, entry.text));
            if !entry.tags.is_empty() {
                s.push_str(&format!("Tags: {}\n", entry.tags.join(", ")));
            }
            s
        };

        let right_para = Paragraph::new(Line::from(right_para_text))
            .block(Block::default().borders(Borders::ALL).title("Full text"))
            .wrap(Wrap { trim: false });
        f.render_widget(right_para, inner_chunks[1]);
    }
}
