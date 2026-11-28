//! Non-modal tree of the files in the comparison's head commit.
use super::{file_tree::FileTree, *};
use crate::review::all_files;

#[derive(Default)]
pub(super) struct FileBrowser {
    pub(super) active: bool,
    pub(super) query: String,
    pub(super) editing: bool,
    diff_collapsed: HashSet<String>,
    head: Option<String>,
    paths: Vec<String>,
    tree: FileTree,
    rows: Vec<TreeRow>,
    collapsed: HashSet<String>,
    selected: usize,
    paths_job: Option<Job<Vec<String>>>,
    source_job: Option<(String, Job<String>)>,
    requested_source: Option<String>,
    error: Option<String>,
}

impl FileBrowser {
    fn rebuild_rows(&mut self) {
        if self.query.is_empty() {
            self.rows = self.tree.rows(&self.collapsed);
        } else {
            let query = self.query.to_lowercase();
            self.rows = self
                .paths
                .iter()
                .enumerate()
                .filter_map(|(index, path)| {
                    clean(path)
                        .to_lowercase()
                        .contains(&query)
                        .then_some(TreeRow::File { depth: 0, index })
                })
                .collect();
        }
    }
}

impl App {
    pub(super) fn start_path_filter(&mut self) {
        self.focus = Focus::Files;
        self.file_browser.editing = true;
    }

    pub(super) fn path_filter_key(&mut self, key: KeyEvent) {
        let current = self.current();
        let browser = &mut self.file_browser;
        let was_empty = browser.query.is_empty();
        match key.code {
            KeyCode::Enter | KeyCode::Esc => {
                browser.editing = false;
                return;
            }
            KeyCode::Backspace => {
                if browser.query.pop().is_none() {
                    return;
                }
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => browser.query.clear(),
            KeyCode::Char(c)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::ALT,
                ) && !c.is_control()
                    && browser.query.len() < 4096 =>
            {
                browser.query.push(c)
            }
            _ => return,
        }
        if was_empty != browser.query.is_empty() {
            std::mem::swap(&mut self.collapsed, &mut browser.diff_collapsed);
        }
        browser.selected = 0;
        browser.rebuild_rows();
        if browser.active {
            self.preview_browser_selection();
        } else {
            self.selected = current
                .and_then(|index| self.visible().iter().position(|&i| i == index))
                .unwrap_or(0);
            self.folder = None;
            if self.current() != current {
                self.reset_position();
            }
            self.reconcile_tree();
        }
    }

    pub(super) fn draw_path_filter(&self, frame: &mut Frame, area: Rect) -> Rect {
        let browser = &self.file_browser;
        if !browser.editing && browser.query.is_empty() {
            return area;
        }
        let [filter, tree] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        let hint = if browser.editing {
            "  Enter:確定"
        } else {
            "  p:編集"
        };
        frame.render_widget(
            Paragraph::new(format!("パス: {}{hint}", clean(&browser.query)))
                .style(Style::default().fg(Color::Cyan)),
            filter,
        );
        if matches!(self.overlay, Overlay::None) || self.browser_source() {
            self.register_click(filter, MouseAction::PathFilter);
        }
        tree
    }

    pub(super) fn browser_source(&self) -> bool {
        matches!(&self.overlay, Overlay::Source(source) if source.from_browser)
    }

    pub(super) fn toggle_file_browser(&mut self) {
        let current = self.current();
        let browser = &mut self.file_browser;
        browser.active = !browser.active;
        browser.requested_source = None;
        browser.editing = false;
        if browser.active
            && (browser.head.as_ref() != Some(&self.comparison.head_commit)
                || browser.error.is_some())
        {
            let repository = self.repository_copy();
            let head = self.comparison.head_commit.clone();
            let revision = head.clone();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(
                    all_files::list_paths(&repository, &revision).map_err(|e| format!("{e:#}")),
                );
            });
            self.file_browser = FileBrowser {
                active: true,
                head: Some(head),
                paths_job: Some(rx),
                query: std::mem::take(&mut self.file_browser.query),
                diff_collapsed: std::mem::take(&mut self.file_browser.diff_collapsed),
                ..Default::default()
            };
        }
        if self.browser_source() {
            self.overlay = Overlay::None;
        }
        let visible = self.visible();
        self.selected = current
            .and_then(|current| visible.iter().position(|&i| i == current))
            .unwrap_or(0);
        if self.current() != current {
            self.reset_position();
        }
        self.reconcile_tree();
        self.focus = Focus::Files;
        if self.file_browser.active {
            self.preview_browser_selection();
        }
    }

    pub(super) fn refresh_file_browser(&mut self) {
        if self.file_browser.active
            && self.file_browser.head.as_ref() != Some(&self.comparison.head_commit)
        {
            self.file_browser.active = false;
            self.toggle_file_browser();
        }
    }

    pub(super) fn poll_file_browser(&mut self) -> bool {
        let browser = &mut self.file_browser;
        if !browser.active {
            return false;
        }
        let mut changed = false;
        if let Some(result) = browser.paths_job.as_ref().and_then(job_result) {
            browser.paths_job = None;
            match result {
                Ok(paths) => {
                    browser.tree = FileTree::new(paths.iter().map(String::as_str).enumerate());
                    browser.paths = paths;
                    browser.collapsed = browser.tree.folders();
                    browser.rebuild_rows();
                }
                Err(error) => {
                    browser.error = Some(format!("ファイル一覧を読み込めません: {error}"))
                }
            }
            changed = true;
            self.reveal_current_in_browser();
            self.preview_browser_selection();
        }
        if matches!(self.overlay, Overlay::None) || self.browser_source() {
            let browser = &mut self.file_browser;
            if let Some(result) = browser
                .source_job
                .as_ref()
                .and_then(|(_, job)| job_result(job))
            {
                let (path, _) = browser.source_job.take().unwrap();
                if browser.requested_source.as_ref() == Some(&path) {
                    browser.requested_source = None;
                    match result {
                        Ok(text) => {
                            let mut source =
                                Source::new(self.comparison.head_commit.clone(), path, 1, text);
                            source.from_browser = true;
                            self.overlay = Overlay::Source(source);
                        }
                        Err(error) => self.error = Some(format!("ソースを開けません: {error}")),
                    }
                    changed = true;
                }
            }
            self.start_browser_source();
        }
        changed
    }

    pub(super) fn file_browser_key(&mut self, key: KeyCode) -> bool {
        let browser = &mut self.file_browser;
        let count = browser.rows.len();
        let previous = browser.selected;
        match key {
            KeyCode::Enter => self.open_browser_row(previous, Focus::Diff),
            KeyCode::Left | KeyCode::Right => match browser.rows.get(previous) {
                Some(TreeRow::Dir { key: path, .. }) => {
                    if key == KeyCode::Right {
                        browser.collapsed.remove(path);
                    } else {
                        browser.collapsed.insert(path.clone());
                    }
                    browser.rebuild_rows();
                }
                Some(TreeRow::File { depth, .. }) if key == KeyCode::Left && *depth > 0 => {
                    if let Some(parent) = browser.rows[..previous].iter().rposition(
                        |row| matches!(row, TreeRow::Dir { depth: d, .. } if d + 1 == *depth),
                    ) {
                        browser.selected = parent;
                    }
                }
                _ => {}
            },
            KeyCode::Down | KeyCode::Char('j') => browser.selected = shifted(previous, 1, count),
            KeyCode::Up | KeyCode::Char('k') => browser.selected = shifted(previous, -1, count),
            KeyCode::Home => browser.selected = 0,
            KeyCode::End => browser.selected = count.saturating_sub(1),
            KeyCode::PageUp => {
                browser.selected = shifted(
                    previous,
                    -(self.mouse_panes[0].height.saturating_sub(2).max(1) as isize),
                    count,
                )
            }
            KeyCode::PageDown => {
                browser.selected = shifted(
                    previous,
                    self.mouse_panes[0].height.saturating_sub(2).max(1) as isize,
                    count,
                )
            }
            // Review markers and the unreviewed filter are for the diff list.
            KeyCode::Char(' ' | 'u') => {}
            _ => return false,
        }
        if self.file_browser.selected != previous {
            self.preview_browser_selection();
        }
        true
    }

    pub(super) fn open_browser_row(&mut self, row: usize, focus: Focus) {
        self.error = None;
        let browser = &mut self.file_browser;
        let Some(entry) = browser.rows.get(row) else {
            return;
        };
        browser.selected = row;
        self.focus = Focus::Files;
        match entry {
            TreeRow::Dir { key, .. } => {
                if !browser.collapsed.remove(key) {
                    browser.collapsed.insert(key.clone());
                }
                browser.rebuild_rows();
            }
            TreeRow::File { .. } => self.focus = focus,
        }
        self.preview_browser_selection();
    }

    fn reveal_current_in_browser(&mut self) {
        let Some(file) = self.current().map(|i| &self.comparison.files[i]) else {
            return;
        };
        if file.status == 'D' {
            return;
        }
        let path = &file.new_path;
        let browser = &mut self.file_browser;
        let Some(index) = browser.paths.iter().position(|p| p == path) else {
            return;
        };
        if browser.query.is_empty() {
            for (separator, _) in path.match_indices('/') {
                browser.collapsed.remove(&path[..separator]);
            }
            browser.rebuild_rows();
        }
        if let Some(row) = browser
            .rows
            .iter()
            .position(|row| matches!(row, TreeRow::File { index: i, .. } if *i == index))
        {
            browser.selected = row;
        }
    }

    fn preview_browser_selection(&mut self) {
        let browser = &mut self.file_browser;
        browser.requested_source = None;
        let Some(TreeRow::File { index, .. }) = browser.rows.get(browser.selected) else {
            return;
        };
        let path = browser.paths[*index].clone();
        if matches!(&self.overlay, Overlay::Source(source) if source.from_browser && source.path == path)
        {
            return;
        }
        if !matches!(self.overlay, Overlay::None) && !self.browser_source() {
            return;
        }
        if matches!(self.overlay, Overlay::None)
            && self.current().is_some_and(|i| {
                let file = &self.comparison.files[i];
                file.status != 'D' && file.new_path == path
            })
        {
            return;
        }
        if self.open_all_files_diff(&path, None) {
            self.overlay = Overlay::None;
        } else {
            self.file_browser.requested_source = Some(path);
            self.start_browser_source();
        }
    }

    fn start_browser_source(&mut self) {
        if self.file_browser.source_job.is_some() {
            return;
        }
        let Some(path) = self.file_browser.requested_source.clone() else {
            return;
        };
        let repository = self.repository_copy();
        let head = self.comparison.head_commit.clone();
        let worker_path = path.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(
                repository
                    .file_text(&head, &worker_path)
                    .map_err(|e| format!("{e:#}")),
            );
        });
        self.file_browser.source_job = Some((path, rx));
    }

    pub(super) fn draw_file_browser(&self, frame: &mut Frame, area: Rect) {
        let browser = &self.file_browser;
        let focused = self.focus == Focus::Files;
        let block = panel(
            if focused {
                "全ファイル [操作中]"
            } else {
                "全ファイル"
            },
            focused,
        );
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let message = if browser.paths_job.is_some() {
            Some("読み込み中…")
        } else if let Some(error) = &browser.error {
            Some(error.as_str())
        } else if browser.rows.is_empty() {
            Some(if browser.query.is_empty() {
                "ファイルはありません"
            } else {
                "一致するパスはありません"
            })
        } else {
            None
        };
        if let Some(message) = message {
            frame.render_widget(
                Paragraph::new(clean(message)).wrap(Wrap { trim: false }),
                inner,
            );
            return;
        }
        let height = usize::from(inner.height);
        let start = browser
            .selected
            .saturating_sub(height / 2)
            .min(browser.rows.len().saturating_sub(height));
        let items: Vec<_> = browser
            .rows
            .iter()
            .skip(start)
            .take(height)
            .map(|row| {
                let (depth, label) = match row {
                    TreeRow::Dir { depth, key, label } => (
                        *depth,
                        format!(
                            "{} {}/",
                            if browser.collapsed.contains(key) {
                                "▸"
                            } else {
                                "▾"
                            },
                            clean(label)
                        ),
                    ),
                    TreeRow::File { depth, index } => (
                        *depth,
                        if browser.query.is_empty() {
                            clean(browser.paths[*index].rsplit('/').next().unwrap_or_default())
                        } else {
                            clean(&browser.paths[*index])
                        },
                    ),
                };
                let indent = depth
                    .saturating_mul(2)
                    .min(usize::from(inner.width.saturating_sub(10)));
                ListItem::new(format!("{}{label}", " ".repeat(indent)))
            })
            .collect();
        if matches!(self.overlay, Overlay::None) || self.inline_code() {
            for (row, _) in items.iter().enumerate() {
                self.register_click(
                    Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
                    MouseAction::BrowserRow(start + row),
                );
            }
        }
        let mut state = ListState::default().with_selected(Some(browser.selected - start));
        frame.render_stateful_widget(
            List::new(items).highlight_style(selected_style()),
            inner,
            &mut state,
        );
    }

    pub(super) fn browser_loading_source(&self) -> bool {
        self.file_browser.requested_source.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{app, press};
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn toggling_keeps_the_tree_in_the_left_pane_and_jk_navigates_it() {
        let (_fixture, mut app) = app();
        app.focus = Focus::Files;
        app.selected = 1; // Start on a diff not present in the test's head listing.
        press(&mut app, KeyCode::Char('f'));
        assert!(app.file_browser.active);
        assert!(matches!(app.overlay, Overlay::None));

        let (tx, rx) = mpsc::channel();
        app.file_browser.paths_job = Some(rx);
        tx.send(Ok(vec!["a.rs".into(), "src/plain.txt".into()]))
            .unwrap();
        assert!(app.poll_file_browser());
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.file_browser.rows.len(), 3);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.file_browser.selected, 1);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.file_browser.selected, 0);

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let buffer = terminal.draw(|frame| app.draw(frame)).unwrap().buffer;
        let pane = app.mouse_panes[0];
        assert!(matches!(
            app.mouse_control_at(pane.x + 1, pane.y + 2),
            Some(MouseAction::BrowserRow(1))
        ));
        let text: String = buffer
            .content
            .iter()
            .flat_map(|cell| cell.symbol().chars())
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(text.contains("[差分]"));
        app.activate_mouse_control(MouseAction::ToggleFileBrowser);
        assert!(!app.file_browser.active);
        assert!(matches!(app.overlay, Overlay::None));
        press(&mut app, KeyCode::Char('f'));
        assert_eq!(app.file_browser.rows.len(), 3);
    }

    #[test]
    fn path_filter_stays_inline_and_is_shared_between_file_list_modes() {
        let (_fixture, mut app) = app();
        app.focus = Focus::Files;
        press(&mut app, KeyCode::Char('p'));
        for c in "B.RS".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let buffer = terminal.draw(|frame| app.draw(frame)).unwrap().buffer;
        let text: String = buffer
            .content
            .iter()
            .flat_map(|cell| cell.symbol().chars())
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(text.contains("パス:B.RS"));
        assert!(text.contains("[差分]b.rs"));
        assert!(app.file_browser.editing);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.visible(), vec![1]);

        press(&mut app, KeyCode::Char('f'));
        let (tx, rx) = mpsc::channel();
        app.file_browser.paths_job = Some(rx);
        tx.send(Ok(vec!["a.rs".into(), "b.rs".into(), "src/b.rs".into()]))
            .unwrap();
        assert!(app.poll_file_browser());
        assert_eq!(app.file_browser.rows.len(), 2);
        assert!(matches!(app.overlay, Overlay::None));
        press(&mut app, KeyCode::Char('f'));
        assert_eq!(app.visible(), vec![1]);
        assert_eq!(app.file_browser.query, "B.RS");

        press(&mut app, KeyCode::Char('p'));
        assert!(!app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.visible(), vec![0, 1, 2]);
    }

    #[test]
    fn opening_files_keeps_the_tree_available_alongside_diff_or_source() {
        let (_fixture, mut app) = app();
        app.toggle_file_browser();
        let (tx, rx) = mpsc::channel();
        app.file_browser.paths_job = Some(rx);
        tx.send(Ok(vec!["a.rs".into(), "plain.txt".into()]))
            .unwrap();
        assert!(app.poll_file_browser());
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.current(), Some(0));
        assert!(app.focus == Focus::Diff);
        assert!(matches!(app.overlay, Overlay::None));

        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('j'));
        assert!(app.browser_loading_source());
        assert!(app.focus == Focus::Files);
        press(&mut app, KeyCode::Enter);
        assert!(app.focus == Focus::Diff);
        let (tx, rx) = mpsc::channel();
        app.file_browser.source_job = Some(("plain.txt".into(), rx));
        tx.send(Ok("unchanged file".into())).unwrap();
        assert!(app.poll_file_browser());
        assert!(
            matches!(&app.overlay, Overlay::Source(source) if source.from_browser && source.path == "plain.txt")
        );
        press(&mut app, KeyCode::Tab);
        assert!(app.focus == Focus::Files);
        assert!(app.browser_source());
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.file_browser.selected, 0);
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.file_browser.active);
    }
}
