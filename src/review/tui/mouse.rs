//! Keep hit testing on the exact Paragraph lines and viewport used for drawing.
use super::*;
use ratatui::buffer::CellWidth;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MouseHit {
    Control(MouseAction),
    Pane(Focus),
    Code(usize, usize),
}

pub(super) struct CodeViewport {
    area: Rect,
    offset: usize,
    horizontal: u16,
    lines: Vec<Line<'static>>,
    prefixes: Vec<usize>,
}

impl CodeViewport {
    pub(super) fn height(&self) -> u16 {
        self.area.height
    }
    pub(super) fn width(&self) -> u16 {
        self.area.width
    }

    // Match Paragraph's LineTruncator, including clipping inside wide glyphs.
    // Both mouse hit testing and cursor painting use this exact visible-cell walk.
    fn cells(&self, index: usize) -> Vec<(u16, u16, usize)> {
        let Some(line) = self.lines.get(index) else {
            return Vec::new();
        };
        let mut skip = self.horizontal;
        let mut x = 0;
        let mut byte = 0;
        let mut result = Vec::new();
        for grapheme in line.styled_graphemes(Style::default()) {
            let width = grapheme.symbol.cell_width();
            let start = byte;
            byte += grapheme.symbol.len();
            if width > self.area.width {
                continue;
            }
            if x + width > self.area.width {
                break;
            }
            if skip >= width {
                skip -= width;
                continue;
            }
            skip = 0;
            if let Some(byte) = start.checked_sub(self.prefixes[index]) {
                result.push((x, width, byte));
            }
            x += width;
        }
        if byte == self.prefixes[index] && x < self.area.width {
            result.push((x, 1, 0));
        }
        result
    }

    pub(super) fn visible_byte(&self, row: usize, byte: usize) -> Option<usize> {
        let index = row.checked_sub(self.offset)?;
        let cells = self.cells(index);
        if cells.iter().any(|(_, _, b)| *b == byte) {
            Some(byte)
        } else {
            cells.first().map(|(_, _, b)| *b)
        }
    }

    pub(super) fn draw_cursor(
        &self,
        frame: &mut Frame,
        cursor: &cursor::CodeCursor,
        show_selection: bool,
        valid: impl Fn(usize) -> bool,
    ) {
        let mut position = None;
        for index in 0..self.lines.len().min(self.area.height as usize) {
            let row = self.offset + index;
            if !valid(row)
                || (cursor.anchor.is_none() && cursor.position.is_none_or(|p| p.row != row))
            {
                continue;
            }
            for (x, width, byte) in self.cells(index) {
                if show_selection && cursor.selected(row, byte) {
                    for dx in 0..width {
                        let cell = &mut frame.buffer_mut()
                            [(self.area.x + x + dx, self.area.y + index as u16)];
                        cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
                    }
                }
                if cursor.position == Some(cursor::Position { row, byte }) {
                    position = Some((self.area.x + x, self.area.y + index as u16));
                }
            }
        }
        if let Some(position) = position {
            frame.set_cursor_position(position);
        }
    }

    fn hit(&self, column: u16, row: u16) -> Option<(usize, usize)> {
        if !self.area.contains((column, row).into()) {
            return None;
        }
        let index = usize::from(row - self.area.y);
        self.cells(index)
            .into_iter()
            .find(|(x, width, _)| (*x..x + width).contains(&(column - self.area.x)))
            .map(|(_, _, byte)| (self.offset + index, byte))
    }
}

impl App {
    pub(super) fn record_code_viewport(
        &self,
        area: Rect,
        offset: usize,
        horizontal: u16,
        lines: &[Line<'static>],
        prefixes: Vec<usize>,
    ) {
        *self.code_viewport.borrow_mut() = Some(CodeViewport {
            area,
            offset,
            horizontal,
            lines: lines.to_vec(),
            prefixes,
        });
    }

    fn mouse_hit(&self, column: u16, row: u16) -> Option<MouseHit> {
        if let Some(action) = self.mouse_control_at(column, row) {
            return Some(MouseHit::Control(action));
        }
        if matches!(self.overlay, Overlay::None)
            && self.focus == Focus::Files
            && self.mouse_panes[1].contains((column, row).into())
        {
            return Some(MouseHit::Pane(Focus::Diff));
        }
        if let Some((row, byte)) = self
            .code_viewport
            .borrow()
            .as_ref()
            .and_then(|view| view.hit(column, row))
        {
            return Some(MouseHit::Code(row, byte));
        }
        if matches!(self.overlay, Overlay::None) || self.inline_code() {
            for (index, area) in self.mouse_panes.iter().enumerate() {
                if area.contains((column, row).into()) {
                    return Some(MouseHit::Pane(if index == 0 {
                        Focus::Files
                    } else {
                        Focus::Diff
                    }));
                }
            }
        }
        None
    }

    pub(super) fn mouse(&mut self, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.mouse_down = self
                    .mouse_hit(mouse.column, mouse.row)
                    .map(|hit| (mouse.column, mouse.row, hit));
                false
            }
            MouseEventKind::Drag(_) => {
                self.mouse_down = None;
                false
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some((column, row, hit)) = self.mouse_down.take() else {
                    return false;
                };
                if (column, row) != (mouse.column, mouse.row)
                    || self.mouse_hit(column, row) != Some(hit)
                {
                    return false;
                }
                match hit {
                    MouseHit::Pane(focus) => {
                        if focus == Focus::Files && self.inline_code() {
                            self.overlay = Overlay::None;
                        }
                        self.focus = focus;
                        true
                    }
                    MouseHit::Control(action) => {
                        self.activate_mouse_control(action);
                        true
                    }
                    MouseHit::Code(row, byte) => {
                        self.click_symbol(row, byte, mouse.modifiers.contains(KeyModifiers::SHIFT))
                    }
                }
            }
            _ => self.scroll_by_mouse(mouse),
        }
    }

    pub(super) fn click_symbol(&mut self, row: usize, byte: usize, usages: bool) -> bool {
        let target = match &self.overlay {
            Overlay::None => {
                let Some(index) = self.current() else {
                    return false;
                };
                let file = self.display_file(index);
                let Some(line) = file.lines.get(row) else {
                    return false;
                };
                if line.kind == LineKind::Header {
                    return false;
                }
                let (commit, path) = if line.kind == LineKind::Removed {
                    (self.comparison.merge_base.as_str(), file.old_path.as_str())
                } else {
                    (self.comparison.head_commit.as_str(), file.new_path.as_str())
                };
                (commit.to_owned(), path.to_owned(), clean(&line.text))
            }
            Overlay::Source(source) => {
                let Some(text) = source.lines.get(row) else {
                    return false;
                };
                (source.commit.clone(), source.path.clone(), text.clone())
            }
            Overlay::SymbolPicker(picker) => {
                let Some(line) = picker.lines.get(row) else {
                    return false;
                };
                let (commit, path) = picker.snapshot_for_row(row);
                (commit.to_owned(), path.to_owned(), line.text.clone())
            }
            _ => return false,
        };
        let (commit, path, text) = target;
        let Some(range) = crate::review::symbols::pickable_identifiers(&text, &path)
            .into_iter()
            .find(|range| range.contains(&byte))
        else {
            return false;
        };
        let name = text[range].to_owned();
        let origin = match std::mem::replace(&mut self.overlay, Overlay::None) {
            Overlay::Source(source) => Some(Box::new(source)),
            Overlay::SymbolPicker(mut picker) => picker.origin.take(),
            _ => None,
        };
        self.search_symbol(
            commit,
            path,
            name,
            if usages {
                SymbolMode::Usages
            } else {
                SymbolMode::Definitions
            },
            origin,
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{buffer::Buffer, widgets::Widget};

    #[test]
    fn pane_clicks_select_before_search_and_wheel_preserves_keyboard_focus() {
        use ratatui::{backend::TestBackend, Terminal};
        let (_fixture, mut app) = super::super::tests::app();
        app.focus = Focus::Files;
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let right = app.mouse_panes[1];
        let click = |app: &mut App, x, y| {
            let event = |kind| MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            };
            app.mouse(event(MouseEventKind::Down(MouseButton::Left)));
            app.mouse(event(MouseEventKind::Up(MouseButton::Left)))
        };
        // A code click while the left pane is active only selects the right pane.
        let (x, y) = {
            let viewport = app.code_viewport.borrow();
            let view = viewport.as_ref().unwrap();
            (view.area.x + view.prefixes[0] as u16 + 4, view.area.y)
        };
        assert!(click(&mut app, x, y));
        assert!(app.focus == Focus::Diff);
        assert!(matches!(app.overlay, Overlay::None));
        assert!(click(&mut app, x, y));
        assert!(matches!(app.overlay, Overlay::Symbols(_)));
        super::super::tests::press(&mut app, KeyCode::Esc);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        app.focus = Focus::Files;
        assert!(click(&mut app, right.x, right.y));
        assert!(app.focus == Focus::Diff);
        assert!(matches!(app.overlay, Overlay::None));
        let left = app.mouse_panes[0];
        app.mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: left.x + 1,
            row: left.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.focus == Focus::Diff);
        app.focus = Focus::Files;
        let file = app.current();
        app.diff_offset = 0;
        app.mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: right.x + 1,
            row: right.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.focus == Focus::Files);
        assert_eq!(app.current(), file);
        assert!(app.diff_offset > 0);
        super::super::tests::press(&mut app, KeyCode::Down);
        assert!(app.focus == Focus::Files);
        // A modal absorbs clicks, including clicks on the pane's title behind it.
        app.overlay = Overlay::Help(0);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(!click(&mut app, right.x, right.y));
        assert!(matches!(app.overlay, Overlay::Help(_)));
    }

    #[test]
    fn source_and_name_selection_render_inside_right_pane() {
        use ratatui::{backend::TestBackend, Terminal};
        let (_fixture, mut app) = super::super::tests::app();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        app.overlay = Overlay::Source(Source::new(
            "head".into(),
            "a.rs".into(),
            1,
            "fn value() {}".into(),
        ));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(app.code_viewport.borrow().as_ref().unwrap().area.x >= app.mouse_panes[1].x);
        super::super::tests::press(&mut app, KeyCode::Enter);
        assert!(matches!(app.overlay, Overlay::Source(_)));
        super::super::tests::press(&mut app, KeyCode::Char('s'));
        assert!(matches!(app.overlay, Overlay::SymbolPicker(_)));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(app.code_viewport.borrow().as_ref().unwrap().area.x >= app.mouse_panes[1].x);
        super::super::tests::press(&mut app, KeyCode::Char('q'));
        assert!(matches!(app.overlay, Overlay::Source(_)));
        super::super::tests::press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.focus == Focus::Files);
    }

    #[test]
    fn inline_views_keep_left_file_clicks_and_tab_accessible() {
        use ratatui::{backend::TestBackend, Terminal};
        let (_fixture, mut app) = super::super::tests::app();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        for picker in [false, true] {
            app.focus = Focus::Diff;
            let source = Source::new("head".into(), "a.rs".into(), 1, "fn value() {}".into());
            if picker {
                app.open_source_symbol_picker(source);
            } else {
                app.overlay = Overlay::Source(source);
            }
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let action = app
                .mouse_targets
                .borrow()
                .iter()
                .find(|(_, action)| *action == MouseAction::SelectRow(1))
                .unwrap()
                .1;
            app.activate_mouse_control(action);
            assert!(matches!(app.overlay, Overlay::None));
            assert_eq!(app.current(), Some(1));
            assert!(app.focus == Focus::Files);
        }
        app.focus = Focus::Diff;
        app.open_diff_symbol_picker();
        super::super::tests::press(&mut app, KeyCode::Tab);
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.focus == Focus::Files);
    }

    #[test]
    fn a_button_remains_clickable_during_an_unrelated_redraw() {
        use ratatui::{backend::TestBackend, Terminal};
        let (_fixture, mut app) = super::super::tests::app();
        app.overlay = Overlay::Help(0);
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let area = app
            .mouse_targets
            .borrow()
            .iter()
            .find(|(_, action)| *action == MouseAction::Key(KeyCode::Esc))
            .unwrap()
            .0;
        let click = |kind| MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(click(MouseEventKind::Down(MouseButton::Left)));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(app.mouse(click(MouseEventKind::Up(MouseButton::Left))));
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn hit_positions_match_paragraph_with_wide_glyphs_and_horizontal_scroll() {
        let line = Line::from(vec![
            Span::raw("  名"),
            Span::styled("前", selected_style()),
            Span::raw("x yz"),
        ]);
        for width in 1..12 {
            for horizontal in 0..12 {
                let area = Rect::new(3, 4, width, 1);
                let mut buffer = Buffer::empty(area);
                Paragraph::new(vec![line.clone()])
                    .scroll((0, horizontal))
                    .render(area, &mut buffer);
                let viewport = CodeViewport {
                    area,
                    offset: 17,
                    horizontal,
                    lines: vec![line.clone()],
                    prefixes: vec![0],
                };
                let text = line.to_string();
                for x in area.x..area.right() {
                    if let Some((row, byte)) = viewport.hit(x, area.y) {
                        assert_eq!(row, 17);
                        let glyph = text[byte..].chars().next().unwrap().to_string();
                        let rendered = buffer[(x, area.y)].symbol();
                        // A wide glyph's second cell belongs to that same glyph.
                        assert!(rendered == glyph || (x > area.x && buffer[(x - 1, area.y)].symbol() == glyph), "width={width} offset={horizontal} x={x} expected={glyph} rendered={rendered}");
                    }
                }
                assert_eq!(viewport.hit(area.right(), area.y), None);
            }
        }
    }
}
