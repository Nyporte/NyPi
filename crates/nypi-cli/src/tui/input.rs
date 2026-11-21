//! The message editor: a char buffer with a cursor, newlines, and history of sent messages.
//! Widths come from unicode-width, so CJK text wraps and places the cursor correctly.

use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub struct Input {
    chars: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    /// Position while browsing history, and the draft it replaced.
    browsing: Option<(usize, String)>,
}

impl Input {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
        self.browsing = None;
    }

    pub fn insert_str(&mut self, text: &str) {
        // Pasted CRLF arrives as two characters; one newline is what the user means.
        for c in text.replace("\r\n", "\n").replace('\r', "\n").chars() {
            self.insert(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars.len());
    }

    /// Start of the current line.
    pub fn home(&mut self) {
        while self.cursor > 0 && self.chars[self.cursor - 1] != '\n' {
            self.cursor -= 1;
        }
    }

    /// End of the current line.
    pub fn end(&mut self) {
        while self.cursor < self.chars.len() && self.chars[self.cursor] != '\n' {
            self.cursor += 1;
        }
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
        self.browsing = None;
    }

    /// Delete the word before the cursor (Ctrl-W).
    pub fn delete_word(&mut self) {
        while self.cursor > 0 && self.chars[self.cursor - 1].is_whitespace() {
            self.backspace();
        }
        while self.cursor > 0 && !self.chars[self.cursor - 1].is_whitespace() {
            self.backspace();
        }
    }

    /// Take the text for sending; non-empty messages go into history.
    pub fn take(&mut self) -> String {
        let text = self.text();
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        self.clear();
        text
    }

    pub fn history_prev(&mut self) {
        let index = match &self.browsing {
            Some((index, _)) if *index == 0 => return,
            Some((index, _)) => index - 1,
            None if self.history.is_empty() => return,
            None => {
                self.browsing = Some((self.history.len(), self.text()));
                self.history.len() - 1
            }
        };
        self.load(index);
    }

    pub fn history_next(&mut self) {
        let Some((index, draft)) = self.browsing.clone() else { return };
        if index + 1 >= self.history.len() {
            self.chars = draft.chars().collect();
            self.cursor = self.chars.len();
            self.browsing = None;
        } else {
            self.load(index + 1);
        }
    }

    fn load(&mut self, index: usize) {
        let draft = self.browsing.take().map(|(_, d)| d).unwrap_or_default();
        self.chars = self.history[index].chars().collect();
        self.cursor = self.chars.len();
        self.browsing = Some((index, draft));
    }

    /// The text wrapped to `width` columns, and the cursor's (column, row) in it.
    pub fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(1);
        let mut rows = vec![String::new()];
        let (mut column, mut cursor) = (0usize, (0usize, 0usize));
        for (index, c) in self.chars.iter().enumerate() {
            if index == self.cursor {
                cursor = (column, rows.len() - 1);
            }
            if *c == '\n' {
                rows.push(String::new());
                column = 0;
                continue;
            }
            let w = c.width().unwrap_or(0);
            if column + w > width {
                rows.push(String::new());
                column = 0;
                if index == self.cursor {
                    cursor = (0, rows.len() - 1);
                }
            }
            rows.last_mut().expect("at least one row").push(*c);
            column += w;
        }
        if self.cursor == self.chars.len() {
            cursor = if column >= width { (0, rows.len()) } else { (column, rows.len() - 1) };
            if column >= width {
                rows.push(String::new());
            }
        }
        (rows, cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_and_history() {
        let mut input = Input::default();
        input.insert_str("hello world");
        input.delete_word();
        assert_eq!(input.text(), "hello ");
        input.home();
        input.insert('>');
        assert_eq!(input.text(), ">hello ");
        assert_eq!(input.take(), ">hello ");
        input.insert_str("draft");
        input.history_prev();
        assert_eq!(input.text(), ">hello ");
        input.history_next();
        assert_eq!(input.text(), "draft");
    }

    #[test]
    fn layout_wraps_wide_characters_and_places_the_cursor() {
        let mut input = Input::default();
        input.insert_str("你好世界ab\ncd");
        let (rows, cursor) = input.layout(6);
        assert_eq!(rows, ["你好世", "界ab", "cd"]);
        assert_eq!(cursor, (2, 2));
        input.home();
        assert_eq!(input.layout(6).1, (0, 2));
        input.insert_str("\r\n");
        assert_eq!(input.text(), "你好世界ab\n\ncd");
    }
}
