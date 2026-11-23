//! Both result dialogs preview the immutable blob used by their explicit open action.
//! Source owns full-file navigation and return state, so the small excerpt stays separate.
use super::*;

#[derive(Clone, PartialEq, Eq, Debug)]
struct BlobKey {
    commit: String,
    path: String,
}

pub(super) struct Target {
    key: BlobKey,
    line: u32,
}

impl Target {
    pub(super) fn new(commit: &str, path: &str, line: u32) -> Self {
        Self {
            key: BlobKey {
                commit: commit.into(),
                path: path.into(),
            },
            line,
        }
    }
}

type Lines = std::result::Result<Vec<String>, String>;

#[derive(Default)]
pub(super) struct Preview {
    cache: VecDeque<(BlobKey, Lines)>,
    job: Option<(BlobKey, Job<Vec<String>>)>,
}

impl Preview {
    fn lookup(&self, key: &BlobKey) -> Option<&Lines> {
        self.cache
            .iter()
            .find(|(cached, _)| cached == key)
            .map(|(_, text)| text)
    }

    fn poll(&mut self) -> bool {
        let Some(result) = self.job.as_ref().and_then(|(_, job)| job_result(job)) else {
            return false;
        };
        let (key, _) = self.job.take().unwrap();
        self.cache.push_back((key, result));
        while self.cache.len() > 4 {
            self.cache.pop_front();
        }
        true
    }
}

impl App {
    fn result_preview_target(&self) -> Option<Target> {
        match &self.overlay {
            Overlay::Symbols(search) => search.preview_target(),
            Overlay::References { tab, selected } => {
                let file = self.current()?;
                let (commit, path) = self.comparison.files[file].source(&self.comparison);
                let row = self.indexes.get(commit)?.rows(path, *tab).get(*selected)?;
                Some(Target::new(commit, &row.path, row.line))
            }
            _ => None,
        }
    }

    pub(super) fn poll_result_preview(&mut self) -> bool {
        let mut changed = self.result_preview.poll();
        let Some(target) = self.result_preview_target() else {
            return changed;
        };
        if self.result_preview.job.is_none() && self.result_preview.lookup(&target.key).is_none() {
            let key = target.key;
            let worker_key = key.clone();
            let repository = self.repository_copy();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(
                    repository
                        .file_text(&worker_key.commit, &worker_key.path)
                        .map(|text| text.lines().map(clean).collect())
                        .map_err(|error| format!("{error:#}")),
                );
            });
            self.result_preview.job = Some((key, rx));
            changed = true;
        }
        changed
    }

    /// Leave room for the result list on short terminals; omit the panel until a result exists.
    pub(super) fn result_preview_area(
        &self,
        frame: &mut Frame,
        area: Rect,
        modal_height: u16,
    ) -> Rect {
        let Some(target) = self.result_preview_target() else {
            return area;
        };
        let Some((results, preview)) = preview_regions(area, modal_height) else {
            return area;
        };
        frame.render_widget(
            panel(
                &format!("周辺コード {}:{}", clean(&target.key.path), target.line),
                false,
            ),
            preview,
        );
        let inner = preview.inner(Margin::new(1, 1));
        match self.result_preview.lookup(&target.key) {
            Some(Ok(lines)) => {
                if lines.is_empty() || target.line == 0 || target.line as usize > lines.len() {
                    frame.render_widget(Paragraph::new("該当行のコードはありません"), inner);
                } else {
                    frame.render_widget(
                        Paragraph::new(excerpt(lines, target.line, inner.height as usize)),
                        inner,
                    );
                }
            }
            Some(Err(error)) => frame.render_widget(
                Paragraph::new(format!("周辺コードを読み込めません: {}", clean(error)))
                    .wrap(Wrap { trim: false }),
                inner,
            ),
            None => frame.render_widget(Paragraph::new("周辺コードを読み込み中…"), inner),
        }
        results
    }
}

fn preview_regions(area: Rect, modal_height: u16) -> Option<(Rect, Rect)> {
    if area.height < 5 {
        return None;
    }
    // Size against the whole dialog, including its tabs and footer, while keeping result rows.
    let height = modal_height.div_ceil(2).clamp(3, area.height - 2);
    let regions = Layout::vertical([Constraint::Min(2), Constraint::Length(height)]).split(area);
    Some((regions[0], regions[1]))
}

fn excerpt(lines: &[String], line: u32, height: usize) -> Vec<Line<'static>> {
    let selected = line.saturating_sub(1) as usize;
    let start = selected
        .saturating_sub(height / 2)
        .min(lines.len().saturating_sub(height));
    lines
        .iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(row, text)| {
            Line::styled(
                format!(
                    "{} {:>5}  {}",
                    if row == selected { "▶" } else { " " },
                    row + 1,
                    text
                ),
                if row == selected {
                    selected_style()
                } else {
                    Style::default()
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpt_uses_real_numbers_and_current_line() {
        let lines = (1..=12).map(|i| format!("code{i}")).collect::<Vec<_>>();
        let rows = excerpt(&lines, 8, 5);
        assert_eq!(rows[0].to_string(), "      6  code6");
        assert_eq!(rows[2].to_string(), "▶     8  code8");
        assert_eq!(rows[2].style, selected_style());
        assert_eq!(excerpt(&lines, 1, 5)[0].to_string(), "▶     1  code1");
        assert_eq!(excerpt(&lines, 12, 5)[4].to_string(), "▶    12  code12");
    }

    #[test]
    fn preview_uses_half_of_dialog_and_preserves_result_rows() {
        // Reference tabs/footer reserve 3 rows; name search reserves 8 rows.
        for reserved in [3, 8] {
            for modal_height in 0_u16..80 {
                let area = Rect::new(2, 4, 100, modal_height.saturating_sub(reserved));
                let Some((results, preview)) = preview_regions(area, modal_height) else {
                    assert!(area.height < 5);
                    continue;
                };
                assert_eq!(results.height + preview.height, area.height);
                assert!(results.height >= 2);
                assert!(preview.height >= 3);
                assert_eq!(preview.y, results.bottom());
                assert_eq!(preview.bottom(), area.bottom());
                assert_eq!(
                    preview.height,
                    modal_height.div_ceil(2).min(area.height - 2)
                );
            }
        }
        let (_, preview) = preview_regions(Rect::new(0, 0, 100, 30), 40).unwrap();
        assert_eq!(preview.height, 20);
    }

    #[test]
    fn excerpt_fills_tall_preview_and_keeps_selected_line_visible() {
        let lines = (1..=40).map(|i| format!("code{i}")).collect::<Vec<_>>();
        for selected in [1, 20, 40] {
            let rows = excerpt(&lines, selected, 18);
            assert_eq!(rows.len(), 18);
            assert!(rows.iter().any(|row| {
                row.to_string() == format!("▶ {:>5}  code{}", selected, selected)
                    && row.style == selected_style()
            }));
        }
    }

    #[test]
    fn reference_selection_uses_current_key_and_keeps_open_explicit() {
        use ratatui::backend::TestBackend;
        let (_fixture, mut app) = super::super::tests::app();
        let root = app.repository.root.clone();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        std::fs::write(
            root.join("main.ts"),
            "import './first';\nimport './second';\nexport const main = 1;\n",
        )
        .unwrap();
        std::fs::write(root.join("first.ts"), "export const first = 1;\n").unwrap();
        std::fs::write(root.join("second.ts"), "export const second = 2;\n").unwrap();
        std::fs::write(root.join("caller.ts"), "import './main';\n").unwrap();
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ]);
        let commit = git(&["rev-parse", "HEAD"]);
        app.comparison.merge_base = commit.clone();
        app.comparison.head_commit = commit.clone();
        app.comparison.files[0].old_path = "main.ts".into();
        app.comparison.files[0].new_path = "main.ts".into();
        app.indexes.insert(
            commit.clone(),
            ReferenceIndex::load(&app.repository, &commit).unwrap(),
        );
        app.overlay = Overlay::References {
            tab: 1,
            selected: 0,
        };
        let first = app.result_preview_target().unwrap();
        app.result_preview
            .cache
            .push_back((first.key.clone(), Ok(vec!["FIRST_ONLY".into()])));
        let render = |app: &mut App| {
            let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .flat_map(|cell| cell.symbol().chars())
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
        };
        assert!(render(&mut app).contains("FIRST_ONLY"));
        super::super::tests::press(&mut app, KeyCode::Down);
        let second = app.result_preview_target().unwrap();
        assert_ne!(first.key, second.key);
        assert!(matches!(
            app.overlay,
            Overlay::References { selected: 1, .. }
        ));
        let text = render(&mut app);
        assert!(text.contains("周辺コードを読み込み中…"), "{text}");
        assert!(!text.contains("FIRST_ONLY"));
        // A completion for the old selection is cached but cannot paint the new selection.
        let (tx, rx) = mpsc::channel();
        app.result_preview.job = Some((first.key.clone(), rx));
        tx.send(Ok(vec!["STALE_ONLY".into()])).unwrap();
        assert!(app.result_preview.poll());
        assert!(!render(&mut app).contains("STALE_ONLY"));
        app.activate_mouse_control(MouseAction::SelectRow(0));
        assert!(matches!(
            app.overlay,
            Overlay::References { selected: 0, .. }
        ));
        // Same blob, different selected line: the highlight is computed at draw time.
        assert_eq!(app.result_preview_target().unwrap().key, first.key);
        super::super::tests::press(&mut app, KeyCode::Tab);
        assert!(matches!(
            app.overlay,
            Overlay::References {
                tab: 0,
                selected: 0
            }
        ));
        assert!(!render(&mut app).contains("FIRST_ONLY"));
        // The actual rendered open action reuses Enter and the immutable committed source.
        let action = app
            .mouse_targets
            .borrow()
            .iter()
            .find_map(|(_, action)| {
                (*action == MouseAction::Key(KeyCode::Enter)).then_some(*action)
            })
            .unwrap();
        app.activate_mouse_control(action);
        assert!(
            matches!(&app.overlay, Overlay::Source(source) if source.commit == commit && source.path == "caller.ts")
        );
        super::super::tests::press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn cache_is_bounded_and_never_confuses_commit_or_path() {
        let mut preview = Preview::default();
        for i in 0..6 {
            let (tx, rx) = mpsc::channel();
            preview.job = Some((Target::new(&i.to_string(), "a.rs", 1).key, rx));
            tx.send(Ok(vec![format!("code{i}")])).unwrap();
            assert!(preview.poll());
        }
        assert_eq!(preview.cache.len(), 4);
        assert!(preview.lookup(&Target::new("0", "a.rs", 1).key).is_none());
        assert!(preview
            .lookup(&Target::new("5", "other.rs", 1).key)
            .is_none());
        assert_eq!(
            preview
                .lookup(&Target::new("5", "a.rs", 3).key)
                .unwrap()
                .as_ref()
                .unwrap(),
            &["code5"]
        );
    }
}
