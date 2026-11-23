//! my-job-tool の DirNode/tree_rows と同じフォルダ先・名前順の構成。
//! egui の行描画と集計は共有できないため、端末に必要な行情報だけを組み立てる。
use std::collections::{BTreeMap, HashSet};

use super::{App, Focus};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Row {
    Dir {
        depth: usize,
        key: String,
        label: String,
    },
    File {
        depth: usize,
        index: usize,
    },
}

#[derive(Default)]
struct Dir {
    dirs: BTreeMap<String, Dir>,
    files: Vec<(String, usize)>,
}

impl Dir {
    fn rows(&self, prefix: &str, depth: usize, collapsed: &HashSet<String>, out: &mut Vec<Row>) {
        for (name, child) in &self.dirs {
            let key = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            out.push(Row::Dir {
                depth,
                key: key.clone(),
                label: name.clone(),
            });
            if !collapsed.contains(&key) {
                child.rows(&key, depth + 1, collapsed, out);
            }
        }
        out.extend(self.files.iter().map(|(_, index)| Row::File {
            depth,
            index: *index,
        }));
    }
}

/// Shared path builder; all-files keeps this tree across key presses.
#[derive(Default)]
pub(super) struct FileTree {
    root: Dir,
}

impl FileTree {
    pub(super) fn new<'a>(paths: impl IntoIterator<Item = (usize, &'a str)>) -> Self {
        let mut root = Dir::default();
        for (index, path) in paths {
            let mut parts = path.split('/').collect::<Vec<_>>();
            let name = parts.pop().unwrap_or_default();
            let mut dir = &mut root;
            for part in parts {
                dir = dir.dirs.entry(part.into()).or_default();
            }
            dir.files.push((name.into(), index));
        }
        fn sort(dir: &mut Dir) {
            dir.files.sort_by(|a, b| a.0.cmp(&b.0));
            for child in dir.dirs.values_mut() {
                sort(child);
            }
        }
        sort(&mut root);
        Self { root }
    }

    pub(super) fn rows(&self, collapsed: &HashSet<String>) -> Vec<Row> {
        let mut rows = Vec::new();
        self.root.rows("", 0, collapsed, &mut rows);
        rows
    }

    pub(super) fn folders(&self) -> HashSet<String> {
        self.rows(&HashSet::new())
            .into_iter()
            .filter_map(|row| match row {
                Row::Dir { key, .. } => Some(key),
                Row::File { .. } => None,
            })
            .collect()
    }
}

impl App {
    pub(super) fn tree_rows(&self, collapsed: &HashSet<String>) -> Vec<Row> {
        FileTree::new(
            self.visible()
                .into_iter()
                .map(|index| (index, self.comparison.files[index].path())),
        )
        .rows(collapsed)
    }

    pub(super) fn tree_selected(&self, rows: &[Row]) -> Option<usize> {
        if let Some(key) = &self.folder {
            if let Some(pos) = rows
                .iter()
                .position(|row| matches!(row, Row::Dir { key: row_key, .. } if row_key == key))
            {
                return Some(pos);
            }
        }
        let index = self.current()?;
        rows.iter().position(
            |row| matches!(row, Row::File { index: row_index, .. } if *row_index == index),
        )
    }

    pub(super) fn reveal_file(&mut self, index: usize) {
        let path = self.comparison.files[index].path();
        self.collapsed.retain(|key| {
            !path
                .strip_prefix(key.as_str())
                .is_some_and(|tail| tail.starts_with('/'))
        });
        self.folder = None;
    }

    pub(super) fn select_first_tree_file(&mut self) {
        self.selected = 0;
        self.folder = None;
        if let Some(row @ Row::File { index, .. }) = self
            .tree_rows(&HashSet::new())
            .into_iter()
            .find(|row| matches!(row, Row::File { .. }))
        {
            self.select_tree_row(&row);
            self.reveal_file(index);
        }
    }

    fn select_tree_row(&mut self, row: &Row) {
        match row {
            Row::Dir { key, .. } => self.folder = Some(key.clone()),
            Row::File { index, .. } => {
                if let Some(selected) = self.visible().iter().position(|i| i == index) {
                    if self.current() != Some(*index) {
                        self.reset_position();
                    }
                    self.selected = selected;
                    self.folder = None;
                }
            }
        }
    }

    pub(super) fn select_changed_row(&mut self, index: usize, toggle_folder: bool) -> bool {
        let rows = self.tree_rows(&self.collapsed);
        let Some(row) = rows.get(index) else {
            return false;
        };
        self.focus = Focus::Files;
        self.select_tree_row(row);
        if toggle_folder && matches!(row, Row::Dir { .. }) {
            self.tree_control(None);
        }
        matches!(row, Row::File { .. })
    }

    pub(super) fn move_tree(&mut self, delta: isize) {
        let rows = self.tree_rows(&self.collapsed);
        let pos = super::shifted(self.tree_selected(&rows).unwrap_or(0), delta, rows.len());
        if let Some(row) = rows.get(pos) {
            self.select_tree_row(row);
        }
    }

    pub(super) fn tree_control(&mut self, expand: Option<bool>) {
        let rows = self.tree_rows(&self.collapsed);
        let Some(pos) = self.tree_selected(&rows) else {
            return;
        };
        match &rows[pos] {
            Row::Dir { key, .. } => {
                let open = expand.unwrap_or_else(|| self.collapsed.contains(key));
                if open { self.collapsed.remove(key); } else { self.collapsed.insert(key.clone()); }
                self.folder = Some(key.clone());
            }
            Row::File { depth, .. } if expand == Some(false) && *depth > 0 => {
                if let Some(row) = rows[..pos].iter().rev().find(|row| matches!(row, Row::Dir { depth: dir_depth, .. } if dir_depth + 1 == *depth)) {
                    self.select_tree_row(row);
                }
            }
            _ => {}
        }
    }

    pub(super) fn reconcile_tree(&mut self) {
        let rows = self.tree_rows(&self.collapsed);
        if self.folder.as_ref().is_some_and(|folder| {
            rows.iter()
                .any(|row| matches!(row, Row::Dir { key, .. } if key == folder))
        }) {
            return;
        }
        self.folder = None;
        if let Some(index) = self.current() {
            self.reveal_file(index);
        }
    }

    pub(super) fn folder_focused(&self) -> bool {
        self.focus == Focus::Files && self.folder.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::tui::tests::{app, press};
    use crate::review::tui::Overlay;
    use crossterm::event::KeyCode;

    fn nested_app() -> (crate::review::tui::tests::Fixture, App) {
        let (fixture, mut app) = app();
        for (file, path) in
            app.comparison
                .files
                .iter_mut()
                .zip(["src/nested/a.rs", "src/b.rs", "root.rs"])
        {
            file.old_path = path.into();
            file.new_path = path.into();
        }
        (fixture, app)
    }

    #[test]
    fn rows_are_directory_first_and_collapse_by_full_path() {
        let (_fixture, mut app) = nested_app();
        assert_eq!(
            app.tree_rows(&app.collapsed),
            vec![
                Row::Dir {
                    depth: 0,
                    key: "src".into(),
                    label: "src".into()
                },
                Row::Dir {
                    depth: 1,
                    key: "src/nested".into(),
                    label: "nested".into()
                },
                Row::File { depth: 2, index: 0 },
                Row::File { depth: 1, index: 1 },
                Row::File { depth: 0, index: 2 },
            ]
        );
        app.collapsed.insert("src/nested".into());
        assert_eq!(app.tree_rows(&app.collapsed).len(), 4);
        app.collapsed.insert("src".into());
        assert_eq!(app.tree_rows(&app.collapsed).len(), 2);
    }

    #[test]
    fn folder_selection_keeps_diff_and_does_not_mark_previous_file() {
        let (_fixture, mut app) = nested_app();
        app.focus = Focus::Files;
        app.diff_offset = 2;
        press(&mut app, KeyCode::Up);
        assert_eq!(app.folder.as_deref(), Some("src/nested"));
        assert_eq!(app.current(), Some(0));
        assert_eq!(app.diff_offset, 2);
        press(&mut app, KeyCode::Char(' '));
        assert!(!app
            .viewed
            .is_viewed(&app.comparison, &app.comparison.files[0]));
        press(&mut app, KeyCode::Enter);
        assert!(app.collapsed.contains("src/nested"));
        press(&mut app, KeyCode::Down);
        assert_eq!(app.current(), Some(1));
        assert!(app.folder.is_none());
        assert_eq!(app.diff_offset, 0);
        press(&mut app, KeyCode::Left);
        assert_eq!(app.folder.as_deref(), Some("src"));
        press(&mut app, KeyCode::Left);
        assert!(app.collapsed.contains("src"));
        press(&mut app, KeyCode::Right);
        assert!(!app.collapsed.contains("src"));
    }

    #[test]
    fn jk_navigation_follows_focus_and_enter_selects_diff_without_popup() {
        let (_fixture, mut app) = nested_app();
        app.focus = Focus::Files;
        app.folder = Some("src".into());
        app.collapsed.insert("src".into());
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.current(), Some(1));
        assert!(!app.collapsed.contains("src"));
        assert!(app.folder.is_none());
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.current(), Some(0));
        press(&mut app, KeyCode::Enter);
        assert!(app.focus == Focus::Diff);
        assert!(matches!(app.overlay, Overlay::None));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.overlay, Overlay::None));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.current(), Some(0));
        assert_eq!(app.diff_offset, 0);
        assert_eq!(app.cursor.position.unwrap().row, 1);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.diff_offset, 0);
        press(&mut app, KeyCode::Char('s'));
        assert!(matches!(app.overlay, Overlay::SymbolPicker(_)));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.focus == Focus::Diff);
        app.focus = Focus::Files;
        app.collapsed.extend(["src".into(), "src/nested".into()]);
        app.folder = Some("src".into());
        assert!(app.open_all_files_diff("src/nested/a.rs", None));
        assert!(app.focus == Focus::Files);
        assert!(app.collapsed.is_empty());
        assert!(app.folder.is_none());
        assert_eq!(app.current(), Some(0));
    }

    #[test]
    fn filter_and_mark_remove_empty_folders_and_keep_selection_visible() {
        let (_fixture, mut app) = nested_app();
        app.focus = Focus::Files;
        press(&mut app, KeyCode::Char('u'));
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.current(), Some(1));
        assert!(!app
            .tree_rows(&app.collapsed)
            .iter()
            .any(|row| matches!(row, Row::Dir { key, .. } if key == "src/nested")));
        press(&mut app, KeyCode::Left);
        assert_eq!(app.folder.as_deref(), Some("src"));
        press(&mut app, KeyCode::Char('u'));
        assert_eq!(app.current(), Some(1));
        assert_eq!(app.folder.as_deref(), Some("src"));
        app.folder = None;
        press(&mut app, KeyCode::Char('u'));
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.current(), Some(2));
        assert!(app.folder.is_none());
        assert!(app.tree_selected(&app.tree_rows(&app.collapsed)).is_some());
    }

    #[test]
    fn nested_names_are_sanitized_and_render_at_minimum_widths() {
        use ratatui::{backend::TestBackend, Terminal};
        let (_fixture, mut app) = nested_app();
        app.comparison.files[0].new_path = "src/weird\u{1b}/line\nfile.rs".into();
        for width in [1, 5, 12, 30, 80] {
            let mut terminal = Terminal::new(TestBackend::new(width, 10)).unwrap();
            terminal
                .draw(|frame| app.draw_files(frame, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            for cell in &buffer.content {
                assert!(!cell.symbol().contains('\u{1b}'));
                assert!(!cell.symbol().contains('\n'));
            }
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal
            .draw(|frame| app.draw_files(frame, frame.area()))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("b.rs"));
        assert!(!rendered.contains("src/b.rs"));
    }
}
