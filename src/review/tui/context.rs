//! Desktop context expansion is egui-specific; build terminal-only display rows from its hunk ranges.
use super::super::git::{DiffLine, FileDiff};
use super::*;

const EXPAND_STEP: usize = 20;

#[derive(Clone, Copy)]
pub(super) struct Gap {
    old: u32,
    new: u32,
    len: usize,
    pub(super) top: bool,
    bottom: bool,
}

pub(super) struct Expanded {
    pub(super) key: ColorKey,
    source: Vec<String>,
    original: FileDiff,
    file: FileDiff,
    pub(super) gaps: HashMap<usize, Gap>,
    revealed: HashMap<(u32, u32), (usize, usize)>,
    full: bool,
}

pub(super) struct OpenedContext {
    revealed: HashMap<(u32, u32), (usize, usize)>,
    full: bool,
}

fn hunk(text: &str) -> Option<(u32, u32, u32, u32)> {
    let mut parts = text.strip_prefix("@@ ")?.split_whitespace();
    fn side(s: &str) -> Option<(u32, u32)> {
        let mut p = s.get(1..)?.split(',');
        Some((
            p.next()?.parse().ok()?,
            p.next().map_or(Some(1), |n| n.parse().ok())?,
        ))
    }
    let (old, oc) = side(parts.next()?)?;
    let (new, nc) = side(parts.next()?)?;
    Some((old, oc, new, nc))
}

impl Expanded {
    pub(super) fn is_full(&self) -> bool {
        self.full
    }

    fn new(key: ColorKey, file: FileDiff, text: String) -> Self {
        let mut result = Self {
            key,
            source: text.lines().map(str::to_owned).collect(),
            original: file.clone(),
            file,
            gaps: HashMap::new(),
            revealed: HashMap::new(),
            full: false,
        };
        result.rebuild();
        result
    }
    fn gap(&mut self, old: u32, new: u32, len: usize, top: bool, bottom: bool) {
        if len == 0 {
            return;
        }
        let (front, back) = self.revealed.get(&(old, new)).copied().unwrap_or_default();
        let front = if self.full { len } else { front.min(len) };
        let back = back.min(len - front);
        for offset in 0..front {
            self.context(old, new, offset);
        }
        let hidden = len - front - back;
        if hidden > 0 {
            self.gaps.insert(
                self.file.lines.len(),
                Gap {
                    old,
                    new,
                    len,
                    top,
                    bottom,
                },
            );
            self.file.lines.push(DiffLine {
                kind: LineKind::Header,
                text: if hidden <= EXPAND_STEP {
                    format!("[全{hidden}行を表示]")
                } else if top {
                    format!("↑ 20行  [全{hidden}行を表示]")
                } else if bottom {
                    format!("↓ 20行  [全{hidden}行を表示]")
                } else {
                    format!("↓ 20行  ↑ 20行  [全{hidden}行を表示]")
                },
                old_line: None,
                new_line: None,
            });
        }
        for offset in len - back..len {
            self.context(old, new, offset);
        }
    }
    fn context(&mut self, old: u32, new: u32, offset: usize) {
        let n = new + offset as u32;
        if let Some(text) = self.source.get(n.saturating_sub(1) as usize) {
            self.file.lines.push(DiffLine {
                kind: LineKind::Context,
                text: text.clone(),
                old_line: Some(old + offset as u32),
                new_line: Some(n),
            });
        }
    }
    fn rebuild(&mut self) {
        self.file.lines.clear();
        self.gaps.clear();
        let mut end = (1u32, 1u32);
        let mut seen = false;
        for line in self.original.lines.clone() {
            if let Some((old, oc, new, nc)) = (line.kind == LineKind::Header)
                .then(|| hunk(&line.text))
                .flatten()
            {
                // A zero-count side names the line before the insertion/deletion.
                let start = (old + u32::from(oc == 0), new + u32::from(nc == 0));
                let old_gap = start.0.saturating_sub(end.0);
                let new_gap = start.1.saturating_sub(end.1);
                if old_gap == new_gap {
                    self.gap(end.0, end.1, new_gap as usize, !seen, false);
                }
                end = (start.0 + oc, start.1 + nc);
                seen = true;
            }
            self.file.lines.push(line);
        }
        if seen {
            let remaining = self
                .source
                .len()
                .saturating_add(1)
                .saturating_sub(end.1 as usize);
            self.gap(end.0, end.1, remaining, false, true);
        }
        if !seen {
            self.gap(1, 1, self.source.len(), false, true);
        }
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        for line in &self.file.lines {
            line.text.hash(&mut hash);
            line.old_line.hash(&mut hash);
            line.new_line.hash(&mut hash);
        }
        self.file.digest = format!("{}:context:{}", self.key.digest, hash.finish());
    }
}

impl App {
    pub(super) fn display_file(&self, index: usize) -> &FileDiff {
        self.expanded
            .as_ref()
            .filter(|e| e.key == self.original_color_key(index))
            .map_or(&self.comparison.files[index], |e| &e.file)
    }
    pub(super) fn original_color_key(&self, index: usize) -> ColorKey {
        let file = &self.comparison.files[index];
        ColorKey {
            base: self.comparison.merge_base.clone(),
            head: self.comparison.head_commit.clone(),
            path: file.path().into(),
            digest: file.digest.clone(),
        }
    }
    fn start_context(&mut self) {
        if self.context_job.is_some() {
            return;
        }
        let Some(index) = self.current() else { return };
        let key = self.original_color_key(index);
        if self.context_attempt.as_ref() == Some(&key)
            || self
                .expanded
                .as_ref()
                .is_some_and(|expanded| expanded.key == key)
        {
            return;
        }
        self.context_attempt = Some(key.clone());
        let file = self.comparison.files[index].clone();
        if file.status == 'D' {
            return;
        }
        let repo = self.repository_copy();
        let commit = key.head.clone();
        let path = file.new_path.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(
                repo.file_text(&commit, &path)
                    .map(|text| Expanded::new(key, file, text))
                    .map_err(|e| format!("{e:#}")),
            );
        });
        self.context_job = Some(rx);
    }
    pub(super) fn poll_context(&mut self) -> bool {
        let mut changed = false;
        if let Some(result) = self.context_job.as_ref().and_then(job_result) {
            self.context_job = None;
            if self.current().map(|i| self.original_color_key(i)).as_ref()
                != self.context_attempt.as_ref()
            {
                self.context_attempt = None;
            }
            if let Ok(mut expanded) = result {
                if let Some(index) = self
                    .current()
                    .filter(|&i| expanded.key == self.original_color_key(i))
                {
                    if let Some(previous) = self.expanded.take() {
                        if previous.full || !previous.revealed.is_empty() {
                            self.context_history.insert(
                                previous.key,
                                OpenedContext {
                                    revealed: previous.revealed,
                                    full: previous.full,
                                },
                            );
                        }
                    }
                    if let Some(opened) = self.context_history.remove(&expanded.key) {
                        expanded.revealed = opened.revealed;
                        expanded.full = opened.full;
                        expanded.rebuild();
                    }
                    let original = &self.comparison.files[index].lines;
                    let row_for = |row: usize| {
                        original.get(row).and_then(|anchor| {
                            expanded.file.lines.iter().position(|line| {
                                line.kind == anchor.kind
                                    && line.old_line == anchor.old_line
                                    && line.new_line == anchor.new_line
                                    && line.text == anchor.text
                            })
                        })
                    };
                    // Offset zero stays at the new beginning so the first gap remains discoverable.
                    if self.diff_offset > 0 {
                        if let Some(row) = row_for(self.diff_offset) {
                            self.diff_offset = row;
                        }
                    }
                    self.cursor.remap_rows(row_for);
                    self.expanded = Some(expanded);
                    self.mouse_down = None;
                    changed = true;
                }
            }
        }
        self.start_context();
        changed
    }
    /// Return (all, backwards) only over a rendered action label, excluding separator spaces.
    pub(super) fn context_gap_action(&self, row: usize, byte: usize) -> Option<(bool, bool)> {
        let index = self.current()?;
        let expanded = self
            .expanded
            .as_ref()
            .filter(|e| e.key == self.original_color_key(index))?;
        let gap = expanded.gaps.get(&row)?;
        let text = &expanded.file.lines.get(row)?.text;
        let counts = expanded
            .revealed
            .get(&(gap.old, gap.new))
            .copied()
            .unwrap_or_default();
        let hidden = gap.len.saturating_sub(counts.0 + counts.1);
        let mut offset = 0;
        if hidden > EXPAND_STEP {
            let label = if gap.top { "↑ 20行" } else { "↓ 20行" };
            if byte < label.len() {
                return Some((false, gap.top));
            }
            offset = label.len() + 2;
            if !gap.top && !gap.bottom {
                if (offset..offset + "↑ 20行".len()).contains(&byte) {
                    return Some((false, true));
                }
                offset += "↑ 20行".len() + 2;
            }
        }
        if (offset..text.len()).contains(&byte) {
            Some((true, gap.top))
        } else {
            None
        }
    }
    pub(super) fn expand_gap(&mut self, row: usize, all: bool, backwards: bool) -> bool {
        let Some(index) = self.current() else {
            return false;
        };
        if !self
            .expanded
            .as_ref()
            .is_some_and(|e| e.key == self.original_color_key(index))
        {
            return false;
        }
        let expanded = self.expanded.as_mut().unwrap();
        let Some(gap) = expanded.gaps.get(&row).copied() else {
            return false;
        };
        let counts = expanded.revealed.entry((gap.old, gap.new)).or_default();
        let remaining = gap.len.saturating_sub(counts.0 + counts.1);
        let amount = if all {
            remaining
        } else {
            remaining.min(EXPAND_STEP)
        };
        if backwards {
            counts.1 += amount
        } else {
            counts.0 += amount
        }
        expanded.rebuild();
        self.cursor = Default::default();
        // Keep the clicked gap's near edge at its screen position.
        if row < self.diff_offset {
            self.diff_offset += amount;
        }
        true
    }
    pub(super) fn expand_visible_gap(&mut self, all: bool) {
        let Some(index) = self.current() else { return };
        if let Some((row, top)) = self
            .expanded
            .as_ref()
            .filter(|e| e.key == self.original_color_key(index))
            .and_then(|e| {
                e.gaps
                    .iter()
                    .filter(|(r, _)| {
                        **r >= self.diff_offset && **r < self.diff_offset + self.page_height
                    })
                    .min_by_key(|(r, _)| **r)
                    .map(|(r, g)| (*r, g.top))
            })
        {
            self.expand_gap(row, all, top);
        }
    }
    pub(super) fn toggle_full_file(&mut self) {
        let Some(index) = self.current() else { return };
        if !self
            .expanded
            .as_ref()
            .is_some_and(|e| e.key == self.original_color_key(index))
        {
            return;
        }
        let anchor = self
            .display_file(index)
            .lines
            .get(self.diff_offset)
            .map(|l| (l.old_line, l.new_line));
        let e = self.expanded.as_mut().unwrap();
        e.full = !e.full;
        e.rebuild();
        self.cursor = Default::default();
        self.diff_offset = anchor
            .and_then(|(old, new)| {
                e.file.lines.iter().position(|l| {
                    if let Some(new) = new {
                        l.new_line == Some(new)
                    } else {
                        old.is_some() && l.old_line == old
                    }
                })
            })
            .unwrap_or(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn expanded() -> (super::super::tests::Fixture, App) {
        let (fixture, mut app) = super::super::tests::app();
        let f = &mut app.comparison.files[0];
        f.lines = vec![
            DiffLine {
                kind: LineKind::Header,
                text: "@@ -51,2 +51,3 @@".into(),
                old_line: None,
                new_line: None,
            },
            DiffLine {
                kind: LineKind::Context,
                text: "line51".into(),
                old_line: Some(51),
                new_line: Some(51),
            },
            DiffLine {
                kind: LineKind::Removed,
                text: "old".into(),
                old_line: Some(52),
                new_line: None,
            },
            DiffLine {
                kind: LineKind::Added,
                text: "line52".into(),
                old_line: None,
                new_line: Some(52),
            },
            DiffLine {
                kind: LineKind::Added,
                text: "line53".into(),
                old_line: None,
                new_line: Some(53),
            },
            DiffLine {
                kind: LineKind::Header,
                text: "@@ -103 +104 @@".into(),
                old_line: None,
                new_line: None,
            },
            DiffLine {
                kind: LineKind::Removed,
                text: "old104".into(),
                old_line: Some(103),
                new_line: None,
            },
            DiffLine {
                kind: LineKind::Added,
                text: "line104".into(),
                old_line: None,
                new_line: Some(104),
            },
        ];
        let text = (1..=155).map(|n| format!("line{n}\n")).collect();
        app.expanded = Some(Expanded::new(
            app.original_color_key(0),
            app.comparison.files[0].clone(),
            text,
        ));
        (fixture, app)
    }
    #[test]
    fn repeated_top_twenty_and_middle_both_edges_preserve_number_shifts() {
        let (_fixture, mut app) = expanded();
        assert!(app.expand_gap(0, false, true));
        assert_eq!(app.display_file(0).lines[1].new_line, Some(31));
        assert!(app.expand_gap(0, false, true));
        assert_eq!(app.display_file(0).lines[1].new_line, Some(11));
        assert!(app.expand_gap(0, false, true));
        assert_eq!(app.display_file(0).lines[0].new_line, Some(1));
        let row = *app.expanded.as_ref().unwrap().gaps.keys().min().unwrap();
        assert!(app.expand_gap(row, false, false));
        let line = &app.display_file(0).lines[row];
        assert_eq!((line.old_line, line.new_line), (Some(53), Some(54)));
        let row = *app.expanded.as_ref().unwrap().gaps.keys().min().unwrap();
        assert!(app.expand_gap(row, false, true));
        let e = app.expanded.as_ref().unwrap();
        let row = *e.gaps.keys().min().unwrap();
        assert_eq!(
            (
                e.file.lines[row + 1].old_line,
                e.file.lines[row + 1].new_line
            ),
            (Some(83), Some(84))
        );
    }
    #[test]
    fn full_toggle_preserves_canonical_and_restores_partial_ranges() {
        let (_fixture, mut app) = expanded();
        let canonical = app.comparison.files[0].lines.len();
        app.expand_gap(0, false, true);
        let partial = app.display_file(0).lines.len();
        super::super::tests::press(&mut app, KeyCode::Char('a'));
        assert!(app.expanded.as_ref().unwrap().gaps.is_empty());
        assert_eq!(
            app.display_file(0)
                .lines
                .iter()
                .filter(|l| l.kind == LineKind::Added)
                .count(),
            3
        );
        assert_eq!(app.comparison.files[0].lines.len(), canonical);
        assert_eq!(app.comparison.files[0].digest, "digest");
        super::super::tests::press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.display_file(0).lines.len(), partial);
    }

    #[test]
    fn switching_files_restores_only_their_own_opened_context() {
        let (_fixture, mut app) = expanded();
        assert!(app.expand_gap(0, false, true));
        let known = app.display_file(0).lines.len();
        let load = |app: &mut App, index| {
            app.selected = index;
            let key = app.original_color_key(index);
            let expanded = Expanded::new(
                key.clone(),
                app.comparison.files[index].clone(),
                (1..=155).map(|n| format!("line{n}\n")).collect(),
            );
            let (tx, rx) = mpsc::channel();
            tx.send(Ok(expanded)).unwrap();
            app.context_attempt = Some(key);
            app.context_job = Some(rx);
            app.poll_context();
        };
        load(&mut app, 1);
        assert_eq!(app.expanded.as_ref().unwrap().key.path, "b.rs");
        load(&mut app, 0);
        assert_eq!(app.display_file(0).lines.len(), known);
        assert_eq!(app.display_file(0).lines[1].new_line, Some(31));
    }
    #[test]
    fn zero_count_hunks_start_after_the_named_line() {
        let (_fixture, mut app) = expanded();
        let mut f = app.comparison.files[0].clone();
        f.lines = vec![
            DiffLine {
                kind: LineKind::Header,
                text: "@@ -0,0 +1 @@".into(),
                old_line: None,
                new_line: None,
            },
            DiffLine {
                kind: LineKind::Added,
                text: "inserted".into(),
                old_line: None,
                new_line: Some(1),
            },
        ];
        let mut e = Expanded::new(app.original_color_key(0), f, "inserted\noriginal\n".into());
        e.full = true;
        e.rebuild();
        let last = e.file.lines.last().unwrap();
        assert_eq!((last.old_line, last.new_line), (Some(1), Some(2)));
        app.query = "20行".into();
        app.search(false, true);
        assert_eq!(app.diff_offset, 0);
    }
    #[test]
    fn keyboard_and_rendered_arrow_click_reveal_same_top_rows() {
        use ratatui::{backend::TestBackend, Terminal};
        let (_fixture, mut app) = expanded();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let b = terminal.backend().buffer();
        let mut arrow = None;
        for y in 0..30 {
            for x in 0..120 {
                if b[(x, y)].symbol() == "↑" {
                    arrow = Some((x, y));
                    break;
                }
            }
            if arrow.is_some() {
                break;
            }
        }
        let (column, row) = arrow.unwrap();
        assert!(!app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE
        }));
        assert!(app.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE
        }));
        assert_eq!(app.display_file(0).lines[1].new_line, Some(31));
        super::super::tests::press(&mut app, KeyCode::Char('z'));
        super::super::tests::press(&mut app, KeyCode::Char('e'));
        assert_eq!(app.display_file(0).lines[1].new_line, Some(11));
        super::super::tests::press(&mut app, KeyCode::Char('z'));
        super::super::tests::press(&mut app, KeyCode::Char('E'));
        assert_eq!(app.display_file(0).lines[0].new_line, Some(1));
    }
    #[test]
    fn gap_actions_follow_visible_labels_and_ignore_separator_and_end() {
        let (_fixture, mut app) = expanded();
        assert_eq!(app.context_gap_action(0, 0), Some((false, true)));
        assert_eq!(app.context_gap_action(0, "↑ 20行".len()), None);
        let row = *app.expanded.as_ref().unwrap().gaps.keys().max().unwrap();
        let text = app.display_file(0).lines[row].text.clone();
        assert!(text.starts_with("↓ 20行  [全"));
        assert!(!text.contains('↑'));
        assert_eq!(app.context_gap_action(row, 0), Some((false, false)));
        assert_eq!(
            app.context_gap_action(row, "↓ 20行  ".len()),
            Some((true, false))
        );
        assert_eq!(app.context_gap_action(row, text.len()), None);
        app.expand_gap(0, false, true);
        app.expand_gap(0, false, true);
        assert_eq!(app.context_gap_action(0, 0), Some((true, true)));
    }
    #[test]
    fn lazy_load_preserves_original_content_hit_but_keeps_zero_at_top() {
        for offset in [0, 3] {
            let (_fixture, mut app) = expanded();
            let expanded = app.expanded.take().unwrap();
            let anchor = app.comparison.files[0].lines[offset].clone();
            app.diff_offset = offset;
            app.cursor.position = Some(super::super::cursor::Position { row: 3, byte: 2 });
            app.cursor.anchor = Some(super::super::cursor::Position { row: 1, byte: 1 });
            let (tx, rx) = mpsc::channel();
            tx.send(Ok(expanded)).unwrap();
            app.context_job = Some(rx);
            app.context_attempt = Some(app.original_color_key(0));
            assert!(app.poll_context());
            let cursor = app.cursor.position.unwrap();
            let start = app.cursor.anchor.unwrap();
            assert_eq!(
                (start.row, start.byte, cursor.row, cursor.byte),
                (2, 1, 4, 2)
            );
            if offset == 0 {
                assert_eq!(app.diff_offset, 0);
            } else {
                let row = &app.display_file(0).lines[app.diff_offset];
                assert_eq!(
                    (row.old_line, row.new_line, row.text.as_str()),
                    (anchor.old_line, anchor.new_line, anchor.text.as_str())
                );
            }
        }
    }
    #[test]
    fn metadata_only_text_can_expand_without_interpreting_hunk_shaped_code() {
        let (_fixture, app) = expanded();
        let mut file = app.comparison.files[0].clone();
        file.lines = vec![DiffLine {
            kind: LineKind::Header,
            text: "old mode 100644".into(),
            old_line: None,
            new_line: None,
        }];
        let mut e = Expanded::new(
            app.original_color_key(0),
            file,
            "@@ -3 +3 @@\nplain\n".into(),
        );
        assert_eq!(e.gaps.len(), 1);
        e.full = true;
        e.rebuild();
        assert_eq!(e.file.lines.len(), 3);
        assert_eq!(e.file.lines[1].kind, LineKind::Context);
        assert_eq!(
            (e.file.lines[1].old_line, e.file.lines[1].new_line),
            (Some(1), Some(1))
        );
    }
}
