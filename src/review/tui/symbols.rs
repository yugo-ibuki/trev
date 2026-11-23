//! 名前検索は既存の検索結果・ソース描画を使い、戻る場所を保持する。
use super::*;
use crate::review::{all_files::ContentResults, symbols};

pub(super) struct Symbols {
    commit: String,
    path: String,
    mode: SymbolMode,
    query: String,
    selected: usize,
    submitted: Option<(SymbolMode, String)>,
    results: Option<ContentResults>,
    job: Option<(SymbolMode, String, Job<ContentResults>)>,
    source_job: Option<(String, u32, Job<String>)>,
    origin: Option<Box<Source>>,
    error: Option<String>,
}

// Desktop review_jump::ident_at はマウスの文字位置用。端末では行の候補を順に選ぶため、
// 検索側の字句処理でコメント・文字列を除き、バイト範囲をそのまま強調表示に使う。
pub(super) struct SymbolPicker {
    pub(super) lines: Vec<PickLine>,
    selected: usize,
    token: usize,
    commit: String,
    path: String,
    old: Option<(String, String)>,
    pub(super) origin: Option<Box<Source>>,
}

pub(super) struct PickLine {
    pub(super) text: String,
    ranges: Vec<std::ops::Range<usize>>,
    removed: bool,
    number: u32,
}

impl PickLine {
    fn new(text: &str, path: &str, removed: bool, number: u32) -> Self {
        // Sanitize before computing ranges so byte positions always match the rendered text.
        let text = clean(text);
        let ranges = symbols::pickable_identifiers(&text, path);
        Self {
            text,
            ranges,
            removed,
            number,
        }
    }
}

impl SymbolPicker {
    fn selected_name(&self) -> Option<&str> {
        let line = self.lines.get(self.selected)?;
        let range = line.ranges.get(self.token)?;
        Some(&line.text[range.clone()])
    }

    pub(super) fn snapshot(&self) -> (&str, &str) {
        self.snapshot_for_row(self.selected)
    }

    pub(super) fn snapshot_for_row(&self, row: usize) -> (&str, &str) {
        if self.lines.get(row).is_some_and(|line| line.removed) {
            if let Some((commit, path)) = &self.old {
                return (commit, path);
            }
        }
        (&self.commit, &self.path)
    }
}

impl Symbols {
    pub(super) fn preview_target(&self) -> Option<super::result_preview::Target> {
        let hit = self.results.as_ref()?.hits.get(self.selected)?;
        Some(super::result_preview::Target::new(
            &self.commit,
            &hit.path,
            hit.line,
        ))
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn active_tab(&self) -> usize {
        usize::from(self.mode == SymbolMode::Usages)
    }

    pub(super) fn select_row(&mut self, index: usize) {
        self.selected = index.min(self.count().saturating_sub(1));
    }

    fn new(commit: String, path: String, mode: SymbolMode, origin: Option<Box<Source>>) -> Self {
        Self {
            commit,
            path,
            mode,
            origin,
            query: String::new(),
            selected: 0,
            submitted: None,
            results: None,
            job: None,
            source_job: None,
            error: None,
        }
    }

    fn invalidate(&mut self) {
        self.selected = 0;
        self.submitted = None;
        self.results = None;
        self.source_job = None;
        self.error = None;
    }

    fn count(&self) -> usize {
        self.results
            .as_ref()
            .map_or(0, |results| results.hits.len())
    }
}

impl App {
    pub(super) fn search_symbol(
        &mut self,
        commit: String,
        path: String,
        name: String,
        mode: SymbolMode,
        origin: Option<Box<Source>>,
    ) {
        let mut search = Symbols::new(commit, path, mode, origin);
        search.query = name;
        self.submit_symbols(&mut search);
        self.overlay = Overlay::Symbols(Box::new(search));
    }

    fn submit_symbols(&self, search: &mut Symbols) {
        let repository = self.repository_copy();
        let commit = search.commit.clone();
        let path = search.path.clone();
        let query = search.query.clone();
        let mode = search.mode;
        let (tx, rx) = mpsc::channel();
        search.submitted = Some((mode, query.clone()));
        search.error = None;
        std::thread::spawn(move || {
            let _ = tx.send(
                symbols::find(&repository, &commit, &path, &query, mode)
                    .map_err(|error| format!("{error:#}")),
            );
        });
        search.job = Some((mode, search.query.clone(), rx));
    }

    pub(super) fn open_diff_symbol_picker(&mut self) {
        let Some(index) = self.current() else { return };
        let file = self.display_file(index);
        let selected = file
            .lines
            .iter()
            .take(self.diff_offset)
            .filter(|line| line.kind != LineKind::Header)
            .count();
        let lines: Vec<_> = file
            .lines
            .iter()
            .filter(|line| line.kind != LineKind::Header)
            .map(|line| {
                let removed = line.kind == LineKind::Removed;
                let path = if removed {
                    &file.old_path
                } else {
                    &file.new_path
                };
                PickLine::new(
                    &line.text,
                    path,
                    removed,
                    if removed {
                        line.old_line
                    } else {
                        line.new_line
                    }
                    .unwrap_or(0),
                )
            })
            .collect();
        let selected = selected.min(lines.len().saturating_sub(1));
        let (commit, path) = file.source(&self.comparison);
        self.overlay = Overlay::SymbolPicker(Box::new(SymbolPicker {
            lines,
            selected,
            token: 0,
            commit: commit.into(),
            path: path.into(),
            old: Some((self.comparison.merge_base.clone(), file.old_path.clone())),
            origin: None,
        }));
    }

    pub(super) fn open_source_symbol_picker(&mut self, source: Source) {
        let lines = source
            .lines
            .iter()
            .enumerate()
            .map(|(index, text)| PickLine::new(text, &source.path, false, (index + 1) as u32))
            .collect();
        let selected = source.offset.min(source.lines.len().saturating_sub(1));
        self.overlay = Overlay::SymbolPicker(Box::new(SymbolPicker {
            lines,
            selected,
            token: 0,
            commit: source.commit.clone(),
            path: source.path.clone(),
            old: None,
            origin: Some(Box::new(source)),
        }));
    }

    pub(super) fn symbol_picker_key(&mut self, key: KeyEvent, mut picker: Box<SymbolPicker>) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.overlay = picker
                    .origin
                    .take()
                    .map_or(Overlay::None, |source| Overlay::Source(*source));
                return;
            }
            KeyCode::Up | KeyCode::Down => {
                let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                picker.selected = shifted(picker.selected, delta, picker.lines.len());
                picker.token = 0;
            }
            KeyCode::Left => picker.token = picker.token.saturating_sub(1),
            KeyCode::Right => {
                let count = picker
                    .lines
                    .get(picker.selected)
                    .map_or(0, |line| line.ranges.len());
                picker.token = shifted(picker.token, 1, count);
            }
            KeyCode::Enter | KeyCode::Char('d') | KeyCode::Char('R') => {
                if let Some(name) = picker.selected_name().map(str::to_owned) {
                    let mode = if key.code == KeyCode::Char('R') {
                        SymbolMode::Usages
                    } else {
                        SymbolMode::Definitions
                    };
                    let (commit, path) = picker.snapshot();
                    self.search_symbol(
                        commit.into(),
                        path.into(),
                        name,
                        mode,
                        picker.origin.take(),
                    );
                    return;
                }
            }
            _ => {}
        }
        self.overlay = Overlay::SymbolPicker(picker);
    }

    pub(super) fn draw_symbol_picker(&self, frame: &mut Frame, picker: &SymbolPicker, area: Rect) {
        frame.render_widget(Clear, area);
        frame.render_widget(panel("コード内の名前を選ぶ", true), area);
        let regions = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area.inner(Margin::new(2, 1)));
        let (commit, path) = picker.snapshot();
        frame.render_widget(
            Paragraph::new(format!(
                "{}  {}",
                clean(path),
                clean(&commit.chars().take(10).collect::<String>())
            )),
            regions[0],
        );
        let page = usize::from(regions[1].height).max(1);
        let start = picker
            .selected
            .saturating_sub(page / 2)
            .min(picker.lines.len().saturating_sub(page));
        let rows: Vec<Line> = picker
            .lines
            .iter()
            .enumerate()
            .skip(start)
            .take(page)
            .map(|(index, line)| {
                let mut spans = vec![Span::raw(format!(
                    "{} {:>5} {}",
                    if index == picker.selected { "▶" } else { " " },
                    line.number,
                    if line.removed { "-" } else { " " }
                ))];
                if index == picker.selected {
                    if let Some(range) = line.ranges.get(picker.token) {
                        spans.push(Span::raw(line.text[..range.start].to_owned()));
                        spans.push(Span::styled(
                            line.text[range.clone()].to_owned(),
                            selected_style(),
                        ));
                        spans.push(Span::raw(line.text[range.end..].to_owned()));
                    } else {
                        spans.push(Span::styled(
                            line.text.clone(),
                            Style::default().fg(Color::Yellow),
                        ));
                    }
                } else {
                    spans.push(Span::raw(line.text.clone()));
                }
                Line::from(spans)
            })
            .collect();
        // Keep the chosen name visible even on a long code line.
        let horizontal = picker
            .lines
            .get(picker.selected)
            .and_then(|line| {
                line.ranges.get(picker.token).map(|range| {
                    let prefix = Line::from(format!(
                        "▶ {:>5} {}{}",
                        line.number,
                        if line.removed { "-" } else { " " },
                        &line.text[..range.start]
                    ))
                    .width();
                    let token_width = Span::raw(&line.text[range.clone()]).width();
                    prefix
                        .saturating_add(token_width)
                        .saturating_sub(usize::from(regions[1].width))
                        .min(prefix)
                        .min(u16::MAX as usize) as u16
                })
            })
            .unwrap_or(0);
        let prefixes = picker
            .lines
            .iter()
            .enumerate()
            .skip(start)
            .take(rows.len())
            .map(|(index, line)| {
                format!(
                    "{} {:>5} {}",
                    if index == picker.selected { "▶" } else { " " },
                    line.number,
                    if line.removed { "-" } else { " " }
                )
                .len()
            })
            .collect();
        self.record_code_viewport(regions[1], start, horizontal, &rows, prefixes);
        frame.render_widget(Paragraph::new(rows).scroll((0, horizontal)), regions[1]);
        let name = picker
            .selected_name()
            .unwrap_or("この行に選べる名前はありません");
        frame.render_widget(
            Paragraph::new(format!(
                "選択: {name}\n↑↓:行 ←→:名前 Enter/d:定義 Shift+R:使用箇所 Esc:戻る"
            ))
            .wrap(Wrap { trim: false }),
            regions[2],
        );
        self.draw_click_buttons(
            frame,
            regions[3],
            &[("戻る", MouseAction::Key(KeyCode::Char('q')))],
        );
    }

    pub(super) fn open_diff_symbols(&mut self, mode: SymbolMode) {
        let Some(index) = self.current() else { return };
        let file = self.display_file(index);
        let selection = (self.focus == Focus::Diff)
            .then(|| self.cursor.selected_diff_identifier(&file.lines))
            .flatten();
        let line = selection
            .as_ref()
            .and_then(|(row, _)| file.lines.get(*row))
            .or_else(|| {
                file.lines
                    .iter()
                    .skip(self.diff_offset)
                    .find(|line| line.kind != LineKind::Header)
            });
        let (commit, path) = if line.is_some_and(|line| line.kind == LineKind::Removed) {
            (self.comparison.merge_base.as_str(), file.old_path.as_str())
        } else if selection.is_some() {
            (self.comparison.head_commit.as_str(), file.new_path.as_str())
        } else {
            file.source(&self.comparison)
        };
        let mut search = Symbols::new(commit.into(), path.into(), mode, None);
        if let Some((_, query)) = selection {
            search.query = query;
        }
        self.overlay = Overlay::Symbols(Box::new(search));
    }

    pub(super) fn open_source_symbols(&mut self, source: Source, mode: SymbolMode) {
        let query = if self.focus == Focus::Diff {
            source.cursor.selected_source_identifier(&source.lines)
        } else {
            None
        };
        let mut search = Symbols::new(
            source.commit.clone(),
            source.path.clone(),
            mode,
            Some(Box::new(source)),
        );
        if let Some(query) = query {
            search.query = query;
        }
        self.overlay = Overlay::Symbols(Box::new(search));
    }

    pub(super) fn poll_symbols(&mut self) -> bool {
        let Overlay::Symbols(mut search) = std::mem::replace(&mut self.overlay, Overlay::None)
        else {
            unreachable!()
        };
        let mut changed = false;
        if let Some(result) = search.job.as_ref().and_then(|(_, _, job)| job_result(job)) {
            let (mode, query, _) = search.job.take().unwrap();
            if search.submitted.as_ref() == Some(&(mode, query.clone()))
                && search.query == query
                && search.mode == mode
            {
                match result {
                    Ok(results) => search.results = Some(results),
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
                    let mut source = Source::new(search.commit.clone(), path, line, text);
                    source.symbols = Some(search);
                    self.overlay = Overlay::Source(source);
                    return true;
                }
                Err(error) => search.error = Some(format!("ソースを開けません: {error}")),
            }
            changed = true;
        }
        self.overlay = Overlay::Symbols(search);
        changed
    }

    pub(super) fn symbols_key(&mut self, key: KeyEvent, mut search: Box<Symbols>) {
        match key.code {
            KeyCode::Esc => {
                if let Some(source) = search.origin.take() {
                    self.overlay = Overlay::Source(*source);
                }
                return;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                search.mode = match search.mode {
                    SymbolMode::Definitions => SymbolMode::Usages,
                    SymbolMode::Usages => SymbolMode::Definitions,
                };
                search.invalidate();
            }
            KeyCode::Up => search.selected = search.selected.saturating_sub(1),
            KeyCode::Down => search.selected = shifted(search.selected, 1, search.count()),
            KeyCode::PageUp => search.selected = search.selected.saturating_sub(self.page_height),
            KeyCode::PageDown => {
                search.selected =
                    shifted(search.selected, self.page_height as isize, search.count())
            }
            KeyCode::Home => search.selected = 0,
            KeyCode::End => search.selected = search.count().saturating_sub(1),
            KeyCode::Backspace => {
                search.query.pop();
                search.invalidate();
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !c.is_control()
                    && search.query.len() < 512 =>
            {
                search.query.push(c);
                search.invalidate();
            }
            KeyCode::Enter
                if !search.query.is_empty()
                    && search.job.is_none()
                    && search.source_job.is_none() =>
            {
                if search.submitted.as_ref() != Some(&(search.mode, search.query.clone())) {
                    self.submit_symbols(&mut search);
                } else if let Some(hit) = search
                    .results
                    .as_ref()
                    .and_then(|results| results.hits.get(search.selected))
                {
                    let path = hit.path.clone();
                    let worker_path = path.clone();
                    let line = hit.line;
                    let commit = search.commit.clone();
                    let repository = self.repository_copy();
                    let (tx, rx) = mpsc::channel();
                    std::thread::spawn(move || {
                        let _ = tx.send(
                            repository
                                .file_text(&commit, &worker_path)
                                .map_err(|error| format!("{error:#}")),
                        );
                    });
                    search.source_job = Some((path, line, rx));
                }
            }
            _ => {}
        }
        self.overlay = Overlay::Symbols(search);
    }

    pub(super) fn draw_symbols(&self, frame: &mut Frame, search: &Symbols) {
        let area = popup(frame.area(), 96, 94);
        frame.render_widget(Clear, area);
        let commit: String = search.commit.chars().take(10).collect();
        frame.render_widget(
            panel(
                &format!("名前検索 {}:{}", clean(&search.path), clean(&commit)),
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
        let result_area =
            self.result_preview_area(frame, regions[2], area.inner(Margin::new(2, 1)).height);
        let active = search.active_tab();
        let labels = if active == 0 {
            ["定義（選択中）", "使用箇所"]
        } else {
            ["定義", "使用箇所（選択中）"]
        };
        self.draw_click_tabs(frame, regions[0], &labels, active);
        frame.render_widget(
            Paragraph::new(format!("検索語: {}", clean(&search.query))),
            regions[1],
        );
        let message = if let Some(error) = &search.error {
            Some(clean(error))
        } else if search.source_job.is_some() {
            Some("ソースを読み込み中…".into())
        } else if search.job.is_some() {
            Some("名前を検索中…".into())
        } else if search.submitted.is_none() {
            Some("関数名・型名などを入力し、Enterで検索してください".into())
        } else if search.count() == 0 {
            Some("一致する候補はありません".into())
        } else {
            None
        };
        if let Some(message) = message {
            frame.render_widget(
                Paragraph::new(message).wrap(Wrap { trim: false }),
                result_area,
            );
        } else if let Some(results) = &search.results {
            let page = usize::from(result_area.height).max(1);
            let start = search.selected.saturating_sub(page.saturating_sub(1));
            let items: Vec<ListItem> = results
                .hits
                .iter()
                .skip(start)
                .take(page)
                .map(|hit| {
                    ListItem::new(format!(
                        "{}:{}  {}",
                        clean(&hit.path),
                        hit.line,
                        clean(&hit.text)
                    ))
                })
                .collect();
            self.register_list_rows(result_area, start, items.len());
            let mut state =
                ListState::default().with_selected(Some(search.selected.saturating_sub(start)));
            frame.render_stateful_widget(
                List::new(items).highlight_style(selected_style()),
                result_area,
                &mut state,
            );
        }
        let action = if search.count() > 0 {
            "開く"
        } else {
            "検索"
        };
        self.draw_click_buttons(
            frame,
            regions[3],
            &[
                (action, MouseAction::Key(KeyCode::Enter)),
                ("戻る", MouseAction::Key(KeyCode::Esc)),
            ],
        );
        let limit = if search
            .results
            .as_ref()
            .is_some_and(|results| results.truncated)
        {
            " 結果は一部のみ"
        } else {
            ""
        };
        frame.render_widget(Paragraph::new(format!("名前と構文による候補です。同名の別要素を含む場合があります。{limit}\nTab:定義/使用箇所 ↑↓:移動 Enter:検索/開く Esc:戻る Ctrl+C:終了")).wrap(Wrap { trim: false }), regions[4]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::all_files::ContentHit;

    fn press(app: &mut App, key: KeyCode) {
        assert!(!app.key(KeyEvent::new(key, KeyModifiers::NONE)));
    }

    fn click(app: &mut App, column: u16, row: u16, modifiers: KeyModifiers) -> bool {
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers,
        });
        app.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column,
            row,
            modifiers,
        })
    }

    #[test]
    fn results_select_with_arrows_and_single_click_but_open_only_explicitly() {
        use ratatui::backend::TestBackend;
        for mode in [SymbolMode::Definitions, SymbolMode::Usages] {
            let (_fixture, mut app) = super::super::tests::app();
            let mut search = Symbols::new("immutable".into(), "origin.rs".into(), mode, None);
            search.query = "needle".into();
            search.submitted = Some((mode, search.query.clone()));
            search.results = Some(ContentResults {
                hits: vec![
                    ContentHit {
                        path: "first.rs".into(),
                        line: 3,
                        text: "needle".into(),
                    },
                    ContentHit {
                        path: "second.rs".into(),
                        line: 8,
                        text: "needle".into(),
                    },
                ],
                truncated: false,
            });
            app.overlay = Overlay::Symbols(Box::new(search));
            press(&mut app, KeyCode::Down);
            assert!(
                matches!(&app.overlay, Overlay::Symbols(s) if s.selected == 1 && s.source_job.is_none())
            );
            assert!(rendered(&mut app).contains("周辺コードsecond.rs:8"));
            // Use the actual rendered result's click target, as the mouse event path does.
            let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let rect = app
                .mouse_targets
                .borrow()
                .iter()
                .find_map(|(rect, action)| (*action == MouseAction::SelectRow(0)).then_some(*rect))
                .unwrap();
            assert!(click(&mut app, rect.x, rect.y, KeyModifiers::NONE));
            assert!(
                matches!(&app.overlay, Overlay::Symbols(s) if s.selected == 0 && s.source_job.is_none())
            );
            let text = rendered(&mut app);
            assert!(text.contains("周辺コードfirst.rs:3"));
            assert!(!text.contains("周辺コードsecond.rs:8"));
            press(&mut app, KeyCode::Enter);
            assert!(
                matches!(&app.overlay, Overlay::Symbols(s) if s.source_job.as_ref().is_some_and(|(path, line, _)| path == "first.rs" && *line == 3))
            );
        }
    }

    #[test]
    fn mouse_search_keeps_removed_head_and_source_origins() {
        let (_fixture, mut app) = super::super::tests::app();
        app.record_code_viewport(
            Rect::new(10, 10, 30, 2),
            0,
            0,
            &[Line::from("old needle"), Line::from("new value")],
            vec![0, 0],
        );
        assert!(click(&mut app, 14, 10, KeyModifiers::NONE));
        let Overlay::Symbols(search) = &app.overlay else {
            panic!()
        };
        assert_eq!((&*search.commit, &*search.query), ("basecommit", "needle"));
        press(&mut app, KeyCode::Esc);
        assert!(click(&mut app, 14, 11, KeyModifiers::SHIFT));
        let Overlay::Symbols(search) = &app.overlay else {
            panic!()
        };
        assert_eq!(
            (&*search.commit, &*search.query, search.mode),
            ("headcommit", "value", SymbolMode::Usages)
        );
        let mut source = Source::new("immutable".into(), "a.rs".into(), 1, "fn 名前() {}".into());
        source.horizontal = 9;
        app.overlay = Overlay::Source(source);
        app.record_code_viewport(
            Rect::new(10, 10, 30, 1),
            0,
            0,
            &[Line::from("     1  fn 名前() {}")],
            vec![8],
        );
        assert!(!click(&mut app, 10, 10, KeyModifiers::NONE));
        assert!(click(&mut app, 21, 10, KeyModifiers::NONE));
        let Overlay::Symbols(search) = &app.overlay else {
            panic!()
        };
        assert_eq!((&*search.commit, &*search.query), ("immutable", "名前"));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(&app.overlay, Overlay::Source(source) if source.horizontal == 9));
    }

    #[test]
    fn mouse_picker_search_preserves_source_and_keyboard_cancels_press() {
        let (_fixture, mut app) = super::super::tests::app();
        app.open_source_symbol_picker(Source::new(
            "immutable".into(),
            "a.rs".into(),
            1,
            "fn 名前() {}".into(),
        ));
        app.record_code_viewport(
            Rect::new(0, 0, 40, 1),
            0,
            0,
            &[Line::from("▶     1  fn 名前() {}")],
            vec![11],
        );
        assert!(click(&mut app, 12, 0, KeyModifiers::NONE));
        let Overlay::Symbols(search) = &app.overlay else {
            panic!()
        };
        assert_eq!((&*search.commit, &*search.query), ("immutable", "名前"));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::Source(_)));
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        press(&mut app, KeyCode::Down);
        assert!(!app.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 12,
            row: 0,
            modifiers: KeyModifiers::NONE
        }));
    }

    #[test]
    fn mouse_ignores_drag_overlays_and_non_identifiers() {
        let (_fixture, mut app) = super::super::tests::app();
        app.overlay = Overlay::Source(Source::new(
            "immutable".into(),
            "a.rs".into(),
            1,
            "// name\nlet s = \"name\";".into(),
        ));
        app.record_code_viewport(
            Rect::new(0, 0, 30, 2),
            0,
            0,
            &[Line::from("// name"), Line::from("let s = \"name\";")],
            vec![0, 0],
        );
        assert!(!click(&mut app, 4, 0, KeyModifiers::NONE));
        assert!(!click(&mut app, 10, 1, KeyModifiers::NONE));
        assert!(!click(&mut app, 3, 1, KeyModifiers::NONE));
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        app.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!app.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 4,
            row: 1,
            modifiers: KeyModifiers::NONE
        }));
        app.overlay = Overlay::Help(0);
        assert!(!click(&mut app, 4, 1, KeyModifiers::NONE));
    }

    #[test]
    fn picker_moves_between_unicode_names_and_submits_without_retyping() {
        let (_fixture, mut app) = super::super::tests::app();
        let mut source = Source::new(
            "immutable".into(),
            "renamed.ts".into(),
            2,
            "// ignored\nconst 名前 = 呼ぶ(値);\nnext();".into(),
        );
        source.horizontal = 9;
        app.open_source_symbol_picker(source);
        let Overlay::SymbolPicker(picker) = std::mem::replace(&mut app.overlay, Overlay::None)
        else {
            panic!()
        };
        assert_eq!(picker.selected_name(), Some("名前"));
        app.symbol_picker_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), picker);
        let Overlay::SymbolPicker(picker) = std::mem::replace(&mut app.overlay, Overlay::None)
        else {
            panic!()
        };
        assert_eq!(picker.selected_name(), Some("呼ぶ"));
        app.symbol_picker_key(
            KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT),
            picker,
        );
        let Overlay::Symbols(search) = &app.overlay else {
            panic!()
        };
        assert_eq!(
            (&*search.commit, &*search.path, &*search.query),
            ("immutable", "renamed.ts", "呼ぶ")
        );
        assert_eq!(search.submitted, Some((SymbolMode::Usages, "呼ぶ".into())));
        assert!(search.job.is_some());
        press(&mut app, KeyCode::Esc);
        assert!(matches!(&app.overlay, Overlay::Source(source)
            if source.offset == 1 && source.horizontal == 9 && source.commit == "immutable"));
    }

    #[test]
    fn picker_diff_rows_keep_their_own_snapshot_and_original_scroll() {
        let (_fixture, mut app) = super::super::tests::app();
        let file = &mut app.comparison.files[0];
        file.old_path = "old.rs".into();
        file.new_path = "new.rs".into();
        file.lines = vec![
            super::super::super::git::DiffLine {
                kind: LineKind::Header,
                text: "@@ fake @@".into(),
                old_line: None,
                new_line: None,
            },
            super::super::super::git::DiffLine {
                kind: LineKind::Removed,
                text: "fn 古い() {}".into(),
                old_line: Some(7),
                new_line: None,
            },
            super::super::super::git::DiffLine {
                kind: LineKind::Added,
                text: "fn 新しい() {}".into(),
                old_line: None,
                new_line: Some(8),
            },
        ];
        app.diff_offset = 0;
        app.horizontal = 7;
        app.open_diff_symbol_picker();
        let Overlay::SymbolPicker(picker) = std::mem::replace(&mut app.overlay, Overlay::None)
        else {
            panic!()
        };
        assert_eq!(picker.selected_name(), Some("古い"));
        assert_eq!(picker.snapshot(), ("basecommit", "old.rs"));
        app.symbol_picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), picker);
        let Overlay::SymbolPicker(picker) = std::mem::replace(&mut app.overlay, Overlay::None)
        else {
            panic!()
        };
        assert_eq!(picker.selected_name(), Some("新しい"));
        assert_eq!(picker.snapshot(), ("headcommit", "new.rs"));
        app.symbol_picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), picker);
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!((app.diff_offset, app.horizontal), (0, 7));
    }

    #[test]
    fn inline_picker_back_and_escape_restore_the_original_source() {
        let (_fixture, mut app) = super::super::tests::app();
        let mut source = Source::new("commit".into(), "a.rs".into(), 1, "fn example() {}".into());
        source.horizontal = 8;
        app.open_source_symbol_picker(source);
        press(&mut app, KeyCode::Char('q'));
        assert!(matches!(&app.overlay, Overlay::Source(source) if source.horizontal == 8));
        press(&mut app, KeyCode::Char('s'));
        app.focus = Focus::Diff;
        press(&mut app, KeyCode::Esc);
        assert!(matches!(&app.overlay, Overlay::Source(source) if source.horizontal == 8));
        assert!(app.focus == Focus::Diff);
    }

    #[test]
    fn picker_renders_safe_highlight_on_small_and_long_lines() {
        use ratatui::backend::TestBackend;
        let (_fixture, mut app) = super::super::tests::app();
        let source = Source::new(
            "commit".into(),
            "bad\u{1b}.rs".into(),
            1,
            format!("{}fn 名前() {{}}\u{1b}", " ".repeat(120)),
        );
        app.open_source_symbol_picker(source);
        for (width, height) in [(1, 1), (24, 8), (55, 16), (100, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert!(terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| !cell.symbol().contains('\u{1b}')));
            if width >= 55 {
                assert!(terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .any(|cell| cell.symbol() == "名" && cell.bg == Color::Cyan));
            }
        }
    }

    #[test]
    fn diff_search_reads_visible_old_or_new_snapshot_and_restores_offsets() {
        let (_fixture, mut app) = super::super::tests::app();
        app.diff_offset = 0;
        app.horizontal = 7;
        press(&mut app, KeyCode::Char('d'));
        let Overlay::Symbols(search) = &app.overlay else {
            panic!()
        };
        assert_eq!(search.commit, "basecommit");
        assert_eq!(search.path, "a.rs");
        assert_eq!(search.mode, SymbolMode::Definitions);
        press(&mut app, KeyCode::Esc);
        app.diff_offset = 1;
        press(&mut app, KeyCode::Char('R'));
        let Overlay::Symbols(search) = &app.overlay else {
            panic!()
        };
        assert_eq!(search.commit, "headcommit");
        assert_eq!(search.mode, SymbolMode::Usages);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.diff_offset, 1);
        assert_eq!(app.horizontal, 7);
    }

    #[test]
    fn symbol_search_marks_active_tab_for_shortcuts_and_tab_changes() {
        use ratatui::backend::TestBackend;
        let (_fixture, mut app) = super::super::tests::app();
        for (shortcut, modifiers, active, inactive) in [
            (
                'd',
                KeyModifiers::NONE,
                "定義（選択中）",
                "使用箇所（選択中）",
            ),
            (
                'R',
                KeyModifiers::SHIFT,
                "使用箇所（選択中）",
                "定義（選択中）",
            ),
        ] {
            assert!(!app.key(KeyEvent::new(KeyCode::Char(shortcut), modifiers)));
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .flat_map(|cell| cell.symbol().chars())
                .filter(|c| !c.is_whitespace())
                .collect();
            assert!(text.contains(active), "{text}");
            assert!(!text.contains(inactive), "{text}");
            press(&mut app, KeyCode::Tab);
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .flat_map(|cell| cell.symbol().chars())
                .filter(|c| !c.is_whitespace())
                .collect();
            assert!(text.contains(inactive), "{text}");
            assert!(!text.contains(active), "{text}");
            press(&mut app, KeyCode::Esc);
        }
    }

    #[test]
    fn result_source_returns_to_query_and_then_original_source_with_provenance() {
        let (_fixture, mut app) = super::super::tests::app();
        let mut origin = Source::new(
            "anothercommit".into(),
            "unmodified.py".into(),
            3,
            "one\ntwo\nthree\nfour".into(),
        );
        origin.horizontal = 8;
        app.overlay = Overlay::Source(origin);
        press(&mut app, KeyCode::Char('d'));
        let Overlay::Symbols(search) = &mut app.overlay else {
            panic!()
        };
        assert_eq!(search.commit, "anothercommit");
        assert_eq!(search.path, "unmodified.py");
        search.query = "find_me".into();
        search.submitted = Some((SymbolMode::Definitions, "find_me".into()));
        search.results = Some(ContentResults {
            hits: vec![ContentHit {
                path: "found.py".into(),
                line: 2,
                text: "def find_me():".into(),
            }],
            truncated: false,
        });
        let (tx, rx) = mpsc::channel();
        tx.send(Ok("start\ndef find_me():\n    pass".into()))
            .unwrap();
        search.source_job = Some(("found.py".into(), 2, rx));
        assert!(app.poll_symbols());
        let Overlay::Source(source) = &app.overlay else {
            panic!()
        };
        assert_eq!(source.commit, "anothercommit");
        assert_eq!(source.offset, 1);
        assert_eq!(source.path, "found.py");
        // Searches can be nested from the matched source and return one level at a time.
        press(&mut app, KeyCode::Char('R'));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(&app.overlay, Overlay::Source(source) if source.path == "found.py"));
        press(&mut app, KeyCode::Esc);
        let Overlay::Symbols(search) = &app.overlay else {
            panic!()
        };
        assert_eq!(search.query, "find_me");
        assert_eq!(search.count(), 1);
        press(&mut app, KeyCode::Esc);
        let Overlay::Source(source) = &app.overlay else {
            panic!()
        };
        assert_eq!(source.path, "unmodified.py");
        assert_eq!(source.offset, 2);
        assert_eq!(source.horizontal, 8);
    }

    #[test]
    fn changing_input_or_tab_discards_pending_results_and_allows_retry() {
        let (_fixture, mut app) = super::super::tests::app();
        press(&mut app, KeyCode::Char('d'));
        let Overlay::Symbols(search) = &mut app.overlay else {
            panic!()
        };
        search.query = "old".into();
        search.submitted = Some((SymbolMode::Definitions, "old".into()));
        let (tx, rx) = mpsc::channel();
        search.job = Some((SymbolMode::Definitions, "old".into(), rx));
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Tab);
        tx.send(Ok(ContentResults {
            hits: vec![],
            truncated: false,
        }))
        .unwrap();
        assert!(app.poll_symbols());
        let Overlay::Symbols(search) = &mut app.overlay else {
            panic!()
        };
        assert!(search.results.is_none());
        assert!(search.job.is_none());
        assert_eq!(search.mode, SymbolMode::Usages);
        let (tx, rx) = mpsc::channel();
        search.submitted = Some((SymbolMode::Usages, search.query.clone()));
        search.job = Some((SymbolMode::Usages, search.query.clone(), rx));
        tx.send(Err("failed".into())).unwrap();
        assert!(app.poll_symbols());
        let Overlay::Symbols(search) = &app.overlay else {
            panic!()
        };
        assert!(search.submitted.is_none());
        assert!(search.error.is_some());
    }

    #[test]
    fn symbol_results_fit_small_screens_and_escape_closes() {
        use ratatui::backend::TestBackend;
        let (_fixture, mut app) = super::super::tests::app();
        for (width, height) in [(1, 1), (24, 8), (55, 16), (100, 30)] {
            press(&mut app, KeyCode::Char('d'));
            let Overlay::Symbols(search) = &mut app.overlay else {
                panic!()
            };
            search.query = "name\u{1b}".into();
            search.submitted = Some((SymbolMode::Definitions, search.query.clone()));
            search.results = Some(ContentResults {
                hits: vec![ContentHit {
                    path: "path\u{1b}.py".into(),
                    line: 5,
                    text: "def name():\tpass".into(),
                }],
                truncated: true,
            });
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert!(terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| !cell.symbol().contains('\u{1b}')));
            press(&mut app, KeyCode::Esc);
            assert!(matches!(app.overlay, Overlay::None));
        }
    }
    fn rendered(app: &mut App) -> String {
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .flat_map(|cell| cell.symbol().chars())
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    #[test]
    fn manual_diff_search_prefills_selected_word_and_uses_its_snapshot() {
        use super::super::cursor::Position;
        for (row, query, commit, path) in [
            (0, "needle", "basecommit", "old.rs"),
            (1, "value", "headcommit", "new.rs"),
        ] {
            for (key, selection) in [('d', "viw"), ('R', "viw"), ('d', "vaw")] {
                let (_fixture, mut app) = super::super::tests::app();
                app.comparison.files[0].old_path = "old.rs".into();
                app.comparison.files[0].new_path = "new.rs".into();
                // The visible first row deliberately differs from the selected row.
                rendered(&mut app);
                app.cursor.position = Some(Position { row, byte: 4 });
                for c in selection.chars() {
                    press(&mut app, KeyCode::Char(c));
                }
                let before = (app.cursor.anchor, app.cursor.position);
                press(&mut app, KeyCode::Char(key));
                let Overlay::Symbols(search) = &app.overlay else {
                    panic!()
                };
                assert_eq!(
                    (&*search.query, &*search.commit, &*search.path),
                    (query, commit, path)
                );
                assert_eq!(
                    search.mode,
                    if key == 'd' {
                        SymbolMode::Definitions
                    } else {
                        SymbolMode::Usages
                    }
                );
                assert!(search.submitted.is_none() && search.job.is_none());
                assert!(rendered(&mut app).contains(&format!("検索語:{query}")));
                press(&mut app, KeyCode::Tab);
                assert!(
                    matches!(&app.overlay, Overlay::Symbols(search) if search.query == query && search.submitted.is_none())
                );
                press(&mut app, KeyCode::Enter);
                assert!(
                    matches!(&app.overlay, Overlay::Symbols(search) if search.submitted.as_ref().is_some_and(|(_, submitted)| submitted == query))
                );
                press(&mut app, KeyCode::Esc);
                assert_eq!((app.cursor.anchor, app.cursor.position), before);
                assert!(matches!(app.overlay, Overlay::None));
            }
        }
    }

    #[test]
    fn manual_search_rejects_absent_multiword_multiline_or_inactive_selection() {
        use super::super::cursor::Position;
        for (anchor, position, focus) in [
            (None, Some(Position { row: 1, byte: 4 }), Focus::Diff),
            (
                Some(Position { row: 1, byte: 0 }),
                Some(Position { row: 1, byte: 8 }),
                Focus::Diff,
            ),
            (
                Some(Position { row: 0, byte: 4 }),
                Some(Position { row: 1, byte: 8 }),
                Focus::Diff,
            ),
            (
                Some(Position { row: 1, byte: 4 }),
                Some(Position { row: 1, byte: 8 }),
                Focus::Files,
            ),
        ] {
            let (_fixture, mut app) = super::super::tests::app();
            app.focus = focus;
            app.cursor.anchor = anchor;
            app.cursor.position = position;
            press(&mut app, KeyCode::Char('R'));
            assert!(
                matches!(&app.overlay, Overlay::Symbols(search) if search.query.is_empty() && search.commit == "basecommit" && search.submitted.is_none())
            );
        }
    }

    #[test]
    fn manual_source_search_keeps_exact_unicode_selection_and_origin() {
        use super::super::cursor::Position;
        for key in ['d', 'R'] {
            let (_fixture, mut app) = super::super::tests::app();
            let mut source = Source::new(
                "immutable".into(),
                "test.rs".into(),
                1,
                "fn 名前() {}".into(),
            );
            source.cursor.anchor = Some(Position { row: 0, byte: 3 });
            source.cursor.position = Some(Position { row: 0, byte: 6 });
            app.overlay = Overlay::Source(source);
            press(&mut app, KeyCode::Char(key));
            assert!(
                matches!(&app.overlay, Overlay::Symbols(search) if search.query == "名前" && search.commit == "immutable" && search.job.is_none())
            );
            assert!(rendered(&mut app).contains("検索語:名前"));
            press(&mut app, KeyCode::Esc);
            assert!(
                matches!(&app.overlay, Overlay::Source(source) if source.path == "test.rs" && source.cursor.anchor == Some(Position { row: 0, byte: 3 }) && source.cursor.position == Some(Position { row: 0, byte: 6 }))
            );
        }
    }

    #[test]
    fn result_source_title_shows_search_query_before_key_hints() {
        let (_fixture, mut app) = super::super::tests::app();
        let mut source = Source::new(
            "immutable".into(),
            "found.rs".into(),
            1,
            "fn needle() {}".into(),
        );
        let mut search = Symbols::new(
            "immutable".into(),
            "origin.rs".into(),
            SymbolMode::Usages,
            None,
        );
        search.query = "needle".into();
        source.symbols = Some(Box::new(search));
        app.overlay = Overlay::Source(source);
        assert!(rendered(&mut app).contains("found.rs検索語:needle[操作中]"));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(&app.overlay, Overlay::Symbols(search) if search.query == "needle"));
    }

    #[test]
    fn gd_and_gr_search_identifier_at_cursor_with_source_provenance() {
        use ratatui::backend::TestBackend;
        for (suffix, mode) in [('d', SymbolMode::Definitions), ('r', SymbolMode::Usages)] {
            let (_fixture, mut app) = super::super::tests::app();
            app.focus = Focus::Diff;
            app.overlay = Overlay::Source(Source::new(
                "headcommit".into(),
                "test.rs".into(),
                1,
                "fn value() {}".into(),
            ));
            let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
            for _ in 0..3 {
                super::super::tests::press(&mut app, KeyCode::Char('l'));
            }
            super::super::tests::press(&mut app, KeyCode::Char('g'));
            super::super::tests::press(&mut app, KeyCode::Char(suffix));
            assert!(
                matches!(&app.overlay, Overlay::Symbols(s) if s.query == "value" && s.mode == mode && s.commit == "headcommit")
            );
        }
    }
}
