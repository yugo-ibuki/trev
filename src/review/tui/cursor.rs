//! Character movement uses the same graphemes as Paragraph and mouse hit testing.
use super::*;
use ratatui::buffer::CellWidth;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Position {
    pub row: usize,
    pub byte: usize,
}

#[derive(Default)]
pub(super) struct CodeCursor {
    pub position: Option<Position>,
    pub anchor: Option<Position>,
    pending_g: bool,
    pending_inner: bool,
    pending_around: bool,
    pending_yank: bool,
    pending_find: Option<char>,
    last_find: Option<(char, char)>,
    pending_z: bool,
    count: usize,
    linewise: bool,
}

fn boundaries(text: &str) -> Vec<usize> {
    let line = Line::raw(text);
    let mut byte = 0;
    let mut result = Vec::new();
    for glyph in line.styled_graphemes(Style::default()) {
        result.push(byte);
        byte += glyph.symbol.len();
    }
    if result.is_empty() {
        result.push(0);
    }
    result
}

fn word_class(text: &str, byte: usize, big: bool) -> u8 {
    match text[byte..].chars().next() {
        None => 0,
        Some(c) if c.is_whitespace() => 0,
        Some(c) if big || c.is_alphanumeric() || c == '_' => 1,
        Some(_) => 2,
    }
}

enum Rows<'a> {
    Diff(&'a [crate::review::git::DiffLine]),
    Source(&'a [String]),
    #[cfg(test)]
    Test(&'a [Option<String>]),
}

impl Rows<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Diff(r) => r.len(),
            Self::Source(r) => r.len(),
            #[cfg(test)]
            Self::Test(r) => r.len(),
        }
    }
    fn valid(&self, row: usize) -> bool {
        match self {
            Self::Diff(r) => r.get(row).is_some_and(|r| r.kind != LineKind::Header),
            Self::Source(r) => row < r.len(),
            #[cfg(test)]
            Self::Test(r) => r.get(row).is_some_and(Option::is_some),
        }
    }
    fn text(&self, row: usize) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Diff(r) => clean(&r[row].text).into(),
            Self::Source(r) => r[row].as_str().into(),
            #[cfg(test)]
            Self::Test(r) => r[row].as_deref().unwrap().into(),
        }
    }
}

impl CodeCursor {
    pub(super) fn cancel_pending(&mut self) {
        self.pending_g = false;
        self.pending_inner = false;
        self.pending_around = false;
        self.pending_yank = false;
        self.pending_find = None;
        self.pending_z = false;
        self.count = 0;
        if self.anchor.is_none() {
            self.linewise = false;
        }
    }

    pub(super) fn remap_rows(&mut self, row_for: impl Fn(usize) -> Option<usize>) {
        let remap =
            |position: Position| row_for(position.row).map(|row| Position { row, ..position });
        self.position = self.position.and_then(remap);
        self.anchor = self.position.and(self.anchor.and_then(remap));
    }

    fn normalize(&mut self, rows: &Rows<'_>, offset: usize, height: usize) {
        let end = offset.saturating_add(height.max(1));
        if self
            .position
            .is_none_or(|p| p.row < offset || p.row >= end || !rows.valid(p.row))
        {
            self.position = (offset..end.min(rows.len()))
                .find(|&row| rows.valid(row))
                .map(|row| Position { row, byte: 0 });
            self.anchor = None;
        }
        if let Some(p) = &mut self.position {
            let text = rows.text(p.row);
            p.byte = boundaries(&text)
                .into_iter()
                .take_while(|&b| b <= p.byte)
                .last()
                .unwrap_or(0);
        }
    }

    fn move_by(&mut self, key: char, rows: &Rows<'_>) {
        let Some(mut p) = self.position else { return };
        let text = rows.text(p.row);
        let current = boundaries(&text);
        let column = current.iter().position(|&b| b == p.byte).unwrap_or(0);
        match key {
            'h' => p.byte = current[column.saturating_sub(1)],
            'l' => p.byte = current[(column + 1).min(current.len() - 1)],
            'j' | 'k' => {
                let row = if key == 'j' {
                    (p.row + 1..rows.len()).find(|&r| rows.valid(r))
                } else {
                    (0..p.row).rev().find(|&r| rows.valid(r))
                };
                if let Some(row) = row {
                    let next = boundaries(&rows.text(row));
                    p = Position {
                        row,
                        byte: next[column.min(next.len() - 1)],
                    };
                }
            }
            _ => {}
        }
        self.position = Some(p);
    }

    #[cfg(test)]
    fn inner_word(&mut self, rows: &Rows<'_>) {
        self.word_object(rows, false, false);
    }

    fn word_object(&mut self, rows: &Rows<'_>, big: bool, around: bool) {
        let Some(p) = self.position else { return };
        let text = rows.text(p.row);
        if text.is_empty() {
            return;
        }
        let starts = boundaries(&text);
        let column = starts.iter().position(|&b| b == p.byte).unwrap_or(0);
        // Symbol navigation identifies language names; Vim's text object also
        // needs whitespace and punctuation runs, so use the displayed graphemes.
        let class = |byte| word_class(&text, byte, big);
        let kind = class(starts[column]);
        let mut start = column;
        let mut end = column;
        while start > 0 && class(starts[start - 1]) == kind {
            start -= 1;
        }
        while end + 1 < starts.len() && class(starts[end + 1]) == kind {
            end += 1;
        }
        if around {
            let original_end = end;
            while end + 1 < starts.len() && class(starts[end + 1]) == 0 {
                end += 1;
            }
            if end == original_end {
                while start > 0 && class(starts[start - 1]) == 0 {
                    start -= 1;
                }
            }
        }
        self.linewise = false;
        self.anchor = Some(Position {
            row: p.row,
            byte: starts[start],
        });
        self.position = Some(Position {
            row: p.row,
            byte: starts[end],
        });
    }

    fn line_edge(&mut self, rows: &Rows<'_>, key: char) {
        if let Some(p) = &mut self.position {
            let text = rows.text(p.row);
            p.byte = match key {
                '$' => *boundaries(&text).last().unwrap(),
                '^' => text
                    .char_indices()
                    .find(|(_, c)| !c.is_whitespace())
                    .map_or(0, |(b, _)| b),
                _ => 0,
            };
        }
    }

    fn goto_line(&mut self, rows: &Rows<'_>, line: Option<usize>, last: bool) {
        let row = if let Some(line) = line {
            (0..rows.len()).filter(|&r| rows.valid(r)).min_by_key(|&r| {
                let number = match rows {
                    Rows::Diff(lines) => lines[r]
                        .new_line
                        .or(lines[r].old_line)
                        .map(|n| n as usize)
                        .unwrap_or(r + 1),
                    _ => r + 1,
                };
                number.abs_diff(line)
            })
        } else if last {
            (0..rows.len()).rev().find(|&r| rows.valid(r))
        } else {
            (0..rows.len()).find(|&r| rows.valid(r))
        };
        self.position = row.map(|row| Position { row, byte: 0 });
        self.line_edge(rows, '^');
    }

    fn word_motion(&mut self, rows: &Rows<'_>, key: char) {
        let Some(p) = self.position else { return };
        // Symbol lookup identifies language names; Vim motions instead use the
        // displayed graphemes and visit only rows traversed by this motion.
        let big = key.is_uppercase();
        let backwards = matches!(key, 'b' | 'B');
        let end_of_word = matches!(key, 'e' | 'E');
        let row_range: Box<dyn Iterator<Item = usize>> = if backwards {
            Box::new((0..=p.row).rev())
        } else {
            Box::new(p.row..rows.len())
        };
        let mut fallback = p;
        for row in row_range {
            if !rows.valid(row) {
                continue;
            }
            let text = rows.text(row);
            let starts = boundaries(&text);
            let indices: Box<dyn Iterator<Item = usize>> = if backwards {
                Box::new((0..starts.len()).rev())
            } else {
                Box::new(0..starts.len())
            };
            for i in indices {
                let pos = Position {
                    row,
                    byte: starts[i],
                };
                if (backwards && pos >= p) || (!backwards && pos <= p) {
                    continue;
                }
                fallback = pos;
                let kind = word_class(&text, starts[i], big);
                if text.is_empty() {
                    self.position = Some(pos);
                    return;
                }
                if kind == 0 {
                    continue;
                }
                let boundary = if end_of_word {
                    i + 1 == starts.len() || word_class(&text, starts[i + 1], big) != kind
                } else {
                    i == 0 || word_class(&text, starts[i - 1], big) != kind
                };
                if boundary {
                    self.position = Some(pos);
                    return;
                }
            }
        }
        self.position = Some(fallback);
    }

    fn find_char(&mut self, rows: &Rows<'_>, kind: char, target: char, count: usize) {
        let Some(mut p) = self.position else { return };
        let text = rows.text(p.row);
        let starts = boundaries(&text);
        let current = starts.iter().position(|&b| b == p.byte).unwrap_or(0);
        let forward = matches!(kind, 'f' | 't');
        let found = if forward {
            (current + 1..starts.len())
                .filter(|&i| text[starts[i]..].starts_with(target))
                .nth(count - 1)
        } else {
            (0..current)
                .rev()
                .filter(|&i| text[starts[i]..].starts_with(target))
                .nth(count - 1)
        };
        if let Some(mut i) = found {
            if kind == 't' {
                i = i.saturating_sub(1);
            }
            if kind == 'T' {
                i = (i + 1).min(starts.len() - 1);
            }
            p.byte = starts[i];
            self.position = Some(p);
        }
    }

    fn selected_identifier(&self, rows: &Rows<'_>) -> Option<(usize, String)> {
        let (anchor, position) = (self.anchor?, self.position?);
        if self.linewise || anchor.row != position.row {
            return None;
        }
        let selected = self.selected_text(rows)?;
        let text = selected.trim().to_owned();
        crate::review::symbols::valid_identifier(&text).then_some((position.row, text))
    }

    pub(super) fn selected_diff_identifier(
        &self,
        lines: &[crate::review::git::DiffLine],
    ) -> Option<(usize, String)> {
        self.selected_identifier(&Rows::Diff(lines))
    }

    pub(super) fn selected_source_identifier(&self, lines: &[String]) -> Option<String> {
        self.selected_identifier(&Rows::Source(lines))
            .map(|(_, text)| text)
    }

    fn selected_text(&self, rows: &Rows<'_>) -> Option<String> {
        let (anchor, position) = (self.anchor?, self.position?);
        let (start, end) = (anchor.min(position), anchor.max(position));
        if !rows.valid(start.row) || !rows.valid(end.row) {
            return None;
        }
        let mut lines = Vec::new();
        for row in start.row..=end.row {
            if !rows.valid(row) {
                continue;
            }
            let text = rows.text(row);
            let from = if row == start.row && !self.linewise {
                start.byte.min(text.len())
            } else {
                0
            };
            let to = if row == end.row && !self.linewise {
                boundaries(&text)
                    .into_iter()
                    .find(|&b| b > end.byte)
                    .unwrap_or(text.len())
            } else {
                text.len()
            };
            lines.push(text.get(from..to)?.to_owned());
        }
        Some(lines.join("\n") + if self.linewise { "\n" } else { "" })
    }

    fn yank(&mut self, rows: &Rows<'_>, copy: impl FnOnce(String) -> Result<()>) -> Result<bool> {
        let Some(text) = self.selected_text(rows).filter(|text| !text.is_empty()) else {
            return Ok(false);
        };
        copy(text)?;
        self.position = self.position.zip(self.anchor).map(|(p, a)| p.min(a));
        self.anchor = None;
        self.linewise = false;
        Ok(true)
    }

    pub(super) fn selected(&self, row: usize, byte: usize) -> bool {
        let (Some(anchor), Some(position)) = (self.anchor, self.position) else {
            return false;
        };
        if self.linewise {
            return (anchor.row.min(position.row)..=anchor.row.max(position.row)).contains(&row);
        }
        (anchor.min(position)..=anchor.max(position)).contains(&Position { row, byte })
    }
}

impl App {
    fn cursor_rows(&self) -> Rows<'_> {
        match &self.overlay {
            Overlay::Source(source) => Rows::Source(&source.lines),
            _ => Rows::Diff(
                self.current()
                    .map_or(&[][..], |i| &self.display_file(i).lines),
            ),
        }
    }

    fn cursor_height(&self) -> usize {
        self.code_viewport
            .borrow()
            .as_ref()
            .map_or(self.page_height, |v| usize::from(v.height()))
            .max(1)
    }

    fn take_cursor(&mut self) -> CodeCursor {
        match &mut self.overlay {
            Overlay::Source(s) => std::mem::take(&mut s.cursor),
            _ => std::mem::take(&mut self.cursor),
        }
    }
    fn put_cursor(&mut self, cursor: CodeCursor) {
        match &mut self.overlay {
            Overlay::Source(s) => s.cursor = cursor,
            _ => self.cursor = cursor,
        }
    }
    fn cursor_offsets(&self) -> (usize, u16) {
        match &self.overlay {
            Overlay::Source(s) => (s.offset, s.horizontal),
            _ => (self.diff_offset, self.horizontal),
        }
    }
    fn code_cursor_visible(&self) -> bool {
        match self.overlay {
            Overlay::None => self.current().is_some() && !self.folder_focused(),
            Overlay::Source(_) => self.focus == Focus::Diff,
            _ => false,
        }
    }

    pub(super) fn prepare_code_cursor(&mut self) {
        if !self.code_cursor_visible() {
            return;
        }
        let mut cursor = self.take_cursor();
        cursor.normalize(
            &self.cursor_rows(),
            self.cursor_offsets().0,
            self.cursor_height(),
        );
        self.put_cursor(cursor);
    }

    pub(super) fn align_code_cursor(&mut self) {
        if !self.code_cursor_visible() {
            return;
        }
        let mut cursor = self.take_cursor();
        if let (Some(p), Some(view)) = (cursor.position, self.code_viewport.borrow().as_ref()) {
            if let Some(byte) = view.visible_byte(p.row, p.byte) {
                if byte != p.byte {
                    cursor.position = Some(Position { row: p.row, byte });
                    cursor.anchor = None;
                }
            }
        }
        self.put_cursor(cursor);
    }

    pub(super) fn code_cursor_key(&mut self, key: KeyEvent) -> bool {
        if self.focus != Focus::Diff || !matches!(self.overlay, Overlay::None | Overlay::Source(_))
        {
            return false;
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            return false;
        }
        self.prepare_code_cursor();
        let mut cursor = self.take_cursor();
        if matches!(key.code, KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab) {
            cursor.cancel_pending();
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control {
            let steps = match key.code {
                KeyCode::Char('j' | 'k') => 20,
                KeyCode::Char('d' | 'u') => self.cursor_height().max(2) / 2,
                KeyCode::Char('f' | 'b') => self.cursor_height(),
                _ => {
                    cursor.count = 0;
                    self.put_cursor(cursor);
                    return false;
                }
            };
            let direction = if matches!(key.code, KeyCode::Char('k' | 'u' | 'b')) {
                'k'
            } else {
                'j'
            };
            let distance = steps
                .saturating_mul(cursor.count.max(1))
                .min(self.cursor_rows().len());
            let previous_offset = self.cursor_offsets().0;
            for _ in 0..distance {
                let previous = cursor.position;
                cursor.move_by(direction, &self.cursor_rows());
                if cursor.position == previous {
                    break;
                }
            }
            let max_offset = self
                .cursor_rows()
                .len()
                .saturating_sub(self.cursor_height());
            let offset = if direction == 'k' {
                previous_offset.saturating_sub(distance)
            } else {
                previous_offset.saturating_add(distance).min(max_offset)
            };
            match &mut self.overlay {
                Overlay::Source(s) => s.offset = offset,
                _ => self.diff_offset = offset,
            }
            cursor.cancel_pending();
            self.reveal_code_cursor(&cursor);
            self.put_cursor(cursor);
            return true;
        }
        if let Some(kind) = cursor.pending_find.take() {
            if let KeyCode::Char(target) = key.code {
                cursor.find_char(&self.cursor_rows(), kind, target, cursor.count.max(1));
                cursor.last_find = Some((kind, target));
                cursor.count = 0;
                self.reveal_code_cursor(&cursor);
                self.put_cursor(cursor);
                return true;
            }
        }
        let inner = std::mem::take(&mut cursor.pending_inner);
        let around = std::mem::take(&mut cursor.pending_around);
        let yank = cursor.pending_yank;
        if (inner || around) && matches!(key.code, KeyCode::Char('w' | 'W')) {
            cursor.word_object(&self.cursor_rows(), key.code == KeyCode::Char('W'), around);
            if yank {
                self.copy_cursor_selection(&mut cursor);
            }
            cursor.pending_yank = false;
            cursor.count = 0;
            self.reveal_code_cursor(&cursor);
            self.put_cursor(cursor);
            return true;
        }
        let pending_g = std::mem::take(&mut cursor.pending_g);
        if pending_g && key.code == KeyCode::Char('b') {
            cursor.count = 0;
            self.put_cursor(cursor);
            self.history.enabled = !self.history.enabled;
            return true;
        }
        if pending_g && matches!(key.code, KeyCode::Char('d' | 'r')) {
            let position = cursor.position;
            cursor.count = 0;
            self.put_cursor(cursor);
            if let Some(p) = position {
                self.click_symbol(p.row, p.byte, key.code == KeyCode::Char('r'));
            }
            return true;
        }
        if std::mem::take(&mut cursor.pending_z) && matches!(key.code, KeyCode::Char('e' | 'E')) {
            self.put_cursor(cursor);
            self.expand_visible_gap(key.code == KeyCode::Char('E'));
            return true;
        }
        if let KeyCode::Char(c @ '0'..='9') = key.code {
            if c != '0' || cursor.count > 0 {
                cursor.count = cursor
                    .count
                    .saturating_mul(10)
                    .saturating_add(c.to_digit(10).unwrap() as usize)
                    .min(1_000_000);
                self.put_cursor(cursor);
                return true;
            }
        }
        let count = std::mem::take(&mut cursor.count);
        cursor.pending_yank = false;
        let handled = match key.code {
            KeyCode::Char('i' | 'a') if cursor.anchor.is_some() || yank => {
                cursor.pending_inner = key.code == KeyCode::Char('i');
                cursor.pending_around = !cursor.pending_inner;
                cursor.pending_yank = yank;
                true
            }
            KeyCode::Char('y') => {
                if yank {
                    cursor.anchor = cursor.position;
                    cursor.linewise = true;
                    for _ in 1..count.max(1).min(self.cursor_rows().len()) {
                        let previous = cursor.position;
                        cursor.move_by('j', &self.cursor_rows());
                        if cursor.position == previous {
                            break;
                        }
                    }
                }
                if cursor.anchor.is_some() {
                    self.copy_cursor_selection(&mut cursor);
                } else {
                    cursor.pending_yank = true;
                    cursor.count = count;
                }
                true
            }
            KeyCode::Char('g') if !pending_g => {
                cursor.pending_g = true;
                cursor.count = count;
                true
            }
            KeyCode::Char('g') | KeyCode::Char('G') => {
                cursor.goto_line(
                    &self.cursor_rows(),
                    (count > 0).then_some(count),
                    key.code == KeyCode::Char('G'),
                );
                true
            }
            KeyCode::Char('z') if matches!(self.overlay, Overlay::None) => {
                cursor.pending_z = true;
                true
            }
            KeyCode::Char('C') if matches!(self.overlay, Overlay::None) => {
                self.put_cursor(cursor);
                self.main_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
                return true;
            }
            KeyCode::Esc if cursor.anchor.is_some() => {
                cursor.anchor = None;
                cursor.linewise = false;
                true
            }
            KeyCode::Char('v' | 'V') => {
                let linewise = key.code == KeyCode::Char('V');
                if cursor.anchor.is_some() && cursor.linewise == linewise {
                    cursor.anchor = None;
                } else {
                    cursor.anchor = cursor.anchor.or(cursor.position);
                }
                cursor.linewise = linewise;
                true
            }
            KeyCode::Char(c @ ('h' | 'j' | 'k' | 'l' | 'w' | 'b' | 'e' | 'W' | 'B' | 'E')) => {
                for _ in 0..count.max(1) {
                    let previous = cursor.position;
                    if matches!(c, 'h' | 'j' | 'k' | 'l') {
                        cursor.move_by(c, &self.cursor_rows());
                    } else {
                        cursor.word_motion(&self.cursor_rows(), c);
                    }
                    if cursor.position == previous {
                        break;
                    }
                }
                true
            }
            KeyCode::Char(c @ ('0' | '^' | '$')) => {
                cursor.line_edge(&self.cursor_rows(), c);
                true
            }
            KeyCode::Char(c @ ('f' | 'F' | 't' | 'T')) => {
                cursor.pending_find = Some(c);
                cursor.count = count;
                true
            }
            KeyCode::Char(';' | ',') => {
                if let Some((mut kind, target)) = cursor.last_find {
                    if key.code == KeyCode::Char(',') {
                        kind = match kind {
                            'f' => 'F',
                            'F' => 'f',
                            't' => 'T',
                            _ => 't',
                        };
                    }
                    // Repeating t/T must skip the adjacent character just found.
                    let previous = cursor.position;
                    if matches!(kind, 't' | 'T') {
                        cursor.move_by(if kind == 't' { 'l' } else { 'h' }, &self.cursor_rows());
                    }
                    let before = cursor.position;
                    cursor.find_char(&self.cursor_rows(), kind, target, count.max(1));
                    if cursor.position == before {
                        cursor.position = previous;
                    }
                }
                true
            }
            KeyCode::Char(c @ ('H' | 'M' | 'L')) => {
                let (offset, _) = self.cursor_offsets();
                let end = (offset + self.cursor_height()).min(self.cursor_rows().len());
                let visible: Vec<_> = (offset..end)
                    .filter(|&r| self.cursor_rows().valid(r))
                    .collect();
                let index = match c {
                    'H' => count.max(1) - 1,
                    'L' => visible.len().saturating_sub(count.max(1)),
                    _ => visible.len() / 2,
                };
                if let Some(&row) = visible.get(index.min(visible.len().saturating_sub(1))) {
                    cursor.position = Some(Position { row, byte: 0 });
                    cursor.line_edge(&self.cursor_rows(), '^');
                }
                true
            }
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End => {
                cursor.anchor = None;
                false
            }
            _ => false,
        };
        if handled {
            self.reveal_code_cursor(&cursor);
        }
        self.put_cursor(cursor);
        handled
    }

    pub(super) fn command_copy_selection(&mut self, copy: impl FnOnce(&mut Self, &mut CodeCursor)) {
        if self.focus != Focus::Diff || !matches!(self.overlay, Overlay::None | Overlay::Source(_))
        {
            return;
        }
        let mut cursor = self.take_cursor();
        if cursor.anchor.is_some() {
            // Share Visual-y's clipboard and selection semantics, including failed copies.
            copy(self, &mut cursor);
            self.reveal_code_cursor(&cursor);
        }
        self.put_cursor(cursor);
    }

    pub(super) fn copy_cursor_selection(&mut self, cursor: &mut CodeCursor) {
        let mut clipboard = self.clipboard.take();
        let result = cursor.yank(&self.cursor_rows(), |text| {
            if clipboard.is_none() {
                clipboard = Some(arboard::Clipboard::new()?);
            }
            clipboard.as_mut().unwrap().set_text(text)?;
            Ok(())
        });
        self.clipboard = clipboard;
        if let Err(error) = result {
            self.error = Some(format!("クリップボードにコピーできません: {error}"));
        }
    }

    fn reveal_code_cursor(&mut self, cursor: &CodeCursor) {
        let (mut offset, mut horizontal) = self.cursor_offsets();
        let height = self.cursor_height();
        if let Some(p) = cursor.position {
            if p.row < offset {
                offset = p.row;
            }
            if p.row >= offset.saturating_add(height) {
                offset = p.row + 1 - height;
            }
            let prefix: u16 = if matches!(self.overlay, Overlay::Source(_)) {
                8
            } else {
                0
            };
            let rows = self.cursor_rows();
            let text = rows.text(p.row);
            let column = prefix
                .saturating_add(Line::raw(&text[..p.byte]).width().min(u16::MAX as usize) as u16);
            let glyph_width = Line::raw(&text[p.byte..])
                .styled_graphemes(Style::default())
                .next()
                .map_or(1, |g| g.symbol.cell_width().max(1));
            let width = self
                .code_viewport
                .borrow()
                .as_ref()
                .map_or(1, |v| v.width())
                .max(1);
            if column < horizontal {
                horizontal = column;
            }
            if column.saturating_add(glyph_width) > horizontal.saturating_add(width) {
                horizontal = column.saturating_add(glyph_width).saturating_sub(width);
            }
        }
        match &mut self.overlay {
            Overlay::Source(s) => {
                s.offset = offset;
                s.horizontal = horizontal;
            }
            _ => {
                self.diff_offset = offset;
                self.horizontal = horizontal;
            }
        }
    }

    pub(super) fn draw_code_cursor(&self, frame: &mut Frame) {
        if !self.code_cursor_visible() {
            return;
        }
        let cursor = match &self.overlay {
            Overlay::None => &self.cursor,
            Overlay::Source(source) => &source.cursor,
            _ => return,
        };
        if let Some(view) = self.code_viewport.borrow().as_ref() {
            let rows = self.cursor_rows();
            view.draw_cursor(frame, cursor, self.focus == Focus::Diff, |row| {
                rows.valid(row)
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn source_app(text: &str) -> (super::super::tests::Fixture, App) {
        let (fixture, mut app) = super::super::tests::app();
        app.focus = Focus::Diff;
        app.overlay = Overlay::Source(Source::new("head".into(), "test.rs".into(), 1, text.into()));
        (fixture, app)
    }

    fn chars(app: &mut App, keys: &str) {
        for c in keys.chars() {
            super::super::tests::press(app, KeyCode::Char(c));
        }
    }

    fn source_position(app: &App) -> Position {
        match &app.overlay {
            Overlay::Source(s) => s.cursor.position.unwrap(),
            _ => panic!("source closed"),
        }
    }

    #[test]
    fn command_c_dispatch_copies_visual_selection_in_diff_and_source() {
        for source in [false, true] {
            let (_fixture, mut app) = super::super::tests::app();
            app.focus = Focus::Diff;
            if source {
                app.overlay = Overlay::Source(Source::new(
                    "head".into(),
                    "test.rs".into(),
                    1,
                    "日本_name".into(),
                ));
            }
            chars(&mut app, "viw");
            let before = app.take_cursor();
            let expected = before.selected_text(&app.cursor_rows()).unwrap();
            let start = before.position.unwrap().min(before.anchor.unwrap());
            app.put_cursor(before);
            let mut copied = None;
            assert!(!app.key_with_copy_selection(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER),
                |app, cursor| {
                    cursor
                        .yank(&app.cursor_rows(), |text| {
                            copied = Some(text);
                            Ok(())
                        })
                        .unwrap();
                },
            ));
            assert_eq!(copied.as_deref(), Some(expected.as_str()));
            let after = app.take_cursor();
            assert!(after.anchor.is_none());
            assert_eq!(after.position, Some(start));
            app.put_cursor(after);
            assert!(app.focus == Focus::Diff);
            assert!(app.clipboard.is_none());
        }
    }

    #[test]
    fn command_c_without_active_code_selection_preserves_state() {
        let (_fixture, mut app) = source_app("alpha beta");
        let command_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER);
        assert!(!app.key_with_copy_selection(command_c, |_, _| panic!("no selection")));
        chars(&mut app, "viw");
        let position = source_position(&app);
        app.focus = Focus::Files;
        assert!(!app.key_with_copy_selection(command_c, |_, _| panic!("inactive pane")));
        assert!(app.focus == Focus::Files);
        assert_eq!(source_position(&app), position);
        if let Overlay::Source(source) = &app.overlay {
            assert!(source.cursor.anchor.is_some());
        }
        app.overlay = Overlay::Search("needle".into());
        assert!(!app.key_with_copy_selection(command_c, |_, _| panic!("search input")));
        assert!(matches!(&app.overlay, Overlay::Search(input) if input == "needle"));
        assert!(app.key_with_copy_selection(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            |_, _| panic!("Ctrl+C exits"),
        ));
    }

    #[test]
    fn word_and_word_motions_cross_headers_empty_rows_and_unicode() {
        let lines = vec![
            Some("alpha-> beta".into()),
            None,
            Some("".into()),
            Some("  a\u{301}日本_name x".into()),
        ];
        let rows = Rows::Test(&lines);
        let mut cursor = CodeCursor {
            position: Some(Position { row: 0, byte: 0 }),
            ..Default::default()
        };
        for (key, row, byte) in [
            ('w', 0, 5),
            ('w', 0, 8),
            ('w', 2, 0),
            ('w', 3, 2),
            ('e', 3, 15),
            ('b', 3, 2),
            ('B', 0, 8),
        ] {
            // Empty lines are word stops in both directions.
            if key == 'B' {
                cursor.word_motion(&rows, 'B');
                assert_eq!(cursor.position, Some(Position { row: 2, byte: 0 }));
            }
            cursor.word_motion(&rows, key);
            assert_eq!(cursor.position, Some(Position { row, byte }), "{key}");
        }
        cursor.position = Some(Position { row: 0, byte: 0 });
        cursor.word_motion(&rows, 'E');
        assert_eq!(cursor.position.unwrap().byte, 6);
        cursor.word_motion(&rows, 'W');
        assert_eq!(cursor.position.unwrap().byte, 8);
    }

    #[test]
    fn counts_edges_pages_and_visual_lines_work_through_real_key_dispatch() {
        let text = (1..=30)
            .map(|n| format!("  line_{n} tail"))
            .collect::<Vec<_>>()
            .join("\n");
        let (_fixture, mut app) = source_app(&text);
        app.page_height = 8;
        chars(&mut app, "3j2l");
        assert_eq!(source_position(&app), Position { row: 3, byte: 2 });
        chars(&mut app, "0");
        assert_eq!(source_position(&app).byte, 0);
        chars(&mut app, "^");
        assert_eq!(source_position(&app).byte, 2);
        chars(&mut app, "$");
        assert_eq!(source_position(&app).byte, 12);
        chars(&mut app, "12gg");
        assert_eq!(source_position(&app), Position { row: 11, byte: 2 });
        chars(&mut app, "3G");
        assert_eq!(source_position(&app).row, 2);
        chars(&mut app, "ggV2j");
        if let Overlay::Source(source) = &app.overlay {
            assert_eq!(
                source.cursor.selected_text(&app.cursor_rows()).as_deref(),
                Some("  line_1 tail\n  line_2 tail\n  line_3 tail\n")
            );
            assert!(source.cursor.selected(1, 0));
        }
        super::super::tests::press(&mut app, KeyCode::Esc);
        assert!(matches!(&app.overlay, Overlay::Source(source) if source.cursor.anchor.is_none()));
        chars(&mut app, "gg");
        app.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(source_position(&app).row, 4);
        app.key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert_eq!(source_position(&app).row, 12);
        app.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(source_position(&app).row, 4);
        app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(source_position(&app).row, 0);
        chars(&mut app, "G");
        assert_eq!(source_position(&app).row, 29);
        chars(&mut app, "H");
        assert_eq!(source_position(&app).row, 22);
        chars(&mut app, "M");
        assert_eq!(source_position(&app).row, 26);
        chars(&mut app, "L");
        assert_eq!(source_position(&app).row, 29);
    }

    #[test]
    fn control_j_and_k_move_the_cursor_and_view_twenty_lines_in_diff_and_source() {
        let text = (1..=45)
            .map(|n| format!("line_{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (_fixture, mut app) = source_app(&text);
        app.page_height = 8;
        app.comparison.files[0].lines = (1..=45)
            .map(|n| crate::review::git::DiffLine {
                kind: LineKind::Context,
                text: format!("line_{n}"),
                old_line: Some(n),
                new_line: Some(n),
            })
            .collect();

        for source in [true, false] {
            if !source {
                app.overlay = Overlay::None;
            }
            for (key, row, offset) in [('j', 20, 20), ('j', 40, 37), ('j', 44, 37), ('k', 24, 17)] {
                assert!(!app.key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL)));
                let position = if source {
                    source_position(&app)
                } else {
                    app.cursor.position.unwrap()
                };
                assert_eq!(position.row, row);
                assert_eq!(app.cursor_offsets().0, offset);
            }
        }
    }

    #[test]
    fn character_find_repeat_counts_and_text_objects_keep_grapheme_boundaries() {
        let (_fixture, mut app) = source_app("  foo-> bar bar baz");
        chars(&mut app, "2fb");
        assert_eq!(source_position(&app).byte, 12);
        chars(&mut app, ";");
        assert_eq!(source_position(&app).byte, 16);
        chars(&mut app, ",");
        assert_eq!(source_position(&app).byte, 12);
        chars(&mut app, "Fb");
        assert_eq!(source_position(&app).byte, 8);
        chars(&mut app, "0tb");
        assert_eq!(source_position(&app).byte, 7);
        chars(&mut app, ";");
        assert_eq!(source_position(&app).byte, 11);
        chars(&mut app, ",");
        assert_eq!(source_position(&app).byte, 9);
        chars(&mut app, "0viW");
        assert!(
            matches!(&app.overlay, Overlay::Source(source) if source.cursor.selected_text(&app.cursor_rows()).as_deref() == Some("  "))
        );
        chars(&mut app, "v^viW");
        assert!(
            matches!(&app.overlay, Overlay::Source(source) if source.cursor.selected_text(&app.cursor_rows()).as_deref() == Some("foo->"))
        );
        chars(&mut app, "aW");
        assert!(
            matches!(&app.overlay, Overlay::Source(source) if source.cursor.selected_text(&app.cursor_rows()).as_deref() == Some("foo-> "))
        );
    }

    #[test]
    fn numbered_jumps_use_source_numbers_and_pending_keys_cancel_on_escape_or_tab() {
        use crate::review::git::DiffLine;
        let lines = vec![
            DiffLine {
                kind: LineKind::Header,
                text: "@@".into(),
                old_line: None,
                new_line: None,
            },
            DiffLine {
                kind: LineKind::Added,
                text: "one".into(),
                old_line: None,
                new_line: Some(40),
            },
            DiffLine {
                kind: LineKind::Removed,
                text: "two".into(),
                old_line: Some(90),
                new_line: None,
            },
        ];
        let mut cursor = CodeCursor::default();
        cursor.goto_line(&Rows::Diff(&lines), Some(89), false);
        assert_eq!(cursor.position.unwrap().row, 2);
        let (_fixture, mut app) = source_app("one two three");
        chars(&mut app, "99f");
        super::super::tests::press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));
        let (_fixture, mut app) = source_app("one two three");
        chars(&mut app, "99g");
        super::super::tests::press(&mut app, KeyCode::Tab);
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.focus == Focus::Files);
        assert_eq!(app.cursor.count, 0);
        assert!(!app.cursor.pending_g);
    }

    #[test]
    fn mouse_controls_and_control_p_do_not_become_vim_motions() {
        let (_fixture, mut app) = super::super::tests::app();
        app.focus = Focus::Diff;
        app.activate_mouse_control(MouseAction::Compare);
        assert!(matches!(app.overlay, Overlay::Branches(_)));
        app.overlay = Overlay::None;
        app.focus = Focus::Diff;
        app.activate_mouse_control(MouseAction::AllFiles);
        assert!(matches!(app.overlay, Overlay::AllFiles(_)));
        let (_fixture, mut app) = source_app("one two");
        app.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(matches!(app.overlay, Overlay::AllFiles(_)));
    }

    #[test]
    fn search_identifier_uses_selected_text_and_rejects_linewise_or_invalid_ranges() {
        let lines = vec!["  名前  ".to_owned(), "other".to_owned()];
        let mut cursor = CodeCursor {
            anchor: Some(Position { row: 0, byte: 0 }),
            position: Some(Position { row: 0, byte: 9 }),
            ..Default::default()
        };
        assert_eq!(
            cursor.selected_source_identifier(&lines).as_deref(),
            Some("名前")
        );
        cursor.linewise = true;
        assert!(cursor.selected_source_identifier(&lines).is_none());
        cursor.linewise = false;
        cursor.position = Some(Position { row: 1, byte: 0 });
        assert!(cursor.selected_source_identifier(&lines).is_none());
        cursor.anchor = Some(Position { row: 8, byte: 0 });
        cursor.position = Some(Position { row: 8, byte: 1 });
        assert!(cursor.selected_source_identifier(&lines).is_none());
    }

    #[test]
    fn yank_is_inclusive_backwards_and_skips_headers() {
        let rows = vec![
            Some("  a\u{301}日本!".into()),
            None,
            Some(String::new()),
            Some("last 😀!".into()),
        ];
        let rows = Rows::Test(&rows);
        let mut cursor = CodeCursor {
            anchor: Some(Position { row: 3, byte: 5 }),
            position: Some(Position { row: 0, byte: 2 }),
            ..Default::default()
        };
        let mut copied = None;
        assert!(cursor
            .yank(&rows, |text| {
                copied = Some(text);
                Ok(())
            })
            .unwrap());
        assert_eq!(copied.as_deref(), Some("a\u{301}日本!\n\nlast 😀"));
        assert_eq!(cursor.position, Some(Position { row: 0, byte: 2 }));
        assert!(cursor.anchor.is_none());
        let p = Position { row: 0, byte: 2 };
        cursor.position = Some(p);
        cursor.anchor = Some(p);
        assert_eq!(cursor.selected_text(&rows).as_deref(), Some("a\u{301}"));
    }

    #[test]
    fn yank_leaves_empty_selection_and_failed_copy_intact() {
        let rows = vec![Some(String::new()), Some("word".into())];
        let rows = Rows::Test(&rows);
        let mut cursor = CodeCursor::default();
        assert!(!cursor
            .yank(&rows, |_| panic!("no visual selection"))
            .unwrap());
        let empty = Position { row: 0, byte: 0 };
        cursor.position = Some(empty);
        cursor.anchor = Some(empty);
        assert!(!cursor.yank(&rows, |_| panic!("empty selection")).unwrap());
        assert_eq!(cursor.anchor, Some(empty));
        let p = Position { row: 1, byte: 0 };
        cursor.position = Some(p);
        cursor.anchor = Some(p);
        let error = cursor.yank(&rows, |_| anyhow::bail!("clipboard unavailable"));
        assert!(error.is_err());
        assert_eq!(cursor.position, Some(p));
        assert_eq!(cursor.anchor, Some(p));
    }

    #[test]
    fn inner_word_selects_unicode_words_punctuation_and_whitespace() {
        let rows = vec!["a\u{301}日本_name->  x".into()];
        let rows = Rows::Source(&rows);
        for (byte, expected) in [(3, "a\u{301}日本_name"), (14, "->"), (16, "  ")] {
            let mut cursor = CodeCursor {
                position: Some(Position { row: 0, byte }),
                ..Default::default()
            };
            cursor.inner_word(&rows);
            assert_eq!(cursor.selected_text(&rows).as_deref(), Some(expected));
        }
        let empty = vec![String::new()];
        let mut cursor = CodeCursor {
            position: Some(Position { row: 0, byte: 0 }),
            ..Default::default()
        };
        cursor.inner_word(&Rows::Source(&empty));
        assert!(cursor.anchor.is_none());
    }

    #[test]
    fn diff_yank_uses_clean_code_without_line_numbers_or_hunk_headers() {
        use crate::review::git::DiffLine;
        let lines = vec![
            DiffLine {
                kind: LineKind::Added,
                text: "a\tb".into(),
                old_line: None,
                new_line: Some(20),
            },
            DiffLine {
                kind: LineKind::Header,
                text: "@@ gap @@".into(),
                old_line: None,
                new_line: None,
            },
            DiffLine {
                kind: LineKind::Removed,
                text: "日本\u{1b}!".into(),
                old_line: Some(40),
                new_line: None,
            },
        ];
        let cursor = CodeCursor {
            anchor: Some(Position { row: 0, byte: 0 }),
            position: Some(Position { row: 2, byte: 7 }),
            ..Default::default()
        };
        assert_eq!(
            cursor.selected_text(&Rows::Diff(&lines)).as_deref(),
            Some("a b\n日本 !")
        );
    }

    #[test]
    fn viw_keys_work_in_diff_and_source_without_touching_clipboard() {
        let (_fixture, mut app) = super::super::tests::app();
        app.focus = Focus::Diff;
        app.cursor.position = Some(Position { row: 1, byte: 7 });
        for key in ['v', 'i', 'w'] {
            assert!(app.code_cursor_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)));
        }
        assert!(app.cursor.anchor.is_some());
        assert!(!app
            .cursor
            .selected_text(&app.cursor_rows())
            .unwrap()
            .is_empty());
        app.overlay = Overlay::Source(Source::new(
            "head".into(),
            "test.txt".into(),
            1,
            "let 日本_name = 1".into(),
        ));
        if let Overlay::Source(source) = &mut app.overlay {
            source.cursor.position = Some(Position { row: 0, byte: 7 });
        }
        for key in ['v', 'i', 'w'] {
            assert!(app.code_cursor_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)));
        }
        if let Overlay::Source(source) = &app.overlay {
            assert_eq!(
                source.cursor.selected_text(&app.cursor_rows()).as_deref(),
                Some("日本_name")
            );
        }
        assert!(app.clipboard.is_none());
        // The next non-object key cancels the pending i and remains usable.
        app.code_cursor_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        app.code_cursor_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.code_cursor_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        assert!(matches!(&app.overlay, Overlay::Source(source) if source.cursor.anchor.is_none()));
    }

    #[test]
    fn graphemes_and_visual_ranges_skip_headers_and_clamp_short_lines() {
        let rows = vec![
            None,
            Some("a\u{301}日本".into()),
            None,
            Some("x".into()),
            Some(String::new()),
        ];
        let rows = Rows::Test(&rows);
        let mut cursor = CodeCursor::default();
        cursor.normalize(&rows, 0, 10);
        assert_eq!(cursor.position, Some(Position { row: 1, byte: 0 }));
        cursor.move_by('l', &rows);
        assert_eq!(cursor.position.unwrap().byte, "a\u{301}".len());
        cursor.anchor = cursor.position;
        cursor.move_by('j', &rows);
        assert_eq!(cursor.position, Some(Position { row: 3, byte: 0 }));
        assert!(!cursor.selected(1, 0));
        assert!(cursor.selected(1, 3));
        assert!(cursor.selected(3, 0));
        cursor.move_by('j', &rows);
        cursor.move_by('l', &rows);
        assert_eq!(cursor.position, Some(Position { row: 4, byte: 0 }));
        cursor.normalize(&rows, 1, 1);
        assert!(cursor.anchor.is_none());
    }

    #[test]
    fn file_selection_shows_code_caret_without_taking_keyboard_focus() {
        let (_fixture, mut app) = super::super::tests::app();
        app.focus = Focus::Files;
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_eq!(app.cursor.position, Some(Position { row: 0, byte: 0 }));
        assert!(app.mouse_panes[1].contains(terminal.get_cursor_position().unwrap()));
        assert!(app.focus == Focus::Files);

        // Previewing an existing Visual cursor keeps its byte but hides selection.
        app.cursor.position = Some(Position { row: 0, byte: 2 });
        app.cursor.anchor = Some(Position { row: 0, byte: 0 });
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_eq!(app.cursor.position, Some(Position { row: 0, byte: 2 }));
        let area = app.mouse_panes[1];
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                assert!(!terminal.backend().buffer()[(x, y)]
                    .modifier
                    .contains(Modifier::REVERSED));
            }
        }
        assert!(!app.code_cursor_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
        super::super::tests::press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.selected, 1);
        assert!(app.focus == Focus::Files);
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_eq!(app.cursor.position, Some(Position { row: 0, byte: 0 }));
        assert!(app.mouse_panes[1].contains(terminal.get_cursor_position().unwrap()));

        app.folder = Some("folder".into());
        assert!(!app.code_cursor_visible());
        app.folder = None;
        app.overlay = Overlay::Help(0);
        assert!(!app.code_cursor_visible());
        app.overlay = Overlay::Source(Source::new(
            "head".into(),
            "test.rs".into(),
            1,
            "text".into(),
        ));
        assert!(!app.code_cursor_visible());
        app.focus = Focus::Diff;
        assert!(app.code_cursor_visible());
    }

    #[test]
    fn visual_escape_focus_and_modal_keys_are_separate() {
        let (_fixture, mut app) = super::super::tests::app();
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        app.focus = Focus::Diff;
        terminal.draw(|f| app.draw(f)).unwrap();
        let start = app.cursor.position.unwrap();
        super::super::tests::press(&mut app, KeyCode::Char('v'));
        super::super::tests::press(&mut app, KeyCode::Char('l'));
        assert_eq!(app.cursor.anchor, Some(start));
        assert!(app.cursor.position.unwrap().byte > start.byte);
        super::super::tests::press(&mut app, KeyCode::Esc);
        assert!(app.cursor.anchor.is_none());
        assert!(app.focus == Focus::Diff);
        super::super::tests::press(&mut app, KeyCode::Esc);
        assert!(app.focus == Focus::Files);
        let position = app.cursor.position;
        super::super::tests::press(&mut app, KeyCode::Char('l'));
        assert_eq!(app.cursor.position, position);
        app.focus = Focus::Diff;
        app.overlay = Overlay::Search(String::new());
        super::super::tests::press(&mut app, KeyCode::Char('v'));
        assert!(matches!(&app.overlay, Overlay::Search(s) if s == "v"));
        assert!(app.cursor.anchor.is_none());
    }

    #[test]
    fn cjk_cursor_uses_two_cells_and_reentry_preserves_horizontal_scroll() {
        let (_fixture, mut app) = super::super::tests::app();
        app.focus = Focus::Diff;
        app.overlay = Overlay::Source(Source::new(
            "head".into(),
            "test.rs".into(),
            1,
            "日本a\u{301}xyz".into(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let start = terminal.get_cursor_position().unwrap();
        super::super::tests::press(&mut app, KeyCode::Char('l'));
        terminal.draw(|f| app.draw(f)).unwrap();
        let next = terminal.get_cursor_position().unwrap();
        assert_eq!(next.x, start.x + 2);
        assert_eq!(next.y, start.y);
        super::super::tests::press(&mut app, KeyCode::Char('l'));
        terminal.draw(|f| app.draw(f)).unwrap();
        let combining = terminal.get_cursor_position().unwrap();
        assert_eq!(combining.x, start.x + 4);
        super::super::tests::press(&mut app, KeyCode::Char('l'));
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_eq!(terminal.get_cursor_position().unwrap().x, combining.x + 1);
        if let Overlay::Source(s) = &mut app.overlay {
            s.horizontal = 10;
            s.cursor = CodeCursor::default();
        }
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(
            matches!(&app.overlay, Overlay::Source(s) if s.horizontal == 10 && s.cursor.position.unwrap().byte == 3)
        );
    }

    #[test]
    fn source_cursor_scrolls_and_visual_keeps_syntax_foreground() {
        let (_fixture, mut app) = super::super::tests::app();
        app.focus = Focus::Diff;
        app.overlay = Overlay::Source(Source::new(
            "head".into(),
            "test.rs".into(),
            1,
            "日本a\u{301}x\nline2\nline3\nline4\nline5\nline6\nline7\nline8".into(),
        ));
        if let Overlay::Source(s) = &mut app.overlay {
            s.colors = Some(vec![vec![Span::styled(
                "日本a\u{301}x",
                Style::default().fg(Color::Magenta),
            )]]);
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        super::super::tests::press(&mut app, KeyCode::Char('v'));
        super::super::tests::press(&mut app, KeyCode::Char('l'));
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|c| c.symbol() == "日"
                && c.fg == Color::Magenta
                && c.modifier.contains(Modifier::REVERSED)));
        for _ in 0..7 {
            super::super::tests::press(&mut app, KeyCode::Char('j'));
            terminal.draw(|f| app.draw(f)).unwrap();
        }
        assert!(
            matches!(&app.overlay, Overlay::Source(s) if s.offset > 0 && s.cursor.position.unwrap().row == 7)
        );
        super::super::tests::press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::Source(_)));
    }
}
