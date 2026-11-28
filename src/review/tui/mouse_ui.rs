//! Mouse actions reuse the review's keyboard navigation and search handlers.
use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MouseAction {
    Key(KeyCode),
    AllFiles,
    ToggleFileBrowser,
    PathFilter,
    BrowserRow(usize),
    Compare,
    History,
    SelectRow(usize),
    SelectTab(usize),
    MarkRow(usize),
    ExpandGap {
        row: usize,
        all: bool,
        backwards: bool,
    },
}

impl App {
    pub(super) fn register_click(&self, area: Rect, action: MouseAction) {
        if area.area() > 0 {
            self.mouse_targets.borrow_mut().push((area, action));
        }
    }

    pub(super) fn draw_click_buttons(
        &self,
        frame: &mut Frame,
        area: Rect,
        buttons: &[(&str, MouseAction)],
    ) {
        let mut x = area.x;
        for &(label, action) in buttons {
            let label = Span::styled(format!("[{label}]"), Style::default().fg(Color::Cyan));
            let width = label.width() as u16;
            if width > area.right().saturating_sub(x) {
                break;
            }
            let button = Rect::new(x, area.y, width, area.height.min(1));
            self.register_click(button, action);
            frame.render_widget(Paragraph::new(label), button);
            x = button.right().saturating_add(1);
        }
    }

    pub(super) fn draw_click_tabs(
        &self,
        frame: &mut Frame,
        area: Rect,
        labels: &[&str],
        active: usize,
    ) {
        let divider = " │ ";
        let mut x = area.x;
        for (index, &label) in labels.iter().enumerate() {
            let width = Span::raw(label).width() as u16;
            let visible_width = width.min(area.right().saturating_sub(x));
            self.register_click(
                Rect::new(x, area.y, visible_width, area.height.min(1)),
                MouseAction::SelectTab(index),
            );
            x = x
                .saturating_add(width)
                .saturating_add(Span::raw(divider).width() as u16);
        }
        frame.render_widget(
            Tabs::new(labels.iter().copied())
                .padding("", "")
                .divider(divider)
                .select(active)
                .highlight_style(selected_style()),
            area,
        );
    }

    pub(super) fn register_list_rows(&self, area: Rect, start: usize, count: usize) {
        for row in 0..area.height.min(count.min(u16::MAX as usize) as u16) {
            self.register_click(
                Rect::new(area.x, area.y + row, area.width, 1),
                MouseAction::SelectRow(start + usize::from(row)),
            );
        }
    }

    pub(super) fn draw_gap_controls(&self, frame: &mut Frame, area: Rect, row: usize, text: &str) {
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(text).style(Style::default().fg(Color::Cyan)),
            area,
        );
        let mut column = area.x;
        let mut byte = 0;
        for label in text.split("  ") {
            let width = Span::raw(label).width() as u16;
            if let Some((all, backwards)) = self.context_gap_action(row, byte) {
                let button = Rect::new(column, area.y, width, 1).intersection(area);
                self.register_click(
                    button,
                    MouseAction::ExpandGap {
                        row,
                        all,
                        backwards,
                    },
                );
            }
            column = column.saturating_add(width).saturating_add(2);
            byte += label.len() + 2;
        }
    }

    pub(super) fn mouse_control_at(&self, column: u16, row: u16) -> Option<MouseAction> {
        self.mouse_targets
            .borrow()
            .iter()
            .rev()
            .find(|(area, _)| area.contains((column, row).into()))
            .map(|(_, action)| *action)
    }

    pub(super) fn activate_mouse_control(&mut self, action: MouseAction) {
        self.file_browser.editing = false;
        if matches!(action, MouseAction::SelectRow(_) | MouseAction::MarkRow(_))
            && self.inline_code()
        {
            self.overlay = Overlay::None;
        }
        match action {
            MouseAction::Key(key) => self.mouse_key(key),
            MouseAction::AllFiles => self.open_all_files(),
            MouseAction::ToggleFileBrowser => self.toggle_file_browser(),
            MouseAction::PathFilter => self.start_path_filter(),
            MouseAction::BrowserRow(index) => self.open_browser_row(index, Focus::Files),
            MouseAction::History => self.history.enabled = !self.history.enabled,
            MouseAction::Compare => {
                self.main_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
            }
            MouseAction::SelectRow(index) => self.activate_mouse_row(index),
            MouseAction::SelectTab(index) => self.activate_mouse_tab(index),
            MouseAction::ExpandGap {
                row,
                all,
                backwards,
            } => {
                self.focus = Focus::Diff;
                self.expand_gap(row, all, backwards);
            }
            MouseAction::MarkRow(index) => {
                if self.select_changed_row(index, false) {
                    self.mouse_key(KeyCode::Char(' '));
                }
            }
        }
    }

    fn mouse_key(&mut self, code: KeyCode) {
        let exit = self.key(KeyEvent::new(code, KeyModifiers::NONE));
        self.exit_requested |= exit;
    }

    fn activate_mouse_row(&mut self, index: usize) {
        match &mut self.overlay {
            Overlay::None => {
                self.select_changed_row(index, true);
                return;
            }
            Overlay::AllFiles(search) => search.select_row(index),
            Overlay::Symbols(search) => {
                search.select_row(index);
                return;
            }
            Overlay::Branches(picker) => picker.selected = index,
            Overlay::References { selected, .. } => {
                *selected = index;
                return;
            }
            _ => return,
        }
        self.mouse_key(KeyCode::Enter);
    }

    fn activate_mouse_tab(&mut self, index: usize) {
        let active = match &self.overlay {
            Overlay::AllFiles(search) => search.active_tab(),
            Overlay::Symbols(search) => search.active_tab(),
            Overlay::Branches(picker) => picker.target,
            Overlay::References { tab, .. } => *tab,
            _ => return,
        };
        if index != active {
            self.mouse_key(KeyCode::Tab);
        }
    }

    pub(super) fn scroll_by_mouse(&mut self, mouse: MouseEvent) -> bool {
        let (code, steps) = match mouse.kind {
            MouseEventKind::ScrollUp => (KeyCode::Up, 3),
            MouseEventKind::ScrollDown => (KeyCode::Down, 3),
            MouseEventKind::ScrollLeft => (KeyCode::Left, 1),
            MouseEventKind::ScrollRight => (KeyCode::Right, 1),
            _ => return false,
        };
        let previous_focus = self.focus;
        if matches!(self.overlay, Overlay::None) || self.browser_source() {
            let position = (mouse.column, mouse.row).into();
            if self.mouse_panes[0].contains(position) {
                self.focus = Focus::Files;
            } else if self.mouse_panes[1].contains(position) {
                self.focus = Focus::Diff;
            } else {
                return false;
            }
        }
        for _ in 0..steps {
            self.mouse_key(code);
        }
        self.focus = previous_focus;
        true
    }
}
