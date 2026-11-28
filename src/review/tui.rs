//! my-job-tool のタブと選択状態の分離を端末向けに実装する。既存画面は egui 専用のため描画コードは共有しない。
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{self, Stdout},
    sync::mpsc::{self, Receiver, TryRecvError},
    time::Duration,
};

use anyhow::{Context, Result};
#[cfg(target_os = "macos")]
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::{
    cursor::Show,
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
    Frame, Terminal,
};

use super::{
    git::{Comparison, LineKind, RefChoice, Repository},
    highlight::{self, Spans},
    references::{FileReference, ReferenceIndex},
    viewed::ViewedStore,
};

struct Screen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Screen {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("端末の入力モードを変更できません")?;
        let mut stdout = io::stdout();
        let enter = (|| -> io::Result<()> {
            execute!(stdout, EnterAlternateScreen)?;
            // Request macOS Command keys without waiting for a capability query.
            #[cfg(target_os = "macos")]
            execute!(
                stdout,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
            execute!(stdout, EnableMouseCapture)
        })();
        if let Err(error) = enter {
            Self::leave(&mut stdout);
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                Self::leave(&mut io::stdout());
                let _ = disable_raw_mode();
                Err(error.into())
            }
        }
    }

    fn leave(stdout: &mut impl io::Write) {
        // Restore keyboard mode before leaving its alternate-screen stack. Keep cleanup
        // attempts separate so an unsupported command cannot prevent terminal recovery.
        #[cfg(target_os = "macos")]
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen, Show);
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        Self::leave(self.terminal.backend_mut());
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Files,
    Diff,
}

enum CodeView {
    Diff,
    FullDiff,
    Source,
}

struct Picker {
    target: usize,
    query: String,
    selected: usize,
}

mod all_files;
mod file_browser;
mod file_tree;
use file_tree::Row as TreeRow;
mod blame;
mod context;
mod cursor;
mod mouse;
mod mouse_ui;
use mouse_ui::MouseAction;
mod result_preview;
mod symbols;
use crate::review::symbols::SymbolMode;
use all_files::AllFiles;
use symbols::{SymbolPicker, Symbols};

struct Source {
    commit: String,
    symbols: Option<Box<Symbols>>,
    all_files: Option<Box<AllFiles>>,
    return_focus: Option<Focus>,
    from_browser: bool,
    path: String,
    lines: Vec<String>,
    offset: usize,
    matched: usize,
    horizontal: u16,
    colors: Option<Vec<Spans>>,
    colors_job: Option<Job<Option<Vec<Spans>>>>,
    cursor: cursor::CodeCursor,
}

impl Source {
    fn new(commit: String, path: String, line: u32, text: String) -> Self {
        let lines: Vec<String> = text.lines().map(clean).collect();
        let matched = (line.saturating_sub(1) as usize).min(lines.len().saturating_sub(1));
        let colors_job = if highlight::supports(&path) {
            let worker_path = path.clone();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(Ok(highlight::source(&worker_path, &text)));
            });
            Some(rx)
        } else {
            None
        };
        Self {
            commit,
            symbols: None,
            all_files: None,
            return_focus: None,
            from_browser: false,
            path,
            lines,
            offset: matched,
            matched,
            horizontal: 0,
            colors: None,
            colors_job,
            cursor: Default::default(),
        }
    }
}

enum Overlay {
    SymbolPicker(Box<SymbolPicker>),
    Symbols(Box<Symbols>),
    AllFiles(Box<AllFiles>),
    None,
    Help(u16),
    Search(String),
    Branches(Picker),
    References { tab: usize, selected: usize },
    Source(Source),
}

type Job<T> = Receiver<std::result::Result<T, String>>;

// Commit identities prevent a completed job from coloring another comparison.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ColorKey {
    base: String,
    head: String,
    path: String,
    digest: String,
}

type DiffColors = Vec<Option<Spans>>;

const DEFAULT_FILE_PANE_PERCENT: u16 = 15;
const MIN_FILE_PANE_WIDTH: u16 = 14;
const FILE_PANE_PERCENT_RANGE: std::ops::RangeInclusive<u16> = 10..=60;

struct App {
    repository: Repository,
    comparison: Comparison,
    viewed: ViewedStore,
    file_browser: file_browser::FileBrowser,
    selected: usize,
    only_unviewed: bool,
    collapsed: HashSet<String>,
    folder: Option<String>,
    focus: Focus,
    diff_offset: usize,
    cursor: cursor::CodeCursor,
    horizontal: u16,
    query: String,
    overlay: Overlay,
    error: Option<String>,
    // Retain the clipboard owner while the TUI runs, including on Linux.
    clipboard: Option<arboard::Clipboard>,
    refs: Vec<RefChoice>,
    refs_job: Option<Job<Vec<RefChoice>>>,
    indexes: HashMap<String, ReferenceIndex>,
    index_job: Option<(String, Job<ReferenceIndex>)>,
    index_error: Option<String>,
    page_height: usize,
    help_max: u16,
    help_page: u16,
    history: blame::History,
    colors: VecDeque<(ColorKey, DiffColors)>,
    result_preview: result_preview::Preview,
    colors_job: Option<(ColorKey, Job<DiffColors>)>,
    code_viewport: std::cell::RefCell<Option<mouse::CodeViewport>>,
    mouse_down: Option<(u16, u16, mouse::MouseHit)>,
    expanded: Option<context::Expanded>,
    context_job: Option<Job<context::Expanded>>,
    context_attempt: Option<ColorKey>,
    context_history: HashMap<ColorKey, context::OpenedContext>,
    mouse_targets: std::cell::RefCell<Vec<(Rect, MouseAction)>>,
    mouse_panes: [Rect; 2],
    file_pane_percent: u16,
    exit_requested: bool,
}

pub(super) fn run(
    repository: Repository,
    comparison: Comparison,
    viewed: ViewedStore,
) -> Result<()> {
    let mut app = App {
        repository,
        comparison,
        viewed,
        file_browser: Default::default(),
        selected: 0,
        only_unviewed: false,
        collapsed: HashSet::new(),
        folder: None,
        focus: Focus::Files,
        diff_offset: 0,
        cursor: Default::default(),
        horizontal: 0,
        query: String::new(),
        overlay: Overlay::None,
        error: None,
        clipboard: None,
        refs: Vec::new(),
        refs_job: None,
        indexes: HashMap::new(),
        index_job: None,
        index_error: None,
        page_height: 10,
        help_max: 0,
        help_page: 1,
        history: Default::default(),
        colors: VecDeque::new(),
        result_preview: Default::default(),
        colors_job: None,
        code_viewport: Default::default(),
        mouse_down: None,
        expanded: None,
        context_job: None,
        context_attempt: None,
        context_history: HashMap::new(),
        mouse_targets: Default::default(),
        mouse_panes: Default::default(),
        file_pane_percent: DEFAULT_FILE_PANE_PERCENT,
        exit_requested: false,
    };
    app.select_first_tree_file();
    let mut screen = Screen::enter()?;
    let no_color = std::env::var("NO_COLOR").is_ok_and(|value| !value.is_empty());
    let mut dirty = true;
    loop {
        dirty |= app.poll_jobs();
        if dirty {
            screen.terminal.draw(|frame| {
                app.draw(frame);
                if no_color {
                    // Crossterm's empty color commands can reset reverse video.
                    for cell in &mut frame.buffer_mut().content {
                        cell.set_fg(Color::Reset).set_bg(Color::Reset);
                    }
                }
            })?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if app.key(key) {
                        break;
                    }
                    dirty = true;
                }
                Event::Mouse(mouse) => {
                    dirty |= app.mouse(mouse);
                    if app.exit_requested {
                        break;
                    }
                }
                Event::Resize(_, _) => {
                    app.mouse_down = None;
                    dirty = true;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

impl App {
    fn visible(&self) -> Vec<usize> {
        let path_query = self.file_browser.query.to_lowercase();
        self.comparison
            .files
            .iter()
            .enumerate()
            .filter(|(_, file)| {
                self.file_browser.active
                    || ((!self.only_unviewed || !self.viewed.is_viewed(&self.comparison, file))
                        && (path_query.is_empty()
                            || clean(file.path()).to_lowercase().contains(&path_query)))
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn current(&self) -> Option<usize> {
        self.visible().get(self.selected).copied()
    }

    fn repository_copy(&self) -> Repository {
        Repository {
            root: self.repository.root.clone(),
            identity: self.repository.identity.clone(),
        }
    }

    fn start_refs(&mut self) {
        if self.refs_job.is_some() || !self.refs.is_empty() {
            return;
        }
        let repository = self.repository_copy();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(repository.refs().map_err(|error| format!("{error:#}")));
        });
        self.refs_job = Some(rx);
    }

    fn start_index(&mut self) {
        let Some(file_index) = self.current() else {
            return;
        };
        let (commit, _) = self.comparison.files[file_index].source(&self.comparison);
        if self.indexes.contains_key(commit)
            || self
                .index_job
                .as_ref()
                .is_some_and(|(key, _)| key == commit)
        {
            return;
        }
        let commit = commit.to_owned();
        let worker_commit = commit.clone();
        let repository = self.repository_copy();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = ReferenceIndex::load(&repository, &worker_commit)
                .map_err(|error| format!("{error:#}"));
            let _ = tx.send(result);
        });
        self.index_error = None;
        self.index_job = Some((commit, rx));
    }

    fn color_key(&self, index: usize) -> ColorKey {
        let file = self.display_file(index);
        ColorKey {
            base: self.comparison.merge_base.clone(),
            head: self.comparison.head_commit.clone(),
            path: file.path().to_owned(),
            digest: file.digest.clone(),
        }
    }

    fn start_colors(&mut self) {
        if self.colors_job.is_some() {
            return;
        }
        let Some(index) = self.current() else { return };
        let file = self.display_file(index);
        if !highlight::supports(&file.old_path) && !highlight::supports(&file.new_path) {
            return;
        }
        let key = self.color_key(index);
        if let Some(position) = self.colors.iter().position(|(cached, _)| cached == &key) {
            // Refresh recency without reparsing when returning to a file.
            let cached = self.colors.remove(position).unwrap();
            self.colors.push_back(cached);
            return;
        }
        let file = file.clone();
        let repository = self.repository_copy();
        let worker_key = key.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let colors = highlight::diff(&repository, &worker_key.base, &worker_key.head, &file);
            let _ = tx.send(Ok(colors));
        });
        self.colors_job = Some((key, rx));
    }

    fn poll_jobs(&mut self) -> bool {
        let mut changed = self.poll_context();
        changed |= self.poll_blame();
        if let Some(result) = self.refs_job.as_ref().and_then(job_result) {
            changed = true;
            self.refs_job = None;
            match result {
                Ok(refs) => self.refs = refs,
                Err(error) => self.error = Some(format!("参照一覧を読み込めません: {error}")),
            }
        }
        if let Some(result) = self.index_job.as_ref().and_then(|(_, job)| job_result(job)) {
            changed = true;
            if let Some((commit, _)) = self.index_job.take() {
                match result {
                    Ok(index) => {
                        self.indexes.insert(commit, index);
                    }
                    Err(error) => self.index_error = Some(error),
                }
            }
        }
        if let Some(result) = self
            .colors_job
            .as_ref()
            .and_then(|(_, job)| job_result(job))
        {
            if let Some((key, _)) = self.colors_job.take() {
                if key.base == self.comparison.merge_base && key.head == self.comparison.head_commit
                {
                    // Cache failure as plain text too, so redraws never retry a failed job.
                    self.colors.push_back((key, result.unwrap_or_default()));
                    while self.colors.len() > 6 {
                        self.colors.pop_front();
                    }
                    changed = true;
                }
            }
        }
        if let Overlay::Source(source) = &mut self.overlay {
            if let Some(result) = source.colors_job.as_ref().and_then(job_result) {
                source.colors_job = None;
                source.colors = result.ok().flatten();
                changed = true;
            }
        }
        if matches!(self.overlay, Overlay::Symbols(_)) {
            changed |= self.poll_symbols();
        }
        if matches!(self.overlay, Overlay::AllFiles(_)) {
            changed |= self.poll_all_files();
        }
        changed |= self.poll_file_browser();
        changed |= self.poll_result_preview();
        self.start_colors();
        changed
    }

    fn reset_position(&mut self) {
        self.diff_offset = 0;
        self.horizontal = 0;
        self.cursor = Default::default();
    }

    fn move_file(&mut self, delta: isize) {
        let order: Vec<usize> = self
            .tree_rows(&HashSet::new())
            .iter()
            .filter_map(|row| {
                if let TreeRow::File { index, .. } = row {
                    Some(*index)
                } else {
                    None
                }
            })
            .collect();
        let position = self
            .current()
            .and_then(|index| order.iter().position(|i| *i == index))
            .unwrap_or(0);
        if let Some(&index) = order.get(shifted(position, delta, order.len())) {
            self.selected = self.visible().iter().position(|i| *i == index).unwrap_or(0);
            self.reveal_file(index);
        }
        self.reset_position();
    }

    fn diff_length(&self) -> usize {
        self.current()
            .map_or(0, |index| self.display_file(index).lines.len())
    }

    fn search(&mut self, backwards: bool, include_current: bool) {
        let Some(index) = self.current() else { return };
        if self.query.is_empty() {
            self.error = Some("検索語が空です。/ で入力してください".into());
            return;
        }
        let lines = &self.display_file(index).lines;
        let matches: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                line.kind != LineKind::Header && clean(&line.text).contains(&self.query)
            })
            .map(|(index, _)| index)
            .collect();
        let found = if backwards {
            matches
                .iter()
                .rev()
                .find(|&&line| line < self.diff_offset)
                .or_else(|| matches.last())
        } else {
            matches
                .iter()
                .find(|&&line| {
                    line > self.diff_offset || (include_current && line == self.diff_offset)
                })
                .or_else(|| matches.first())
        };
        if let Some(&offset) = found {
            self.diff_offset = offset;
            self.horizontal = 0;
            let text = clean(&self.display_file(index).lines[offset].text);
            self.cursor = Default::default();
            self.cursor.position = Some(cursor::Position {
                row: offset,
                byte: text.find(&self.query).unwrap_or(0),
            });
        } else {
            self.error = Some(format!("「{}」は見つかりません", self.query));
        }
    }

    fn inline_code(&self) -> bool {
        match &self.overlay {
            Overlay::Source(source) => source.symbols.is_none(),
            Overlay::SymbolPicker(picker) => picker
                .origin
                .as_ref()
                .is_none_or(|source| source.symbols.is_none()),
            _ => false,
        }
    }

    fn key(&mut self, key: KeyEvent) -> bool {
        self.key_with_copy_selection(key, Self::copy_cursor_selection)
    }

    fn key_with_copy_selection(
        &mut self,
        key: KeyEvent,
        copy: impl FnOnce(&mut Self, &mut cursor::CodeCursor),
    ) -> bool {
        self.mouse_down = None;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return true;
        }
        self.error = None;
        if key.modifiers == KeyModifiers::ALT && self.can_resize_panes() {
            match key.code {
                KeyCode::Left | KeyCode::Right => {
                    let step = if key.code == KeyCode::Left { -5 } else { 5 };
                    self.set_file_pane_percent(self.file_pane_percent.saturating_add_signed(step));
                    return false;
                }
                _ => {}
            }
        }
        if self.file_browser.editing {
            self.path_filter_key(key);
            return false;
        }
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::SUPER {
            self.command_copy_selection(copy);
            return false;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p') {
            self.open_all_files();
            return false;
        }
        if self.focus == Focus::Files
            && key.modifiers == KeyModifiers::NONE
            && (matches!(self.overlay, Overlay::None) || self.browser_source())
        {
            if key.code == KeyCode::Char('f') {
                self.toggle_file_browser();
                return false;
            }
            if key.code == KeyCode::Char('p') {
                self.start_path_filter();
                return false;
            }
            if self.file_browser.active && self.file_browser_key(key.code) {
                return false;
            }
        }
        if self.code_cursor_key(key) {
            return false;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
            && matches!(self.overlay, Overlay::None | Overlay::Source(_))
        {
            self.cursor.anchor = None;
            self.cursor.cancel_pending();
            if let Overlay::Source(s) = &mut self.overlay {
                s.cursor.anchor = None;
                s.cursor.cancel_pending();
            }
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) && self.inline_code() {
            if self.browser_source() {
                self.focus = if self.focus == Focus::Files {
                    Focus::Diff
                } else {
                    Focus::Files
                };
            } else {
                self.overlay = Overlay::None;
                self.focus = Focus::Files;
            }
            return false;
        }
        let overlay = std::mem::replace(&mut self.overlay, Overlay::None);
        match overlay {
            Overlay::None => return self.main_key(key),
            Overlay::AllFiles(search) => self.all_files_key(key, search),
            Overlay::SymbolPicker(picker) => self.symbol_picker_key(key, picker),
            Overlay::Symbols(search) => self.symbols_key(key, search),
            Overlay::Help(mut offset) => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => return false,
                    KeyCode::Up | KeyCode::Char('k') => offset = offset.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        offset = offset.saturating_add(1).min(self.help_max)
                    }
                    KeyCode::PageUp => offset = offset.saturating_sub(self.help_page),
                    KeyCode::PageDown => {
                        offset = offset.saturating_add(self.help_page).min(self.help_max)
                    }
                    KeyCode::Home => offset = 0,
                    KeyCode::End => offset = self.help_max,
                    _ => {}
                }
                self.overlay = Overlay::Help(offset);
            }
            Overlay::Search(mut input) => {
                match key.code {
                    KeyCode::Esc => return false,
                    KeyCode::Enter => {
                        self.query = input;
                        if !self.query.is_empty() {
                            self.search(false, true);
                        }
                        return false;
                    }
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(c)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !c.is_control()
                            && input.len() < 4096 =>
                    {
                        input.push(c);
                    }
                    _ => {}
                }
                self.overlay = Overlay::Search(input);
            }
            Overlay::Branches(mut picker) => {
                let count = matching_refs(&self.refs, &picker.query).len();
                match key.code {
                    KeyCode::Esc => return false,
                    KeyCode::Tab | KeyCode::BackTab => {
                        picker.target = 1 - picker.target;
                    }
                    KeyCode::Up => picker.selected = shifted(picker.selected, -1, count),
                    KeyCode::Down => picker.selected = shifted(picker.selected, 1, count),
                    KeyCode::Backspace => {
                        picker.query.pop();
                        picker.selected = 0;
                    }
                    KeyCode::Char(c)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !c.is_control()
                            && picker.query.len() < 4096 =>
                    {
                        picker.query.push(c);
                        picker.selected = 0;
                    }
                    KeyCode::Enter => {
                        if let Some(choice) =
                            matching_refs(&self.refs, &picker.query).get(picker.selected)
                        {
                            let (base, head) = if picker.target == 0 {
                                (choice.revision.as_str(), self.comparison.head.as_str())
                            } else {
                                (self.comparison.base.as_str(), choice.revision.as_str())
                            };
                            match self.repository.compare(base, head) {
                                Ok(comparison) => {
                                    self.comparison = comparison;
                                    self.colors.clear();
                                    self.select_first_tree_file();
                                    self.reset_position();
                                    self.refresh_file_browser();
                                    return false;
                                }
                                Err(error) => {
                                    self.error = Some(format!("比較を変更できません: {error:#}"))
                                }
                            }
                        }
                    }
                    _ => {}
                }
                self.overlay = Overlay::Branches(picker);
            }
            Overlay::References {
                mut tab,
                mut selected,
            } => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return false,
                    KeyCode::Tab | KeyCode::BackTab => {
                        tab = 1 - tab;
                        selected = 0;
                    }
                    KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = shifted(selected, 1, self.references(tab).len());
                    }
                    KeyCode::Enter => {
                        if let Some(reference) = self.references(tab).get(selected) {
                            if let Some(file) = self.current() {
                                let (commit, _) =
                                    self.comparison.files[file].source(&self.comparison);
                                match self.repository.file_text(commit, &reference.path) {
                                    Ok(text) => {
                                        let mut source = Source::new(
                                            commit.to_owned(),
                                            reference.path.clone(),
                                            reference.line,
                                            text,
                                        );
                                        source.return_focus = Some(self.focus);
                                        self.focus = Focus::Diff;
                                        self.overlay = Overlay::Source(source);
                                        return false;
                                    }
                                    Err(error) => {
                                        self.error = Some(format!("ソースを開けません: {error:#}"))
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
                self.overlay = Overlay::References { tab, selected };
            }
            Overlay::Source(mut source) => {
                match key.code {
                    KeyCode::Char('s') => {
                        self.open_source_symbol_picker(source);
                        return false;
                    }
                    KeyCode::Char('d') => {
                        self.open_source_symbols(source, SymbolMode::Definitions);
                        return false;
                    }
                    KeyCode::Char('R') => {
                        self.open_source_symbols(source, SymbolMode::Usages);
                        return false;
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        if source.from_browser {
                            if key.code == KeyCode::Esc || self.focus == Focus::Diff {
                                self.overlay = Overlay::Source(source);
                            }
                            self.focus = Focus::Files;
                        } else if let Some(search) = source.symbols.take() {
                            self.overlay = Overlay::Symbols(search);
                        } else if let Some(search) = source.all_files.take() {
                            if key.code == KeyCode::Esc {
                                self.restore_all_files_origin(search);
                            } else {
                                self.overlay = Overlay::AllFiles(search);
                            }
                        } else {
                            self.focus = source.return_focus.unwrap_or(Focus::Files);
                        }
                        return false;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        source.offset = shifted(source.offset, 1, source.lines.len())
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        source.offset = source.offset.saturating_sub(1)
                    }
                    KeyCode::PageDown => {
                        source.offset =
                            shifted(source.offset, self.page_height as isize, source.lines.len())
                    }
                    KeyCode::PageUp => {
                        source.offset = source.offset.saturating_sub(self.page_height)
                    }
                    KeyCode::Home => source.offset = 0,
                    KeyCode::End => {
                        source.offset = source.lines.len().saturating_sub(self.page_height)
                    }
                    KeyCode::Left | KeyCode::Char('h') => {
                        source.horizontal = source.horizontal.saturating_sub(4)
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        source.horizontal = source.horizontal.saturating_add(4)
                    }
                    _ => {}
                }
                self.overlay = Overlay::Source(source);
            }
        }
        false
    }

    fn main_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('e') => self.expand_visible_gap(false),
            KeyCode::Char('E') => self.expand_visible_gap(true),
            KeyCode::Char('a') => self.toggle_full_file(),
            KeyCode::Char('d') => self.open_diff_symbols(SymbolMode::Definitions),
            KeyCode::Char('R') => self.open_diff_symbols(SymbolMode::Usages),
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_all_files()
            }
            KeyCode::Char('q') => return true,
            KeyCode::Char('?') => self.overlay = Overlay::Help(0),
            KeyCode::Esc => self.focus = Focus::Files,
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = if self.focus == Focus::Files {
                    Focus::Diff
                } else {
                    Focus::Files
                };
            }
            KeyCode::Char('b') => {
                self.start_refs();
                self.overlay = Overlay::Branches(Picker {
                    target: 0,
                    query: String::new(),
                    selected: 0,
                });
            }
            KeyCode::Char('r') => {
                self.start_index();
                self.overlay = Overlay::References {
                    tab: 0,
                    selected: 0,
                };
            }
            KeyCode::Char('/') => self.overlay = Overlay::Search(self.query.clone()),
            KeyCode::Char('n') => self.search(false, false),
            KeyCode::Char('N') => self.search(true, false),
            KeyCode::Char('u') if !self.file_browser.active => {
                let previous = self.current();
                self.only_unviewed = !self.only_unviewed;
                let visible = self.visible();
                self.selected = previous
                    .and_then(|old| visible.iter().position(|&i| i >= old))
                    .unwrap_or_else(|| visible.len().saturating_sub(1));
                self.reconcile_tree();
                self.reset_position();
            }
            KeyCode::Char(' ') if !self.folder_focused() => {
                if let Some(index) = self.current() {
                    let file = &self.comparison.files[index];
                    let value = !self.viewed.is_viewed(&self.comparison, file);
                    match self.viewed.set_viewed(&self.comparison, file, value) {
                        Ok(()) => {
                            if self.only_unviewed && !self.file_browser.active {
                                self.selected =
                                    self.selected.min(self.visible().len().saturating_sub(1));
                                self.reconcile_tree();
                                self.reset_position();
                            }
                        }
                        Err(error) => {
                            self.error = Some(format!("確認状態を保存できません: {error:#}"))
                        }
                    }
                }
            }
            KeyCode::Char('j') if self.focus == Focus::Files => self.move_file(1),
            KeyCode::Char('k') if self.focus == Focus::Files => self.move_file(-1),
            KeyCode::Down if self.focus == Focus::Files => self.move_tree(1),
            KeyCode::Up if self.focus == Focus::Files => self.move_tree(-1),
            KeyCode::Enter if self.focus == Focus::Files => {
                if self.folder_focused() {
                    self.tree_control(None);
                } else if self.current().is_some() {
                    self.focus = Focus::Diff;
                }
            }
            KeyCode::Char('s') if self.focus == Focus::Diff => self.open_diff_symbol_picker(),
            KeyCode::Left if self.focus == Focus::Files => self.tree_control(Some(false)),
            KeyCode::Right if self.focus == Focus::Files => self.tree_control(Some(true)),
            KeyCode::Down | KeyCode::Char('j') => {
                self.diff_offset = shifted(self.diff_offset, 1, self.diff_length())
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.diff_offset = self.diff_offset.saturating_sub(1)
            }
            KeyCode::PageDown => {
                self.diff_offset = shifted(
                    self.diff_offset,
                    self.page_height as isize,
                    self.diff_length(),
                )
            }
            KeyCode::PageUp => self.diff_offset = self.diff_offset.saturating_sub(self.page_height),
            KeyCode::Home => self.diff_offset = 0,
            KeyCode::End => self.diff_offset = self.diff_length().saturating_sub(self.page_height),
            KeyCode::Left | KeyCode::Char('h') => {
                self.horizontal = self.horizontal.saturating_sub(4)
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.horizontal = self.horizontal.saturating_add(4)
            }
            _ => {}
        }
        false
    }

    fn references(&self, tab: usize) -> &[FileReference] {
        let Some(file) = self.current() else {
            return &[];
        };
        let (commit, path) = self.comparison.files[file].source(&self.comparison);
        self.indexes
            .get(commit)
            .map_or(&[], |index| index.rows(path, tab))
    }

    fn can_resize_panes(&self) -> bool {
        let [left, right] = self.mouse_panes;
        !left.is_empty()
            && !right.is_empty()
            && left.x < right.x
            && (matches!(self.overlay, Overlay::None) || self.inline_code())
    }

    fn set_file_pane_percent(&mut self, percent: u16) -> bool {
        let percent = percent.clamp(*FILE_PANE_PERCENT_RANGE.start(), *FILE_PANE_PERCENT_RANGE.end());
        let changed = self.file_pane_percent != percent;
        self.file_pane_percent = percent;
        changed
    }

    fn draw(&mut self, frame: &mut Frame) {
        self.prepare_code_cursor();
        self.start_blame();
        self.code_viewport.borrow_mut().take();
        self.mouse_targets.borrow_mut().clear();
        let area = frame.area();
        let sections = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);
        let viewed_count = self
            .comparison
            .files
            .iter()
            .filter(|file| self.viewed.is_viewed(&self.comparison, file))
            .count();
        let title = format!(
            "base: {} → head: {}\n確認 {}/{}{}",
            short_ref(&self.comparison.base),
            short_ref(&self.comparison.head),
            viewed_count,
            self.comparison.files.len(),
            if self.only_unviewed && !self.file_browser.active {
                "  [未確認のみ]"
            } else {
                ""
            }
        );
        frame.render_widget(
            Paragraph::new(title).style(Style::default().fg(Color::Cyan)),
            sections[0],
        );
        if matches!(self.overlay, Overlay::None) || self.browser_source() {
            let controls = Rect::new(sections[0].x, sections[0].y + 2, sections[0].width, 1)
                .intersection(sections[0]);
            let next_mode = if self.file_browser.active {
                "差分のみ"
            } else {
                "全ファイル"
            };
            let mut buttons = vec![
                (next_mode, MouseAction::ToggleFileBrowser),
                ("パス絞り込み", MouseAction::PathFilter),
            ];
            if matches!(self.overlay, Overlay::None) {
                buttons.extend([
                    ("比較", MouseAction::Compare),
                    ("参照", MouseAction::Key(KeyCode::Char('r'))),
                    ("差分検索", MouseAction::Key(KeyCode::Char('/'))),
                    (
                        if self.history.enabled {
                            "行の履歴:ON"
                        } else {
                            "行の履歴"
                        },
                        MouseAction::History,
                    ),
                ]);
                if let Some(expanded) = self.current().and_then(|i| self.expanded_file(i)) {
                    let label = if expanded.is_full() {
                        "差分に戻る"
                    } else {
                        "全体を表示"
                    };
                    buttons.push((label, MouseAction::Key(KeyCode::Char('a'))));
                }
            }
            buttons.push(("ファイル検索", MouseAction::AllFiles));
            self.draw_click_buttons(frame, controls, &buttons);
        }
        let panes = if area.width < 72 {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
                .split(sections[1])
        } else {
            let width = (u32::from(area.width) * u32::from(self.file_pane_percent) + 50) / 100;
            Layout::horizontal([
                Constraint::Length((width as u16).max(MIN_FILE_PANE_WIDTH)),
                Constraint::Min(0),
            ])
            .split(sections[1])
        };
        self.mouse_panes = [panes[0], panes[1]];
        let file_list = self.draw_path_filter(frame, panes[0]);
        if self.file_browser.active {
            self.draw_file_browser(frame, file_list);
        } else {
            self.draw_files(frame, file_list);
        }
        self.draw_diff(frame, panes[1]);
        if sections[2].height >= 2 {
            let status = Rect::new(sections[2].x, sections[2].y + 1, sections[2].width, 1);
            let (text, color) = self.error.as_ref().map_or_else(
                || {
                    (
                        if let Some(history) = self.blame_status() {
                            history
                        } else if self.browser_loading_source() {
                            "ソースを読み込み中…".into()
                        } else if matches!(self.overlay, Overlay::SymbolPicker(_)) {
                            "↑↓ 行 / ←→ 名前 / Enter 定義 / R 使用箇所 / Esc 戻る".into()
                        } else {
                            format!(
                                "{}   検索={} 横位置={}",
                                if self.file_browser.active && self.focus == Focus::Files {
                                    "j/k:移動 Enter:開閉/操作 p:パス絞り込み f:差分のみ"
                                } else if matches!(self.overlay, Overlay::Source(_)) {
                                    "h/j/k/l w/b/e:移動 v/V:選択 iw:単語 y/⌘C:コピー"
                                } else if self.focus == Focus::Files {
                                    "j/k:次/前のファイル p:パス絞り込み ?:ヘルプ"
                                } else {
                                    "h/j/k/l w/b/e:移動 v/V:選択 iw:単語 y/⌘C:コピー ?:ヘルプ"
                                },
                                clean(&self.query),
                                self.horizontal
                            )
                        },
                        Color::Gray,
                    )
                },
                |error| (clean(error), Color::Red),
            );
            frame.render_widget(
                Paragraph::new(text).style(Style::default().fg(color)),
                status,
            );
        }
        if matches!(self.overlay, Overlay::None) {
            self.draw_click_buttons(
                frame,
                sections[2],
                &[
                    ("確認", MouseAction::Key(KeyCode::Char(' '))),
                    ("未確認のみ", MouseAction::Key(KeyCode::Char('u'))),
                    ("前の一致", MouseAction::Key(KeyCode::Char('N'))),
                    ("次の一致", MouseAction::Key(KeyCode::Char('n'))),
                    ("ヘルプ", MouseAction::Key(KeyCode::Char('?'))),
                    ("終了", MouseAction::Key(KeyCode::Char('q'))),
                ],
            );
        }
        match &self.overlay {
            Overlay::None => {}
            Overlay::AllFiles(search) => self.draw_all_files(frame, search),
            Overlay::SymbolPicker(picker) => self.draw_symbol_picker(frame, picker, panes[1]),
            Overlay::Symbols(search) => self.draw_symbols(frame, search),
            Overlay::Help(offset) => self.draw_help(frame, *offset),
            Overlay::Search(input) => {
                let area = popup(frame.area(), 90, 30);
                frame.render_widget(Clear, area);
                let block = panel("差分内を文字列検索", true);
                let inner = block.inner(area);
                frame.render_widget(block, area);
                let regions =
                    Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
                frame.render_widget(
                    Paragraph::new(format!("/ {}\n空欄で検索を解除 / n:次 N:前", clean(input)))
                        .wrap(Wrap { trim: false }),
                    regions[0],
                );
                self.draw_click_buttons(
                    frame,
                    regions[1],
                    &[
                        ("検索", MouseAction::Key(KeyCode::Enter)),
                        ("戻る", MouseAction::Key(KeyCode::Esc)),
                    ],
                );
            }
            Overlay::Branches(picker) => self.draw_picker(frame, picker),
            Overlay::References { tab, selected } => self.draw_references(frame, *tab, *selected),
            Overlay::Source(source) => self.draw_source(frame, source, panes[1]),
        }
        if self.can_resize_panes() && panes[0].height > 2 {
            let left = panes[0];
            let grip = Rect::new(left.right() - 1, left.y + left.height / 2, 1, 1);
            frame.render_widget(Paragraph::new("↔").style(Style::default().fg(Color::Gray)), grip);
        }
        self.prepare_code_cursor();
        self.align_code_cursor();
        self.draw_code_cursor(frame);
    }

    fn draw_files(&self, frame: &mut Frame, area: Rect) {
        let visible = self.visible();
        let block = panel(
            if self.focus == Focus::Files {
                "変更ファイル [操作中]"
            } else {
                "変更ファイル"
            },
            self.focus == Focus::Files,
        );
        if visible.is_empty() {
            frame.render_widget(
                Paragraph::new(if !self.file_browser.query.is_empty() {
                    "一致するパスはありません"
                } else if self.only_unviewed {
                    "未確認ファイルはありません"
                } else {
                    "変更ファイルはありません"
                })
                .block(block),
                area,
            );
            return;
        }
        let rows = self.tree_rows(&self.collapsed);
        let items: Vec<ListItem> = rows
            .iter()
            .map(|row| {
                let (depth, label) = match row {
                    TreeRow::Dir { depth, key, label } => (
                        *depth,
                        format!(
                            "{} {}/",
                            if self.collapsed.contains(key) {
                                "▸"
                            } else {
                                "▾"
                            },
                            clean(label)
                        ),
                    ),
                    TreeRow::File { depth, index } => {
                        let file = &self.comparison.files[*index];
                        (
                            *depth,
                            format!(
                                "{} {} {}",
                                if self.viewed.is_viewed(&self.comparison, file) {
                                    "✓"
                                } else {
                                    "□"
                                },
                                file.status,
                                clean(file.path().rsplit('/').next().unwrap_or_default())
                            ),
                        )
                    }
                };
                // Keep a name visible even when a deep tree is shown in a narrow pane.
                let indent = depth
                    .saturating_mul(2)
                    .min(area.width.saturating_sub(12) as usize);
                ListItem::new(format!("{}{label}", " ".repeat(indent)))
            })
            .collect();
        let mut state = ListState::default().with_selected(self.tree_selected(&rows));
        let inner = block.inner(area);
        frame.render_stateful_widget(
            List::new(items)
                .block(block)
                .highlight_style(selected_style()),
            area,
            &mut state,
        );
        if matches!(self.overlay, Overlay::None) || self.inline_code() {
            let start = state.offset();
            self.register_list_rows(inner, start, rows.len().saturating_sub(start));
            for (row, entry) in rows
                .iter()
                .skip(start)
                .take(usize::from(inner.height))
                .enumerate()
            {
                if let TreeRow::File { depth, .. } = entry {
                    let indent = depth
                        .saturating_mul(2)
                        .min(area.width.saturating_sub(12) as usize);
                    let mark = Rect::new(inner.x + indent as u16, inner.y + row as u16, 1, 1);
                    self.register_click(
                        mark.intersection(inner),
                        MouseAction::MarkRow(start + row),
                    );
                }
            }
        }
    }

    fn diff_view(&self, index: Option<usize>) -> CodeView {
        if index
            .and_then(|i| self.expanded_file(i))
            .is_some_and(|expanded| expanded.is_full())
        {
            CodeView::FullDiff
        } else {
            CodeView::Diff
        }
    }

    fn draw_diff(&mut self, frame: &mut Frame, area: Rect) {
        let index = self.current();
        let path = index.map_or("", |i| self.comparison.files[i].path());
        let focused = self.focus == Focus::Diff;
        let block = code_panel(
            self.diff_view(index),
            &format!(
                "{path}{}  旧行/新行  Esc:ファイル一覧へ",
                if focused { " [操作中]" } else { "" },
            ),
            focused,
        );
        let inner = block.inner(area);
        self.page_height = usize::from(inner.height).max(1);
        let Some(index) = index else {
            frame.render_widget(
                Paragraph::new("表示する差分はありません").block(block),
                area,
            );
            return;
        };
        let file = self.display_file(index);
        let mut gutters = Vec::new();
        let mut lines = Vec::new();
        let digits = file
            .lines
            .iter()
            .filter_map(|line| line.old_line.max(line.new_line))
            .max()
            .unwrap_or(0)
            .to_string()
            .len()
            .max(3);
        let key = self.color_key(index);
        let colors = self
            .colors
            .iter()
            .find(|(cached, _)| cached == &key)
            .map(|(_, colors)| colors);
        for (line_index, line) in file
            .lines
            .iter()
            .enumerate()
            .skip(self.diff_offset)
            .take(self.page_height)
        {
            let (sign, color) = match line.kind {
                LineKind::Added => ('+', Color::Green),
                LineKind::Removed => ('-', Color::Red),
                LineKind::Header => (' ', Color::Cyan),
                LineKind::Context => (' ', Color::Gray),
            };
            let old = line.old_line.map_or(String::new(), |n| n.to_string());
            let new = line.new_line.map_or(String::new(), |n| n.to_string());
            let text = clean(&line.text);
            let style = if line.kind != LineKind::Header
                && !self.query.is_empty()
                && text.contains(&self.query)
            {
                Style::default()
                    .fg(color)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color)
            };
            gutters.push(Line::styled(
                format!("{old:>digits$} {new:>digits$} {sign} "),
                Style::default().fg(color),
            ));
            lines.push(code_line(
                &text,
                colors
                    .and_then(|colors| colors.get(line_index))
                    .and_then(Option::as_ref),
                style,
            ));
        }
        frame.render_widget(block, area);
        let columns = Layout::horizontal([
            Constraint::Length((digits * 2 + 4) as u16),
            Constraint::Min(1),
        ])
        .split(inner);
        if matches!(self.overlay, Overlay::None) {
            self.record_code_viewport(
                columns[1],
                self.diff_offset,
                self.horizontal,
                &lines,
                vec![0; lines.len()],
            );
        }
        frame.render_widget(Paragraph::new(gutters), columns[0]);
        frame.render_widget(
            Paragraph::new(lines).scroll((0, self.horizontal)),
            columns[1],
        );
        if matches!(self.overlay, Overlay::None) {
            for (visible_row, line) in file
                .lines
                .iter()
                .skip(self.diff_offset)
                .take(self.page_height)
                .enumerate()
            {
                let row = self.diff_offset + visible_row;
                if line.kind == LineKind::Header && self.context_gap_action(row, 0).is_some() {
                    let area = Rect::new(
                        columns[1].x,
                        columns[1].y + visible_row as u16,
                        columns[1].width,
                        1,
                    )
                    .intersection(columns[1]);
                    self.draw_gap_controls(frame, area, row, &line.text);
                }
            }
        }
        if file.lines.is_empty() {
            frame.render_widget(
                Paragraph::new("テキスト差分はありません（バイナリなど）"),
                inner,
            );
        }
    }

    fn draw_help(&mut self, frame: &mut Frame, offset: u16) {
        let area = popup(frame.area(), 92, 90);
        frame.render_widget(Clear, area);
        let block = panel("操作方法 ↑↓:移動 Esc:閉じる", true);
        let inner = block.inner(area);
        let regions = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
        let text = "Tab: ファイル一覧 / 差分の操作先を切替\n左右の境界をドラッグ / Alt+←→: ファイル一覧の幅を調整\n選択中のEsc: 選択解除、通常時のEsc: 左のファイル一覧へ\n↑↓: 操作中の領域を移動\nファイル一覧でEnter: ファイルは差分を選択、フォルダは開閉、←/→: 閉じる/開く\nファイル行で←: 親フォルダへ、Space: ファイルだけ確認\nj / k: ファイル一覧では次 / 前の変更ファイル、コードではカーソルを上下移動\nPageUp/Down: 差分を1画面移動\nHome/End: 差分の先頭 / 末尾\nコードでh/j/k/l: 文字・行移動、数字で回数指定\nw/b/e: 単語の次/前/末尾、W/B/E: 空白区切りの単語\n0/^/$: 行頭/最初の文字/行末、gg/G: 先頭/末尾\n数字+gg/G: 表示済みのソース行番号へ（差分は最も近い行）\nCtrl+j/k: 20行、Ctrl+d/u: 半画面、Ctrl+f/b: 1画面、H/M/L: 画面の上/中央/下\nf/F+文字: 行内の次/前、t/T: その手前、; / ,: 同方向/逆方向へ反復\nv/V: 文字/行選択、選択中の移動で範囲を伸縮\niw/aw/iW/aW: 単語/空白付き単語の選択、y: コピー\n⌘C: 選択範囲をコピー（端末がキーをアプリへ転送する場合）\nyy: 行コピー、yiw/yaw/yiW/yaW: 単語コピー\nコピー成功時は選択解除し範囲の先頭へ移動\ngd: カーソル位置の定義、gr: 使用箇所\ngb / 上部の行の履歴: 現在行の変更者・時刻・コミットを表示/非表示\nコードで←→: 左右スクロール、↑↓: 上下スクロール\nSpace: 確認済みを切替（即時保存）\nu: 未確認のみを切替\nファイル一覧でb / コードでC: 比較するbase / headを選択\n/: 差分内の文字列を検索、空欄Enterで解除\nn / N: 次 / 前の検索結果（末尾から循環）\nファイル一覧でe/E / コードでze/zE: 省略区間を20行/全行表示\na: ファイル全体 / 差分を切替\n左ペインでf: 差分のみ / 全ファイルを切替\n左ペインでp: パス絞り込み、Enter/Esc: 入力終了、Ctrl+u: クリア\n全ファイルツリーでj/k: 移動・プレビュー、Enter: フォルダ開閉 / 右ペインへ\nCtrl+P: ファイル検索、Tab: パス/中身、Enter: 検索/開く\nファイル検索とそのソースでEsc: 開く前へ、ソースでq/戻る: 検索結果へ\nコード上の名前をクリック: 定義、Shift+クリック: 使用箇所\n差分・ソースでs: 右ペイン内でコード内の名前を選ぶ\n名前選択で↑↓: 行、←→: 名前、Enter/d: 定義、Shift+R: 使用箇所\nd / R: 名前を入力して定義 / 使用箇所を検索\n定義・使用箇所でj/k:結果移動 /:検索語の編集 Tab:切替 Enter:検索/開く Esc:戻る\nr: 参照元 / 参照先を表示\n参照一覧でTab: 方向切替、Enter: 該当行のソース\nEsc: 元の差分へ戻る\nq: 終了、Ctrl+C: どの画面からも終了\n\n?: 閉じる / Esc: 閉じる";
        let lines = wrap_help(text, inner.width);
        self.help_page = regions[0].height.max(1);
        self.help_max = lines
            .len()
            .saturating_sub(regions[0].height as usize)
            .min(u16::MAX as usize) as u16;
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(lines).scroll((offset.min(self.help_max), 0)),
            regions[0],
        );
        self.draw_click_buttons(
            frame,
            regions[1],
            &[("戻る", MouseAction::Key(KeyCode::Esc))],
        );
    }

    fn draw_picker(&self, frame: &mut Frame, picker: &Picker) {
        let area = popup(frame.area(), 94, 90);
        frame.render_widget(Clear, area);
        frame.render_widget(
            panel(
                &format!(
                    "選択先: {}  Tab:切替 Enter:適用 Esc:戻る",
                    if picker.target == 0 { "base" } else { "head" }
                ),
                true,
            ),
            area,
        );
        let regions = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area.inner(Margin::new(2, 1)));
        self.draw_click_tabs(
            frame,
            regions[0],
            &["base（比較元）", "head（比較先）"],
            picker.target,
        );
        self.draw_click_buttons(
            frame,
            regions[3],
            &[("戻る", MouseAction::Key(KeyCode::Esc))],
        );
        frame.render_widget(
            Paragraph::new(format!(
                "現在: {}\n検索: {}",
                short_ref(if picker.target == 0 {
                    &self.comparison.base
                } else {
                    &self.comparison.head
                }),
                clean(&picker.query)
            )),
            regions[1],
        );
        let choices = matching_refs(&self.refs, &picker.query);
        if choices.is_empty() {
            frame.render_widget(
                Paragraph::new(if self.refs_job.is_some() {
                    "参照一覧を読み込み中…"
                } else {
                    "一致する参照はありません"
                }),
                regions[2],
            );
        } else {
            let items: Vec<ListItem> = choices
                .iter()
                .map(|choice| {
                    ListItem::new(format!("[{}] {}", clean(choice.kind), clean(&choice.label)))
                })
                .collect();
            let count = items.len();
            let mut state =
                ListState::default().with_selected(Some(picker.selected.min(items.len() - 1)));
            frame.render_stateful_widget(
                List::new(items).highlight_style(selected_style()),
                regions[2],
                &mut state,
            );
            self.register_list_rows(
                regions[2],
                state.offset(),
                count.saturating_sub(state.offset()),
            );
        }
    }

    fn draw_references(&self, frame: &mut Frame, tab: usize, selected: usize) {
        let area = popup(frame.area(), 94, 90);
        frame.render_widget(Clear, area);
        frame.render_widget(
            panel(
                &format!(
                    "{}  Tab:切替 ↑↓:選択 Enter:開く Esc:差分へ",
                    if tab == 0 { "参照元" } else { "参照先" }
                ),
                true,
            ),
            area,
        );
        let references = self.current().and_then(|file| {
            let (commit, path) = self.comparison.files[file].source(&self.comparison);
            self.indexes.get(commit).map(|index| index.for_file(path))
        });
        let note_height = if references.as_ref().is_some_and(|refs| refs.note.is_some()) {
            if area.height >= 16 {
                4
            } else {
                2
            }
        } else {
            0
        };
        let regions = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(note_height),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area.inner(Margin::new(2, 1)));
        let result_area =
            self.result_preview_area(frame, regions[2], area.inner(Margin::new(2, 1)).height);
        self.draw_click_tabs(
            frame,
            regions[0],
            &[
                "参照元（このファイルを使う）",
                "参照先（このファイルが使う）",
            ],
            tab,
        );
        self.draw_click_buttons(
            frame,
            regions[3],
            &[
                ("開く", MouseAction::Key(KeyCode::Enter)),
                ("戻る", MouseAction::Key(KeyCode::Esc)),
            ],
        );
        if self.current().is_none() {
            frame.render_widget(Paragraph::new("ファイルが選択されていません"), result_area);
            return;
        };
        let Some(references) = references else {
            let message = self
                .index_error
                .as_ref()
                .map(|error| format!("参照を読み込めません: {}", clean(error)))
                .unwrap_or_else(|| "参照を読み込み中… Escで差分へ戻れます".into());
            frame.render_widget(
                Paragraph::new(message).wrap(Wrap { trim: false }),
                result_area,
            );
            return;
        };
        if let Some(note) = references.note.as_deref() {
            frame.render_widget(
                Paragraph::new(clean(note))
                    .style(Style::default().fg(Color::Gray))
                    .wrap(Wrap { trim: false }),
                regions[1],
            );
        }
        let list = if tab == 0 {
            references.incoming
        } else {
            references.outgoing
        };
        if list.is_empty() {
            frame.render_widget(Paragraph::new("該当する参照はありません"), result_area);
        } else {
            let items: Vec<ListItem> = list
                .iter()
                .map(|reference| {
                    ListItem::new(format!(
                        "{}:{}  {}",
                        clean(&reference.path),
                        reference.line,
                        clean(&reference.text)
                    ))
                })
                .collect();
            let mut state = ListState::default().with_selected(Some(selected.min(items.len() - 1)));
            frame.render_stateful_widget(
                List::new(items).highlight_style(selected_style()),
                result_area,
                &mut state,
            );
            self.register_list_rows(
                result_area,
                state.offset(),
                list.len().saturating_sub(state.offset()),
            );
        }
    }

    fn draw_source(&self, frame: &mut Frame, source: &Source, pane: Rect) {
        let area = if source.symbols.is_some() {
            popup(frame.area(), 98, 96)
        } else {
            pane
        };
        frame.render_widget(Clear, area);
        let query = source
            .symbols
            .as_ref()
            .map(|search| format!(" 検索語: {}", clean(search.query())))
            .unwrap_or_default();
        let block = code_panel(
            CodeView::Source,
            &format!(
                "{}{query}{} s:名前を選ぶ d:定義 R:使用箇所 j/k ↑↓:移動 Esc:{}",
                source.path,
                if self.focus == Focus::Diff {
                    " [操作中]"
                } else {
                    ""
                },
                if source.from_browser {
                    "全ファイルへ"
                } else if source.symbols.is_some() {
                    "名前検索へ"
                } else if source.all_files.is_some() || source.return_focus.is_some() {
                    "元の画面へ"
                } else {
                    "ファイル一覧へ"
                }
            ),
            self.focus == Focus::Diff,
        );
        let regions =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(block.inner(area));
        let inner = regions[0];
        frame.render_widget(block, area);
        let lines: Vec<Line> = source
            .lines
            .iter()
            .enumerate()
            .skip(source.offset)
            .take(inner.height as usize)
            .map(|(index, text)| {
                let style = if index == source.matched {
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::UNDERLINED)
                } else {
                    Style::default()
                };
                let mut line = code_line(
                    text,
                    source.colors.as_ref().and_then(|colors| colors.get(index)),
                    style,
                );
                line.spans.insert(
                    0,
                    Span::styled(
                        format!("{:>6}  ", index + 1),
                        Style::default().fg(Color::Gray),
                    ),
                );
                line
            })
            .collect();
        let prefixes = source
            .lines
            .iter()
            .enumerate()
            .skip(source.offset)
            .take(lines.len())
            .map(|(index, _)| format!("{:>6}  ", index + 1).len())
            .collect();
        self.record_code_viewport(inner, source.offset, source.horizontal, &lines, prefixes);
        frame.render_widget(Paragraph::new(lines).scroll((0, source.horizontal)), inner);
        self.draw_click_buttons(
            frame,
            regions[1],
            &[("戻る", MouseAction::Key(KeyCode::Char('q')))],
        );
    }
}

// Apply the same sanitization to tokens as to plain code. A malformed or incomplete
// result falls back to the original text, so coloring can never remove code.
fn code_line(text: &str, colors: Option<&Spans>, style: Style) -> Line<'static> {
    if let Some(colors) = colors {
        let spans: Vec<Span<'static>> = colors
            .iter()
            .map(|span| {
                let mut token_style = span.style;
                if style.bg.is_some() && token_style.fg == style.bg {
                    token_style.fg = Some(style.fg.unwrap_or(Color::Gray));
                }
                Span::styled(clean(&span.content), token_style)
            })
            .collect();
        if spans
            .iter()
            .flat_map(|span| span.content.chars())
            .eq(text.chars())
        {
            return Line::from(spans).style(style);
        }
    }
    Line::styled(text.to_owned(), style)
}

fn wrap_help(text: &str, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for row in text.lines() {
        let mut line = String::new();
        for character in row.chars() {
            let mut candidate = line.clone();
            candidate.push(character);
            if !line.is_empty() && Line::raw(&candidate).width() > usize::from(width.max(1)) {
                lines.push(Line::raw(std::mem::take(&mut line)));
            }
            line.push(character);
        }
        lines.push(Line::raw(line));
    }
    lines
}

fn job_result<T>(job: &Job<T>) -> Option<std::result::Result<T, String>> {
    match job.try_recv() {
        Ok(result) => Some(result),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => Some(Err("読み込み処理が終了しました".into())),
    }
}

fn matching_refs<'a>(refs: &'a [RefChoice], query: &str) -> Vec<&'a RefChoice> {
    let query = query.to_lowercase();
    refs.iter()
        .filter(|choice| clean(&choice.label).to_lowercase().contains(&query))
        .collect()
}

fn short_ref(revision: &str) -> String {
    clean(
        revision
            .strip_prefix("refs/heads/")
            .or_else(|| revision.strip_prefix("refs/remotes/"))
            .or_else(|| revision.strip_prefix("refs/tags/"))
            .unwrap_or(revision),
    )
}

fn shifted(position: usize, delta: isize, count: usize) -> usize {
    position
        .saturating_add_signed(delta)
        .min(count.saturating_sub(1))
}

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn panel(title: &str, focused: bool) -> Block<'static> {
    bordered_panel(focused).title(clean(title))
}

fn code_panel(view: CodeView, title: &str, focused: bool) -> Block<'static> {
    let (label, color) = match view {
        CodeView::Diff => ("[差分]", Color::Cyan),
        CodeView::FullDiff => ("[全体＋差分]", Color::LightYellow),
        CodeView::Source => ("[ファイル全体]", Color::LightGreen),
    };
    bordered_panel(focused).title(Line::from(vec![
        Span::styled(
            label,
            Style::default()
                .fg(Color::Black)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  {}", clean(title))),
    ]))
}

fn bordered_panel(focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }))
}

fn selected_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let rows = Layout::vertical([
        Constraint::Percentage((100 - height) / 2),
        Constraint::Percentage(height),
        Constraint::Min(0),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - width) / 2),
        Constraint::Percentage(width),
        Constraint::Min(0),
    ])
    .split(rows[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) struct Fixture(std::path::PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn wide_review_uses_fifteen_percent_for_the_file_list_with_a_minimum_width() {
        use ratatui::backend::TestBackend;

        let (_fixture, mut app) = app();
        for (width, expected) in [(80, 14), (120, 18), (240, 36)] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert_eq!(app.mouse_panes[0].width, expected);
            assert_eq!(app.mouse_panes[1].width, width - expected);
        }
    }

    #[test]
    fn review_pane_width_can_be_adjusted_by_dragging_or_with_alt_arrows() {
        use ratatui::backend::TestBackend;

        let (_fixture, mut app) = app();
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let row = app.mouse_panes[0].y + 4;
        let left_mouse = |kind, column| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let divider = app.mouse_panes[1].x - 1;
        assert!(!app.mouse(left_mouse(MouseEventKind::Down(MouseButton::Left), divider)));
        assert!(app.mouse(left_mouse(MouseEventKind::Drag(MouseButton::Left), 47)));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.mouse_panes[0].width, 48);
        assert!(app.mouse(left_mouse(MouseEventKind::Up(MouseButton::Left), 119)));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.mouse_panes[0].width, 72); // Clamp to 60%.

        assert!(!app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.mouse_panes[0].width, 66);

        let mut narrow = Terminal::new(TestBackend::new(60, 24)).unwrap();
        narrow.draw(|frame| app.draw(frame)).unwrap();
        assert!(!app.can_resize_panes());
        assert_eq!(app.mouse_panes[0].width, 60);
        assert_eq!(app.mouse_panes[1].y, app.mouse_panes[0].bottom());
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.mouse_panes[0].width, 66);
    }

    pub(super) fn app() -> (Fixture, App) {
        use super::super::git::{DiffLine, FileDiff};
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "review-tui-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        let viewed = ViewedStore::load_at(path.clone(), path.join("viewed.json")).unwrap();
        let files = ["a.rs", "b.rs", "c.rs"]
            .iter()
            .map(|path| FileDiff {
                old_path: (*path).into(),
                new_path: (*path).into(),
                status: 'M',
                digest: "digest".into(),
                lines: vec![
                    DiffLine {
                        kind: LineKind::Removed,
                        text: "old needle".into(),
                        old_line: Some(12),
                        new_line: None,
                    },
                    DiffLine {
                        kind: LineKind::Added,
                        text: "new value".into(),
                        old_line: None,
                        new_line: Some(12),
                    },
                    DiffLine {
                        kind: LineKind::Context,
                        text: "second needle".into(),
                        old_line: Some(13),
                        new_line: Some(13),
                    },
                ],
            })
            .collect();
        let app = App {
            repository: Repository {
                root: path.clone(),
                identity: path.clone(),
            },
            comparison: Comparison {
                base: "refs/heads/main".into(),
                head: "refs/heads/topic".into(),
                base_key: "refs/heads/main".into(),
                head_key: "refs/heads/topic".into(),
                merge_base: "basecommit".into(),
                head_commit: "headcommit".into(),
                files,
            },
            viewed,
            file_browser: Default::default(),
            selected: 0,
            only_unviewed: false,
            collapsed: HashSet::new(),
            folder: None,
            focus: Focus::Diff,
            diff_offset: 0,
            cursor: Default::default(),
            horizontal: 0,
            query: String::new(),
            overlay: Overlay::None,
            error: None,
            clipboard: None,
            refs: Vec::new(),
            refs_job: None,
            indexes: HashMap::new(),
            index_job: None,
            index_error: None,
            page_height: 10,
            help_max: 0,
            help_page: 1,
            history: Default::default(),
            colors: VecDeque::new(),
            result_preview: Default::default(),
            colors_job: None,
            code_viewport: Default::default(),
            mouse_down: None,
            expanded: None,
            context_job: None,
            context_attempt: None,
            context_history: HashMap::new(),
            mouse_targets: Default::default(),
            mouse_panes: Default::default(),
            file_pane_percent: DEFAULT_FILE_PANE_PERCENT,
            exit_requested: false,
        };
        (Fixture(path), app)
    }

    pub(super) fn press(app: &mut App, code: KeyCode) -> bool {
        app.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn escape_focuses_files_without_changing_diff_position() {
        let (_fixture, mut app) = app();
        app.selected = 1;
        app.diff_offset = 2;
        app.horizontal = 8;
        for _ in 0..2 {
            assert!(!press(&mut app, KeyCode::Esc));
            assert!(app.focus == Focus::Files);
            assert_eq!((app.selected, app.diff_offset, app.horizontal), (1, 2, 8));
        }
        press(&mut app, KeyCode::Tab);
        assert!(app.focus == Focus::Diff);
        assert_eq!((app.selected, app.diff_offset, app.horizontal), (1, 2, 8));
    }

    #[test]
    fn search_and_reference_modals_restore_the_origin_pane_and_selection() {
        for focus in [Focus::Files, Focus::Diff] {
            let (_fixture, mut app) = app();
            app.focus = focus;
            app.cursor.position = Some(cursor::Position { row: 1, byte: 0 });
            app.cursor.anchor = app.cursor.position;
            for overlay in [
                Overlay::Search("needle".into()),
                Overlay::References {
                    tab: 0,
                    selected: 0,
                },
            ] {
                app.overlay = overlay;
                press(&mut app, KeyCode::Tab);
                press(&mut app, KeyCode::Esc);
                assert!(matches!(app.overlay, Overlay::None));
                assert!(app.focus == focus);
                assert_eq!(app.cursor.anchor, app.cursor.position);
            }
            for close in [KeyCode::Esc, KeyCode::Char('q')] {
                let mut source = Source::new("commit".into(), "a.rs".into(), 1, "source".into());
                source.return_focus = Some(focus);
                app.overlay = Overlay::Source(source);
                app.focus = Focus::Diff;
                press(&mut app, close);
                assert!(matches!(app.overlay, Overlay::None));
                assert!(app.focus == focus);
                assert_eq!(app.cursor.anchor, app.cursor.position);
            }
        }
    }

    #[test]
    fn file_focus_search_and_source_return_preserve_expected_position() {
        let (_fixture, mut app) = app();
        press(&mut app, KeyCode::Down);
        assert_eq!(app.diff_offset, 1);
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.current(), Some(1));
        assert_eq!(app.diff_offset, 0);
        app.query = "needle".into();
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.diff_offset, 2);
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.diff_offset, 0);
        press(&mut app, KeyCode::Char('N'));
        assert_eq!(app.diff_offset, 2);
        app.horizontal = 8;
        app.overlay = Overlay::Source(Source {
            commit: "headcommit".into(),
            symbols: None,
            all_files: None,
            return_focus: None,
            from_browser: false,
            path: "a.rs".into(),
            lines: vec!["source".into()],
            offset: 0,
            matched: 0,
            horizontal: 0,
            colors: None,
            colors_job: None,
            cursor: Default::default(),
        });
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.current(), Some(1));
        assert_eq!(app.diff_offset, 2);
        assert_eq!(app.horizontal, 8);
        assert!(app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn failed_comparison_keeps_existing_diff() {
        let (_fixture, mut app) = app();
        app.selected = 1;
        app.diff_offset = 2;
        app.refs.push(RefChoice {
            label: "invalid".into(),
            revision: "missing".into(),
            kind: "branch",
        });
        app.overlay = Overlay::Branches(Picker {
            target: 0,
            query: String::new(),
            selected: 0,
        });
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.comparison.base, "refs/heads/main");
        assert_eq!(app.current(), Some(1));
        assert_eq!(app.diff_offset, 2);
        assert!(app.error.is_some());
        assert!(matches!(app.overlay, Overlay::Branches(_)));
    }

    #[test]
    fn filtering_keeps_the_nearest_remaining_file() {
        let (fixture, mut app) = app();
        let state = serde_json::json!({ "comparisons": [{ "repository": fixture.0, "base": "refs/heads/main", "head": "refs/heads/topic", "files": {"b.rs": "digest"} }] });
        std::fs::write(
            fixture.0.join("viewed.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        app.viewed =
            ViewedStore::load_at(fixture.0.clone(), fixture.0.join("viewed.json")).unwrap();
        app.selected = 1;
        press(&mut app, KeyCode::Char('u'));
        assert_eq!(app.current(), Some(2));
        press(&mut app, KeyCode::Char('u'));
        assert_eq!(app.current(), Some(2));
    }

    #[test]
    fn all_overlays_render_in_short_and_narrow_terminals() {
        use ratatui::backend::TestBackend;
        let (_fixture, mut app) = app();
        for width in [1, 10, 30, 80] {
            for height in [1, 5, 12, 30] {
                for overlay in [
                    Overlay::None,
                    Overlay::Help(0),
                    Overlay::Search("needle".into()),
                    Overlay::Branches(Picker {
                        target: 1,
                        query: String::new(),
                        selected: 0,
                    }),
                    Overlay::References {
                        tab: 1,
                        selected: 0,
                    },
                    Overlay::Source(Source {
                        commit: "headcommit".into(),
                        symbols: None,
                        all_files: None,
                        return_focus: None,
                        from_browser: false,
                        path: "control\x1b.rs".into(),
                        lines: vec!["text".into()],
                        offset: 0,
                        matched: 0,
                        horizontal: 0,
                        colors: None,
                        colors_job: None,
                        cursor: Default::default(),
                    }),
                ] {
                    app.overlay = overlay;
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal.draw(|frame| app.draw(frame)).unwrap();
                }
            }
        }
    }

    #[test]
    fn help_can_scroll_to_exit_controls_on_a_short_terminal() {
        use ratatui::backend::TestBackend;
        let (_fixture, mut app) = app();
        press(&mut app, KeyCode::Char('?'));
        let mut terminal = Terminal::new(TestBackend::new(55, 16)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(app.help_max > 0);
        press(&mut app, KeyCode::End);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("Ctrl+C"), "{text}");
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn horizontal_scroll_keeps_line_numbers_and_diff_signs_visible() {
        use ratatui::backend::TestBackend;
        let (_fixture, mut app) = app();
        app.horizontal = 4;
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..20)
            .map(|y| (0..100).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        assert!(rows.iter().any(|row| row.contains("12     -")), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains("12 +")), "{rows:?}");
        assert!(buffer
            .content
            .iter()
            .any(|cell| cell.symbol() == "-" && cell.fg == Color::Red));
        assert!(buffer
            .content
            .iter()
            .any(|cell| cell.symbol() == "+" && cell.fg == Color::Green));
    }

    #[test]
    fn diff_jobs_are_single_cached_and_ignore_results_from_another_comparison() {
        let (_fixture, mut app) = app();
        let key = app.color_key(0);
        let (tx, rx) = mpsc::channel();
        app.colors_job = Some((key.clone(), rx));
        app.selected = 1;
        app.start_colors();
        assert!(app
            .colors_job
            .as_ref()
            .is_some_and(|(active, _)| active == &key));
        app.comparison.head_commit = "different-head".into();
        // Avoid starting an unrelated real Git job after rejecting the stale result.
        app.comparison.files[1].old_path = "b.txt".into();
        app.comparison.files[1].new_path = "b.txt".into();
        tx.send(Ok(vec![Some(vec![Span::raw("stale")])])).unwrap();
        app.poll_jobs();
        assert!(app.colors.is_empty());
        assert!(app.colors_job.is_none());

        app.selected = 0;
        for version in 0..8 {
            app.comparison.files[0].digest = format!("version-{version}");
            let key = app.color_key(0);
            let (tx, rx) = mpsc::channel();
            app.colors_job = Some((key, rx));
            tx.send(Ok(Vec::new())).unwrap();
            app.poll_jobs();
            assert!(
                app.colors_job.is_none(),
                "cached files must not start another worker"
            );
        }
        assert_eq!(app.colors.len(), 6);
        assert_eq!(app.colors.front().unwrap().0.digest, "version-2");
        assert_eq!(app.colors.back().unwrap().0.digest, "version-7");
    }

    #[test]
    fn syntax_tokens_keep_diff_gutters_search_and_horizontal_clipping() {
        use ratatui::backend::TestBackend;
        let (_fixture, mut app) = app();
        let key = app.color_key(0);
        app.colors.push_back((
            key,
            vec![
                None,
                Some(vec![
                    Span::styled("new", Style::default().fg(Color::Magenta)),
                    Span::raw(" value"),
                ]),
                None,
            ],
        ));
        app.query = "new".into();
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(buffer.content.iter().any(|cell| cell.symbol() == "n"
            && cell.fg == Color::Magenta
            && cell.bg == Color::DarkGray));
        assert!(buffer
            .content
            .iter()
            .any(|cell| cell.symbol() == "+" && cell.fg == Color::Green));
        assert!(buffer
            .content
            .iter()
            .any(|cell| cell.symbol() == "-" && cell.fg == Color::Red));
        app.horizontal = 4;
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..20)
            .map(|y| (0..100).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        assert!(
            rows.iter().any(|row| row.contains("12 + value")),
            "{rows:?}"
        );
    }

    #[test]
    fn source_match_keeps_syntax_foreground_and_diff_position() {
        use ratatui::backend::TestBackend;
        let (_fixture, mut app) = app();
        app.diff_offset = 2;
        app.horizontal = 8;
        app.overlay = Overlay::Source(Source {
            commit: "headcommit".into(),
            symbols: None,
            all_files: None,
            return_focus: None,
            from_browser: false,
            path: "a.rs".into(),
            lines: vec!["fn main() {}".into()],
            offset: 0,
            matched: 0,
            horizontal: 0,
            colors: Some(vec![vec![
                Span::styled("fn", Style::default().fg(Color::Magenta)),
                Span::raw(" main() {}"),
            ]]),
            colors_job: None,
            cursor: Default::default(),
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(buffer.content.iter().any(|cell| cell.symbol() == "f"
            && cell.fg == Color::Magenta
            && cell.bg == Color::DarkGray
            && cell.modifier.contains(Modifier::UNDERLINED)));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.diff_offset, 2);
        assert_eq!(app.horizontal, 8);
    }

    #[test]
    fn matched_comments_remain_visible_against_the_highlight_background() {
        let spans = vec![Span::styled(
            "// comment",
            Style::default().fg(Color::DarkGray),
        )];
        let diff = code_line(
            "// comment",
            Some(&spans),
            Style::default().fg(Color::Green).bg(Color::DarkGray),
        );
        assert_eq!(diff.spans[0].style.fg, Some(Color::Green));
        let source = code_line(
            "// comment",
            Some(&spans),
            Style::default().bg(Color::DarkGray),
        );
        assert_eq!(source.spans[0].style.fg, Some(Color::Gray));
    }

    #[test]
    fn token_sanitization_preserves_visible_text_and_falls_back_on_incomplete_results() {
        let spans = vec![
            Span::styled("fn\x1b\t", Style::default().fg(Color::Magenta)),
            Span::raw(" name"),
        ];
        let line = code_line("fn   name", Some(&spans), Style::default());
        assert_eq!(line.to_string(), "fn   name");
        assert_eq!(line.spans[0].style.fg, Some(Color::Magenta));
        let incomplete = vec![Span::raw("fn")];
        assert_eq!(
            code_line("fn main() {}", Some(&incomplete), Style::default()).to_string(),
            "fn main() {}"
        );
    }

    #[test]
    fn repository_text_cannot_inject_terminal_controls() {
        assert_eq!(clean("a\x1b[31m\t\r\n\x7fb"), "a [31m    b");
        assert!(!clean("\x1b\u{009b}\0").chars().any(char::is_control));
    }

    #[test]
    fn movement_is_safe_for_empty_lists_and_large_offsets() {
        assert_eq!(shifted(0, -1, 0), 0);
        assert_eq!(shifted(0, 1, 0), 0);
        assert_eq!(shifted(usize::MAX, 1, 3), 2);
        assert_eq!(shifted(2, -1, 3), 1);
    }

    #[test]
    fn popups_fit_tiny_terminals() {
        for width in 0..12 {
            for height in 0..8 {
                let area = Rect::new(0, 0, width, height);
                let popup = popup(area, 94, 90);
                assert!(popup.right() <= area.right());
                assert!(popup.bottom() <= area.bottom());
            }
        }
    }
}
