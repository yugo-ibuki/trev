//! Desktop all-files interaction, using the TUI's existing source renderer.
use super::file_tree::FileTree;
use super::*;
use crate::review::all_files::{self, ContentResults};

pub(super) struct AllFiles {
    head: String,
    // Symbols keeps only a Source origin; Ctrl+P can open over any existing overlay.
    origin_focus: Focus,
    origin: Box<Overlay>,
    tab: usize,
    queries: [String; 2],
    selected: [usize; 2],
    paths: Vec<String>,
    tree: FileTree,
    tree_rows: Vec<TreeRow>,
    collapsed: HashSet<String>,
    matching: Vec<usize>,
    paths_job: Option<Job<Vec<String>>>,
    content: Option<ContentResults>,
    content_job: Option<(String, Job<ContentResults>)>,
    submitted: Option<String>,
    source_job: Option<(String, u32, Job<String>)>,
    error: Option<String>,
}

impl AllFiles {
    pub(super) fn active_tab(&self) -> usize {
        self.tab
    }

    pub(super) fn select_row(&mut self, index: usize) {
        self.selected[self.tab] = index.min(self.count().saturating_sub(1));
    }

    fn new(app: &App) -> Self {
        let repository = app.repository_copy();
        let head = app.comparison.head_commit.clone();
        let worker_head = head.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(
                all_files::list_paths(&repository, &worker_head).map_err(|e| format!("{e:#}")),
            );
        });
        Self {
            head,
            origin_focus: app.focus,
            origin: Box::new(Overlay::None),
            tab: 0,
            queries: Default::default(),
            selected: [0, 0],
            paths: Vec::new(),
            tree: FileTree::default(),
            tree_rows: Vec::new(),
            collapsed: HashSet::new(),
            matching: Vec::new(),
            paths_job: Some(rx),
            content: None,
            content_job: None,
            submitted: None,
            source_job: None,
            error: None,
        }
    }

    fn load_paths(&mut self, paths: Vec<String>) {
        self.paths = paths;
        self.tree = FileTree::new(
            self.paths
                .iter()
                .enumerate()
                .map(|(i, path)| (i, path.as_str())),
        );
        self.collapsed = self.tree.folders();
        self.tree_rows = self.tree.rows(&self.collapsed);
        self.filter_paths();
    }

    fn browsing_tree(&self) -> bool {
        self.tab == 0 && self.queries[0].is_empty()
    }

    fn tree_control(&mut self, expand: Option<bool>) {
        let selected = self.selected[0];
        match self.tree_rows.get(selected) {
            Some(TreeRow::Dir { key, .. }) => {
                let open = expand.unwrap_or_else(|| self.collapsed.contains(key));
                if open {
                    self.collapsed.remove(key);
                } else {
                    self.collapsed.insert(key.clone());
                }
                self.tree_rows = self.tree.rows(&self.collapsed);
            }
            Some(TreeRow::File { depth, .. }) if expand == Some(false) && *depth > 0 => {
                if let Some(parent) = self.tree_rows[..selected].iter().rposition(|row| {
                    matches!(row, TreeRow::Dir { depth: parent_depth, .. } if parent_depth + 1 == *depth)
                }) {
                    self.selected[0] = parent;
                }
            }
            _ => {}
        }
    }

    fn filter_paths(&mut self) {
        let query = self.queries[0].to_lowercase();
        self.matching = self
            .paths
            .iter()
            .enumerate()
            .filter(|(_, path)| clean(path).to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect();
    }

    fn count(&self) -> usize {
        if self.browsing_tree() {
            self.tree_rows.len()
        } else if self.tab == 0 {
            self.matching.len()
        } else {
            self.content.as_ref().map_or(0, |c| c.hits.len())
        }
    }

    fn invalidate(&mut self) {
        self.selected[self.tab] = 0;
        self.error = None;
        if self.tab == 0 {
            self.filter_paths();
        }
        if self.tab == 1 {
            self.content = None;
            self.submitted = None;
        }
        self.source_job = None;
    }

    fn target(&self) -> Option<(String, Option<u32>)> {
        let selected = self.selected[self.tab];
        if self.browsing_tree() {
            match self.tree_rows.get(selected)? {
                TreeRow::File { index, .. } => Some((self.paths[*index].clone(), None)),
                TreeRow::Dir { .. } => None,
            }
        } else if self.tab == 0 {
            self.matching
                .get(selected)
                .map(|&index| (self.paths[index].clone(), None))
        } else {
            self.content
                .as_ref()?
                .hits
                .get(selected)
                .map(|hit| (hit.path.clone(), Some(hit.line)))
        }
    }
}

impl App {
    pub(super) fn open_all_files(&mut self) {
        if matches!(self.overlay, Overlay::AllFiles(_)) {
            return;
        }
        let mut search = AllFiles::new(self);
        search.origin = Box::new(std::mem::replace(&mut self.overlay, Overlay::None));
        self.overlay = Overlay::AllFiles(Box::new(search));
    }

    pub(super) fn restore_all_files_origin(&mut self, search: Box<AllFiles>) {
        self.focus = search.origin_focus;
        self.overlay = *search.origin;
    }

    pub(super) fn poll_all_files(&mut self) -> bool {
        let Overlay::AllFiles(mut search) = std::mem::replace(&mut self.overlay, Overlay::None)
        else {
            unreachable!()
        };
        if search.head != self.comparison.head_commit {
            return true;
        }
        let mut changed = false;
        if let Some(result) = search.paths_job.as_ref().and_then(job_result) {
            search.paths_job = None;
            match result {
                Ok(paths) => {
                    search.load_paths(paths);
                }
                Err(error) => search.error = Some(format!("ファイル一覧を読み込めません: {error}")),
            }
            changed = true;
        }
        if let Some(result) = search
            .content_job
            .as_ref()
            .and_then(|(_, job)| job_result(job))
        {
            let (query, _) = search.content_job.take().unwrap();
            if search.submitted.as_ref() == Some(&query) && search.queries[1] == query {
                match result {
                    Ok(content) => search.content = Some(content),
                    Err(error) => {
                        search.submitted = None;
                        search.error = Some(format!("検索できません: {error}"));
                    }
                }
            }
            changed = true;
        }
        if let Some(result) = search
            .source_job
            .as_ref()
            .and_then(|(_, _, job)| job_result(job))
        {
            let (path, line, _) = search.source_job.take().unwrap();
            match result {
                Ok(text) => {
                    let mut source = Source::new(search.head.clone(), path, line, text);
                    source.all_files = Some(search);
                    self.focus = Focus::Diff;
                    self.overlay = Overlay::Source(source);
                    return true;
                }
                Err(error) => search.error = Some(format!("ソースを開けません: {error}")),
            }
            changed = true;
        }
        self.overlay = Overlay::AllFiles(search);
        changed
    }

    pub(super) fn all_files_key(&mut self, key: KeyEvent, mut search: Box<AllFiles>) {
        match key.code {
            KeyCode::Esc => {
                self.restore_all_files_origin(search);
                return;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                search.tab = 1 - search.tab;
                search.source_job = None;
                search.error = None;
                if search.content_job.is_some() {
                    search.submitted = None;
                }
            }
            KeyCode::Up => {
                search.selected[search.tab] = search.selected[search.tab].saturating_sub(1)
            }
            KeyCode::Down => {
                search.selected[search.tab] =
                    shifted(search.selected[search.tab], 1, search.count())
            }
            KeyCode::PageUp => {
                search.selected[search.tab] =
                    search.selected[search.tab].saturating_sub(self.page_height)
            }
            KeyCode::PageDown => {
                search.selected[search.tab] = shifted(
                    search.selected[search.tab],
                    self.page_height as isize,
                    search.count(),
                )
            }
            KeyCode::Left if search.browsing_tree() => search.tree_control(Some(false)),
            KeyCode::Right if search.browsing_tree() => search.tree_control(Some(true)),
            KeyCode::Home => search.selected[search.tab] = 0,
            KeyCode::End => search.selected[search.tab] = search.count().saturating_sub(1),
            KeyCode::Backspace => {
                search.queries[search.tab].pop();
                search.invalidate();
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !c.is_control()
                    && search.queries[search.tab].len() < 4096 =>
            {
                search.queries[search.tab].push(c);
                search.invalidate();
            }
            KeyCode::Enter => {
                if search.browsing_tree()
                    && matches!(
                        search.tree_rows.get(search.selected[0]),
                        Some(TreeRow::Dir { .. })
                    )
                {
                    search.tree_control(None);
                    self.overlay = Overlay::AllFiles(search);
                    return;
                }
                if search.source_job.is_some() || (search.tab == 1 && search.content_job.is_some())
                {
                    self.overlay = Overlay::AllFiles(search);
                    return;
                }
                if search.tab == 1
                    && !search.queries[1].is_empty()
                    && search.submitted.as_ref() != Some(&search.queries[1])
                {
                    let query = search.queries[1].clone();
                    let head = search.head.clone();
                    let repository = self.repository_copy();
                    let (tx, rx) = mpsc::channel();
                    search.submitted = Some(query.clone());
                    search.error = None;
                    std::thread::spawn(move || {
                        let _ = tx.send(
                            all_files::search_content(&repository, &head, &query)
                                .map_err(|e| format!("{e:#}")),
                        );
                    });
                    search.content_job = Some((search.queries[1].clone(), rx));
                } else if let Some((path, line)) = search.target() {
                    if self.open_all_files_diff(&path, line) {
                        return;
                    }
                    let head = search.head.clone();
                    let worker_path = path.clone();
                    let repository = self.repository_copy();
                    let (tx, rx) = mpsc::channel();
                    std::thread::spawn(move || {
                        let _ = tx.send(
                            repository
                                .file_text(&head, &worker_path)
                                .map_err(|e| format!("{e:#}")),
                        );
                    });
                    search.source_job = Some((path, line.unwrap_or(1), rx));
                }
            }
            _ => {}
        }
        self.overlay = Overlay::AllFiles(search);
    }

    pub(super) fn open_all_files_diff(&mut self, path: &str, line: Option<u32>) -> bool {
        let Some(index) = self
            .comparison
            .files
            .iter()
            .position(|file| file.status != 'D' && file.new_path == path)
        else {
            return false;
        };
        let Some(selected) = self.visible().iter().position(|&visible| visible == index) else {
            return false;
        };
        let offset = match line {
            Some(line) => match self
                .display_file(index)
                .lines
                .iter()
                .position(|row| row.new_line == Some(line))
            {
                Some(offset) => offset,
                None => return false,
            },
            None => 0,
        };
        self.selected = selected;
        self.reveal_file(index);
        self.cursor = Default::default();
        self.diff_offset = offset;
        self.horizontal = 0;
        true
    }

    pub(super) fn draw_all_files(&self, frame: &mut Frame, search: &AllFiles) {
        let area = popup(frame.area(), 96, 94);
        frame.render_widget(Clear, area);
        let short: String = search.head.chars().take(10).collect();
        frame.render_widget(
            panel(
                &format!(
                    "全ファイル head:{}  {}件 / {}ファイル",
                    clean(&short),
                    search.count(),
                    search.paths.len()
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
            Constraint::Length(3),
        ])
        .split(area.inner(Margin::new(2, 1)));
        self.draw_click_tabs(frame, regions[0], &["パス", "中身"], search.tab);
        frame.render_widget(
            Paragraph::new(format!("検索: {}", clean(&search.queries[search.tab]))),
            regions[1],
        );
        let message = if let Some(error) = &search.error {
            Some(clean(error))
        } else if search.source_job.is_some() {
            Some("ソースを読み込み中… Esc:閉じる".into())
        } else if search.paths_job.is_some() {
            Some("ファイル一覧を読み込み中… Esc:閉じる".into())
        } else if search.tab == 1 && search.content_job.is_some() {
            Some("中身を検索中… Esc:閉じる".into())
        } else if search.tab == 1 && search.queries[1].is_empty() {
            Some("検索語を入力してEnterで中身を検索してください".into())
        } else if search.tab == 1 && !search.queries[1].is_empty() && search.submitted.is_none() {
            Some("Enterで中身を検索（大文字・小文字を区別しない文字列検索）".into())
        } else if search.count() == 0 {
            Some("一致するファイル・行はありません".into())
        } else {
            None
        };
        if let Some(message) = message {
            frame.render_widget(
                Paragraph::new(message).wrap(Wrap { trim: false }),
                regions[2],
            );
        } else {
            // Build widgets only for the visible page; arrow keys do not allocate a
            // complete list for large repositories or copy giant matching lines.
            let height = usize::from(regions[2].height).max(1);
            let selected = search.selected[search.tab];
            let start = selected
                .saturating_sub(height / 2)
                .min(search.count().saturating_sub(height));
            let path_label = |index: usize, basename: bool| {
                let path = &search.paths[index];
                let label = if basename {
                    path.rsplit('/').next().unwrap_or_default()
                } else {
                    path
                };
                let status = self
                    .comparison
                    .files
                    .iter()
                    .find(|file| file.status != 'D' && file.new_path == *path)
                    .map(|file| file.status);
                format!("{} {}", status.unwrap_or(' '), clean(label))
            };
            let items: Vec<ListItem> = if search.browsing_tree() {
                search
                    .tree_rows
                    .iter()
                    .skip(start)
                    .take(height)
                    .map(|row| {
                        let (depth, label) = match row {
                            TreeRow::Dir { depth, key, label } => (
                                *depth,
                                format!(
                                    "{} {}/",
                                    if search.collapsed.contains(key) {
                                        "▸"
                                    } else {
                                        "▾"
                                    },
                                    clean(label)
                                ),
                            ),
                            TreeRow::File { depth, index } => (*depth, path_label(*index, true)),
                        };
                        let indent = depth
                            .saturating_mul(2)
                            .min(usize::from(regions[2].width.saturating_sub(12)));
                        ListItem::new(format!("{}{label}", " ".repeat(indent)))
                    })
                    .collect()
            } else if search.tab == 0 {
                search
                    .matching
                    .iter()
                    .skip(start)
                    .take(height)
                    .map(|&index| ListItem::new(path_label(index, false)))
                    .collect()
            } else {
                search
                    .content
                    .as_ref()
                    .map(|content| {
                        content
                            .hits
                            .iter()
                            .skip(start)
                            .take(height)
                            .map(|hit| {
                                let snippet: String = hit.text.chars().take(300).collect();
                                ListItem::new(format!(
                                    "{}:{}  {}",
                                    clean(&hit.path),
                                    hit.line,
                                    clean(&snippet)
                                ))
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            self.register_list_rows(regions[2], start, items.len());
            let mut state =
                ListState::default().with_selected(Some(selected.saturating_sub(start)));
            frame.render_stateful_widget(
                List::new(items).highlight_style(selected_style()),
                regions[2],
                &mut state,
            );
        }
        let buttons = if search.tab == 1 {
            vec![
                (
                    if search.count() > 0 {
                        "開く"
                    } else {
                        "検索"
                    },
                    MouseAction::Key(KeyCode::Enter),
                ),
                ("戻る", MouseAction::Key(KeyCode::Esc)),
            ]
        } else {
            vec![("戻る", MouseAction::Key(KeyCode::Esc))]
        };
        self.draw_click_buttons(frame, regions[3], &buttons);
        let truncated = if search.tab == 1 && search.content.as_ref().is_some_and(|c| c.truncated) {
            "結果は一部のみです。検索語を絞ってください。\n"
        } else {
            ""
        };
        frame.render_widget(Paragraph::new(format!("{truncated}Tab:パス/中身 ↑↓ PgUp/PgDn Home/End:移動\n←→:フォルダ開閉 Enter:開閉/検索/開く Esc:開く前へ Ctrl+C:終了")).wrap(Wrap { trim: false }), regions[4]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::all_files::ContentHit;

    fn search(app: &App) -> AllFiles {
        let mut state = AllFiles {
            head: app.comparison.head_commit.clone(),
            origin_focus: app.focus,
            origin: Box::new(Overlay::None),
            tab: 0,
            queries: Default::default(),
            selected: [0, 0],
            paths: vec!["a.rs".into(), "unchanged.rs".into(), "z.rs".into()],
            tree: FileTree::default(),
            tree_rows: Vec::new(),
            collapsed: HashSet::new(),
            matching: Vec::new(),
            paths_job: None,
            content: None,
            content_job: None,
            submitted: None,
            source_job: None,
            error: None,
        };
        state.load_paths(state.paths.clone());
        state
    }

    fn press(app: &mut App, code: KeyCode) {
        assert!(!app.key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    #[test]
    fn mouse_row_selection_is_bounded_to_the_active_tab() {
        let (_fixture, app) = super::super::tests::app();
        let mut state = search(&app);
        assert_eq!(state.active_tab(), 0);
        state.select_row(usize::MAX);
        assert_eq!(state.selected, [2, 0]);
        state.tab = 1;
        assert_eq!(state.active_tab(), 1);
        state.select_row(usize::MAX);
        assert_eq!(state.selected, [2, 0]);
        state.content = Some(ContentResults {
            hits: vec![ContentHit {
                path: "unchanged.rs".into(),
                line: 1,
                text: "needle".into(),
            }],
            truncated: false,
        });
        state.select_row(usize::MAX);
        assert_eq!(state.target(), Some(("unchanged.rs".into(), Some(1))));
    }

    #[test]
    fn empty_path_query_browses_closed_folders_and_search_returns_to_tree() {
        let (_fixture, mut app) = super::super::tests::app();
        let mut state = search(&app);
        state.load_paths(vec![
            "src/nested/a.rs".into(),
            "src/unchanged.rs".into(),
            "root.rs".into(),
        ]);
        assert_eq!(state.count(), 2);
        assert!(state.collapsed.contains("src"));
        assert!(state.collapsed.contains("src/nested"));
        assert!(state.target().is_none());
        app.overlay = Overlay::AllFiles(Box::new(state));
        press(&mut app, KeyCode::Enter);
        let Overlay::AllFiles(state) = &app.overlay else {
            panic!()
        };
        assert_eq!(state.count(), 4);
        assert!(state.source_job.is_none());
        assert!(!state.collapsed.contains("src"));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Down);
        let Overlay::AllFiles(state) = &app.overlay else {
            panic!()
        };
        assert_eq!(state.target(), Some(("src/nested/a.rs".into(), None)));
        press(&mut app, KeyCode::Left);
        let Overlay::AllFiles(state) = &app.overlay else {
            panic!()
        };
        assert_eq!(state.selected[0], 1);
        press(&mut app, KeyCode::Left);
        let Overlay::AllFiles(state) = &app.overlay else {
            panic!()
        };
        assert!(state.collapsed.contains("src/nested"));
        for c in "unchanged".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        let Overlay::AllFiles(state) = &app.overlay else {
            panic!()
        };
        assert_eq!(state.count(), 1);
        assert_eq!(state.target(), Some(("src/unchanged.rs".into(), None)));
        for _ in "unchanged".chars() {
            press(&mut app, KeyCode::Backspace);
        }
        let Overlay::AllFiles(state) = &app.overlay else {
            panic!()
        };
        assert!(state.browsing_tree());
        assert_eq!(state.selected[0], 0);
        assert_eq!(state.count(), 4);
        assert!(state.collapsed.contains("src/nested"));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn tree_paths_loaded_asynchronously_render_changed_status_and_unchanged_files() {
        use ratatui::backend::TestBackend;
        let (_fixture, mut app) = super::super::tests::app();
        let mut state = search(&app);
        let (tx, rx) = mpsc::channel();
        state.paths_job = Some(rx);
        tx.send(Ok(vec!["a.rs".into(), "src/unchanged.rs".into()]))
            .unwrap();
        app.overlay = Overlay::AllFiles(Box::new(state));
        assert!(app.poll_all_files());
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("▸ src/"));
        assert!(rendered.contains("M a.rs"));
        assert!(!rendered.contains("unchanged.rs"));
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Down);
        let Overlay::AllFiles(state) = &app.overlay else {
            panic!()
        };
        assert_eq!(state.target(), Some(("src/unchanged.rs".into(), None)));
    }

    #[test]
    fn shortcuts_tabs_queries_and_empty_content_are_distinct_from_diff_search() {
        let (_fixture, mut app) = super::super::tests::app();
        app.focus = Focus::Files;
        press(&mut app, KeyCode::Char('f'));
        assert!(matches!(app.overlay, Overlay::AllFiles(_)));
        press(&mut app, KeyCode::Esc);
        assert!(!app.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)));
        assert!(matches!(app.overlay, Overlay::AllFiles(_)));
        app.overlay = Overlay::AllFiles(Box::new(search(&app)));
        for c in "UNCHANGED".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        let Overlay::AllFiles(state) = &app.overlay else {
            panic!()
        };
        assert_eq!(state.target(), Some(("unchanged.rs".into(), None)));
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Enter);
        let Overlay::AllFiles(state) = &app.overlay else {
            panic!()
        };
        assert_eq!(state.count(), 0);
        assert!(state.source_job.is_none());
        assert!(state.content_job.is_none());
        press(&mut app, KeyCode::Char('q'));
        press(&mut app, KeyCode::BackTab);
        let Overlay::AllFiles(state) = &app.overlay else {
            panic!()
        };
        assert_eq!(state.queries, ["UNCHANGED", "q"]);
        assert!(app.query.is_empty());
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('/'));
        assert!(matches!(app.overlay, Overlay::Search(_)));
    }

    #[test]
    fn changed_files_use_original_new_line_numbers_and_preserve_viewed_filter() {
        let (_fixture, mut app) = super::super::tests::app();
        assert!(app.open_all_files_diff("a.rs", Some(12)));
        assert_eq!(app.diff_offset, 1); // Removed old line 12 is not the new line 12.
        assert!(app.open_all_files_diff("a.rs", Some(13)));
        assert_eq!(app.diff_offset, 2);
        assert!(!app.open_all_files_diff("a.rs", Some(200)));
        assert!(!app.open_all_files_diff("unchanged.rs", None));
        app.only_unviewed = true;
        let viewed_path = app.repository.root.join("viewed.json");
        let data = serde_json::json!({ "comparisons": [{
            "repository": app.repository.root,
            "base": app.comparison.base_key,
            "head": app.comparison.head_key,
            "files": { "a.rs": "digest" }
        }] });
        std::fs::write(&viewed_path, serde_json::to_vec(&data).unwrap()).unwrap();
        app.viewed = ViewedStore::load_at(app.repository.root.clone(), viewed_path).unwrap();
        assert!(!app.open_all_files_diff("a.rs", None));
        assert!(app.only_unviewed);
        assert!(app
            .viewed
            .is_viewed(&app.comparison, &app.comparison.files[0]));
    }

    #[test]
    fn async_source_returns_to_exact_search_then_original_diff() {
        let (_fixture, mut app) = super::super::tests::app();
        app.focus = Focus::Diff;
        app.selected = 1;
        app.diff_offset = 2;
        app.horizontal = 8;
        app.only_unviewed = true;
        let mut state = search(&app);
        state.tab = 1;
        state.queries[1] = "needle".into();
        state.submitted = Some("needle".into());
        state.selected[1] = 1;
        state.content = Some(ContentResults {
            hits: vec![
                ContentHit {
                    path: "unchanged.rs".into(),
                    line: 1,
                    text: "needle".into(),
                },
                ContentHit {
                    path: "unchanged.rs".into(),
                    line: 3,
                    text: "needle".into(),
                },
            ],
            truncated: true,
        });
        let (tx, rx) = mpsc::channel();
        state.source_job = Some(("unchanged.rs".into(), 3, rx));
        tx.send(Ok("one\ntwo\nneedle\n".into())).unwrap();
        app.overlay = Overlay::AllFiles(Box::new(state));
        assert!(app.poll_all_files());
        let Overlay::Source(source) = &app.overlay else {
            panic!()
        };
        assert_eq!(source.matched, 2);
        assert_eq!(source.offset, 2);
        press(&mut app, KeyCode::Char('q'));
        let Overlay::AllFiles(state) = &app.overlay else {
            panic!()
        };
        assert_eq!(state.tab, 1);
        assert_eq!(state.queries[1], "needle");
        assert_eq!(state.selected[1], 1);
        assert!(state.content.as_ref().unwrap().truncated);
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!((app.selected, app.diff_offset, app.horizontal), (1, 2, 8));
        assert!(app.only_unviewed);

        let mut source = Source::new(
            "headcommit".into(),
            "unchanged.rs".into(),
            3,
            "one\ntwo\nneedle\n".into(),
        );
        source.all_files = Some(Box::new(search(&app)));
        app.overlay = Overlay::Source(source);
        app.focus = Focus::Diff;
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.focus == Focus::Diff);
        assert_eq!((app.selected, app.diff_offset, app.horizontal), (1, 2, 8));
    }

    #[test]
    fn closing_search_and_inline_result_restores_both_origin_panes() {
        for focus in [Focus::Files, Focus::Diff] {
            let (_fixture, mut app) = super::super::tests::app();
            app.focus = focus;
            app.selected = 1;
            app.diff_offset = 2;
            app.horizontal = 8;
            app.cursor.position = Some(cursor::Position { row: 2, byte: 1 });
            app.cursor.anchor = app.cursor.position;
            app.overlay = Overlay::AllFiles(Box::new(search(&app)));
            press(&mut app, KeyCode::Tab);
            press(&mut app, KeyCode::BackTab);
            assert_eq!(app.cursor.anchor, app.cursor.position);
            press(&mut app, KeyCode::Esc);
            assert!(app.focus == focus);
            assert_eq!(app.cursor.anchor, app.cursor.position);

            let mut source = Source::new(
                "headcommit".into(),
                "unchanged.rs".into(),
                1,
                "needle".into(),
            );
            source.all_files = Some(Box::new(search(&app)));
            app.overlay = Overlay::Source(source);
            app.focus = Focus::Diff;
            press(&mut app, KeyCode::Char('q'));
            assert!(matches!(app.overlay, Overlay::AllFiles(_)));
            press(&mut app, KeyCode::Esc);
            assert!(app.focus == focus);
            assert_eq!((app.selected, app.diff_offset, app.horizontal), (1, 2, 8));

            let mut source = Source::new(
                "headcommit".into(),
                "unchanged.rs".into(),
                1,
                "needle".into(),
            );
            source.all_files = Some(Box::new(search(&app)));
            app.overlay = Overlay::Source(source);
            app.focus = Focus::Diff;
            press(&mut app, KeyCode::Esc);
            assert!(matches!(app.overlay, Overlay::None));
            assert!(app.focus == focus);
            assert_eq!(app.cursor.anchor, app.cursor.position);
            assert_eq!((app.selected, app.diff_offset, app.horizontal), (1, 2, 8));
        }
    }

    #[test]
    fn ctrl_p_and_name_picker_restore_the_original_source() {
        let (_fixture, mut app) = super::super::tests::app();
        app.focus = Focus::Diff;
        let mut source = Source::new(
            "headcommit".into(),
            "unchanged.rs".into(),
            2,
            "one\nneedle\nthree".into(),
        );
        source.horizontal = 7;
        source.cursor.position = Some(cursor::Position { row: 1, byte: 2 });
        source.cursor.anchor = source.cursor.position;
        app.overlay = Overlay::Source(source);
        for open_result in [false, true] {
            assert!(!app.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)));
            press(&mut app, KeyCode::Tab);
            press(&mut app, KeyCode::Char('x'));
            assert!(!app.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)));
            assert!(
                matches!(&app.overlay, Overlay::AllFiles(search) if search.tab == 1 && search.queries[1] == "x")
            );
            if open_result {
                let Overlay::AllFiles(search) = std::mem::replace(&mut app.overlay, Overlay::None)
                else {
                    panic!()
                };
                let mut result =
                    Source::new("headcommit".into(), "result.rs".into(), 1, "result".into());
                result.all_files = Some(search);
                app.overlay = Overlay::Source(result);
            }
            press(&mut app, KeyCode::Esc);
            let Overlay::Source(source) = &app.overlay else {
                panic!()
            };
            assert_eq!(source.path, "unchanged.rs");
            assert_eq!((source.offset, source.horizontal), (1, 7));
            assert_eq!(
                source.cursor.position,
                Some(cursor::Position { row: 1, byte: 2 })
            );
            assert_eq!(source.cursor.anchor, source.cursor.position);
            assert!(app.focus == Focus::Diff);
        }
        // The name picker already owns its source; Esc must reach that handler.
        press(&mut app, KeyCode::Char('s'));
        assert!(matches!(app.overlay, Overlay::SymbolPicker(_)));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(&app.overlay, Overlay::Source(source) if source.path == "unchanged.rs"));
        assert!(app.focus == Focus::Diff);
    }

    #[test]
    fn editing_keeps_one_job_and_discards_stale_results_and_changed_heads() {
        let (_fixture, mut app) = super::super::tests::app();
        let mut state = search(&app);
        state.tab = 1;
        state.queries[1] = "old".into();
        state.submitted = Some("old".into());
        let (tx, rx) = mpsc::channel();
        state.content_job = Some(("old".into(), rx));
        app.overlay = Overlay::AllFiles(Box::new(state));
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Enter);
        let Overlay::AllFiles(state) = &app.overlay else {
            panic!()
        };
        assert!(state.content_job.is_some());
        assert!(state.submitted.is_none());
        tx.send(Ok(ContentResults {
            hits: vec![],
            truncated: true,
        }))
        .unwrap();
        assert!(app.poll_all_files());
        let Overlay::AllFiles(state) = &app.overlay else {
            panic!()
        };
        assert!(state.content.is_none());
        assert!(state.content_job.is_none());
        app.comparison.head_commit = "other-head".into();
        assert!(app.poll_all_files());
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn failed_content_search_can_retry_same_query() {
        let (_fixture, mut app) = super::super::tests::app();
        let mut state = search(&app);
        state.tab = 1;
        state.queries[1] = "needle".into();
        state.submitted = None;
        state.error = Some("failed".into());
        app.overlay = Overlay::AllFiles(Box::new(state));
        press(&mut app, KeyCode::Enter);
        let Overlay::AllFiles(state) = &app.overlay else {
            panic!()
        };
        assert!(state.content_job.is_some());
        assert!(state.error.is_none());
    }

    #[test]
    fn all_files_results_fit_tiny_windows_and_sanitize_untrusted_text() {
        use ratatui::backend::TestBackend;
        let (_fixture, mut app) = super::super::tests::app();
        for width in [1, 8, 40, 100] {
            for height in [1, 5, 12, 30] {
                let mut state = search(&app);
                state.paths.push("bad\x1b[2J\tpath".into());
                state.load_paths(state.paths.clone());
                app.overlay = Overlay::AllFiles(Box::new(state));
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| app.draw(frame)).unwrap();
                assert!(terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .all(|cell| !cell.symbol().contains('\x1b')));
            }
        }
    }
}
