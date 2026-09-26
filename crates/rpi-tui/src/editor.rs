//! 多行编辑器(pi Editor 的 Rust 对应物)。
//!
//! 能力:多行缓冲(Shift+Enter/Ctrl+J 换行、粘贴保留换行)、undo 栈、
//! kill-ring、词导航、输入历史(单行状态时 ↑/↓)。缓冲区按行存 `Vec<char>`
//! 避免多字节字符的字节索引问题。所有操作经由 `handle_key`,Enter 不在
//! 编辑器内消费(提交语义由上层决定)。

use crate::key::Key;
use crate::width::{display_width, wrap_to_width};

const MAX_UNDO: usize = 100;
const MAX_KILL_RING: usize = 10;

/// 多行编辑器状态。
#[derive(Default)]
pub struct Editor {
    lines: Vec<Vec<char>>,
    /// (行下标, 行内字符下标)
    cursor: (usize, usize),
    undo_stack: Vec<EditorSnapshot>,
    kill_ring: Vec<String>,
    history: Vec<String>,
    history_index: Option<usize>,
    /// 历史浏览前的草稿
    draft: Option<String>,
}

#[derive(Clone)]
struct EditorSnapshot {
    lines: Vec<Vec<char>>,
    cursor: (usize, usize),
}

impl Editor {
    pub fn new() -> Self {
        Editor {
            lines: vec![Vec::new()],
            ..Default::default()
        }
    }

    /// 全文(行间 `\n` 连接)。
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|line| line.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// 逻辑行数。
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// 光标(行, 列=字符下标)。
    pub fn cursor_pos(&self) -> (usize, usize) {
        self.cursor
    }

    pub fn clear(&mut self) {
        self.push_undo();
        self.lines = vec![Vec::new()];
        self.cursor = (0, 0);
        self.history_index = None;
        self.draft = None;
    }

    /// 设置整段文本(可含换行,替换当前内容,可撤销)。
    pub fn set_text(&mut self, text: &str) {
        self.push_undo();
        self.lines = text
            .split('\n')
            .map(|line| line.chars().collect())
            .collect();
        if self.lines.is_empty() {
            self.lines = vec![Vec::new()];
        }
        let last = self.lines.len() - 1;
        self.cursor = (last, self.lines[last].len());
    }

    /// 按键处理:返回 true 表示已消费。Enter 不消费(上层提交)。
    pub fn handle_key(&mut self, key: &Key) -> bool {
        match key {
            Key::Char(c) => {
                self.push_undo();
                let (line, col) = self.cursor;
                self.lines[line].insert(col, *c);
                self.cursor.1 += 1;
                true
            }
            Key::Paste(text) => {
                self.push_undo();
                self.insert_text(text);
                true
            }
            Key::ShiftEnter | Key::Ctrl('j') | Key::AltEnter => {
                self.push_undo();
                self.insert_newline();
                true
            }
            Key::Backspace => {
                let (line, col) = self.cursor;
                if col > 0 {
                    self.push_undo();
                    self.lines[line].remove(col - 1);
                    self.cursor.1 -= 1;
                } else if line > 0 {
                    // 行首回删:与上一行合并
                    self.push_undo();
                    let cur = self.lines.remove(line);
                    let prev_len = self.lines[line - 1].len();
                    self.lines[line - 1].extend(cur);
                    self.cursor = (line - 1, prev_len);
                }
                true
            }
            Key::Delete => {
                let (line, col) = self.cursor;
                if col < self.lines[line].len() {
                    self.push_undo();
                    self.lines[line].remove(col);
                } else if line + 1 < self.lines.len() {
                    self.push_undo();
                    let next = self.lines.remove(line + 1);
                    self.lines[line].extend(next);
                }
                true
            }
            Key::Left => {
                let (line, col) = self.cursor;
                if col > 0 {
                    self.cursor.1 -= 1;
                } else if line > 0 {
                    self.cursor = (line - 1, self.lines[line - 1].len());
                }
                true
            }
            Key::Right => {
                let (line, col) = self.cursor;
                if col < self.lines[line].len() {
                    self.cursor.1 += 1;
                } else if line + 1 < self.lines.len() {
                    self.cursor = (line + 1, 0);
                }
                true
            }
            Key::Up => {
                if self.lines.len() == 1 {
                    self.history_prev();
                } else {
                    self.cursor.0 = self.cursor.0.saturating_sub(1);
                    self.clamp_col();
                }
                true
            }
            Key::Down => {
                if self.lines.len() == 1 {
                    self.history_next();
                } else if self.cursor.0 + 1 < self.lines.len() {
                    self.cursor.0 += 1;
                    self.clamp_col();
                }
                true
            }
            Key::Home | Key::Ctrl('a') => {
                self.cursor.1 = 0;
                true
            }
            Key::End | Key::Ctrl('e') => {
                self.cursor.1 = self.lines[self.cursor.0].len();
                true
            }
            Key::Ctrl('u') => {
                // kill 光标前行首内容(emacs 习惯)
                let (line, col) = self.cursor;
                if col > 0 {
                    self.push_undo();
                    let killed: String = self.lines[line][..col].iter().collect();
                    self.push_kill(killed);
                    self.lines[line].drain(..col);
                    self.cursor.1 = 0;
                }
                true
            }
            Key::Ctrl('k') => {
                // kill 光标到行尾;行尾则删除换行(合并下一行)
                let (line, col) = self.cursor;
                if col < self.lines[line].len() {
                    self.push_undo();
                    let killed: String = self.lines[line][col..].iter().collect();
                    self.push_kill(killed);
                    self.lines[line].truncate(col);
                } else if line + 1 < self.lines.len() {
                    self.push_undo();
                    let next = self.lines.remove(line + 1);
                    self.lines[line].extend(next);
                }
                true
            }
            Key::Ctrl('w') => {
                let (line, col) = self.cursor;
                let word_start = self.prev_word_index();
                if word_start < col {
                    self.push_undo();
                    let killed: String = self.lines[line][word_start..col].iter().collect();
                    self.push_kill(killed);
                    self.lines[line].drain(word_start..col);
                    self.cursor.1 = word_start;
                }
                true
            }
            Key::Ctrl('y') => {
                let last = self.kill_ring.last().cloned();
                if let Some(text) = last {
                    self.push_undo();
                    self.insert_text(&text);
                }
                true
            }
            _ => false,
        }
    }

    pub fn undo(&mut self) {
        if let Some(snapshot) = self.undo_stack.pop() {
            self.lines = snapshot.lines;
            self.cursor = snapshot.cursor;
            self.history_index = None;
        }
    }

    /// 提交时把当前文本压入历史(空文本不记)。
    pub fn commit_history(&mut self) {
        let text = self.text();
        if !text.trim().is_empty() && self.history.last().map(|h| h != &text).unwrap_or(true) {
            self.history.push(text);
        }
        self.history_index = None;
        self.draft = None;
    }

    /// 可视化视图:逻辑行按宽度折行成视觉行(最多 max_rows 行,超出取尾部),
    /// 并给出光标在视觉行中的 (行, 显示列)。
    pub fn view(&self, width: usize, max_rows: usize) -> EditorView {
        let width = width.max(1);
        let mut rows: Vec<String> = Vec::new();
        let mut cursor: Option<(usize, usize)> = None;
        for (line_index, line) in self.lines.iter().enumerate() {
            let text: String = line.iter().collect();
            let wrapped = wrap_to_width(&text, width);
            for (row_in_line, row_text) in wrapped.iter().enumerate() {
                rows.push(row_text.clone());
                // 光标落点:光标所在逻辑行的第 N 视觉行
                if line_index == self.cursor.0 {
                    let col = self.cursor.1;
                    let before: String = line[..col.min(line.len())].iter().collect();
                    let before_rows = wrap_to_width(&before, width);
                    let cursor_row_in_line = before_rows.len().saturating_sub(1);
                    if cursor_row_in_line == row_in_line {
                        let row_prefix = before_rows.last().map(String::as_str).unwrap_or("");
                        cursor = Some((rows.len() - 1, display_width(row_prefix)));
                    }
                }
            }
        }
        let total = rows.len();
        let skip = total.saturating_sub(max_rows.max(1));
        let rows: Vec<String> = rows.into_iter().skip(skip).collect();
        let cursor = cursor.map(|(row, col)| (row.saturating_sub(skip), col));
        EditorView {
            rows,
            cursor,
            total_rows: total,
        }
    }

    fn insert_text(&mut self, text: &str) {
        let segments: Vec<&str> = text
            .split('\n')
            .map(|s| s.strip_suffix('\r').unwrap_or(s))
            .collect();
        let (start_line, start_col) = self.cursor;
        // 尾段先摘下:首段进当前行,其余段逐行插入,尾段拼回最后一行
        let tail: Vec<char> = self.lines[start_line].split_off(start_col);
        self.lines[start_line].extend(segments[0].chars());
        let mut current_line = start_line;
        for segment in &segments[1..] {
            self.lines
                .insert(current_line + 1, segment.chars().collect());
            current_line += 1;
        }
        self.lines[current_line].extend(tail.iter());
        self.cursor = (current_line, self.lines[current_line].len() - tail.len());
    }

    fn insert_newline(&mut self) {
        let (line, col) = self.cursor;
        let tail: Vec<char> = self.lines[line].split_off(col);
        self.lines.insert(line + 1, tail);
        self.cursor = (line + 1, 0);
    }

    fn clamp_col(&mut self) {
        let len = self.lines[self.cursor.0].len();
        self.cursor.1 = self.cursor.1.min(len);
    }

    fn push_undo(&mut self) {
        if self.undo_stack.len() >= MAX_UNDO {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(EditorSnapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
        });
    }

    fn push_kill(&mut self, text: String) {
        if !text.is_empty() {
            if self.kill_ring.len() >= MAX_KILL_RING {
                self.kill_ring.remove(0);
            }
            self.kill_ring.push(text);
        }
    }

    /// 前一个词的起始下标(词 = 非空白连续段;不跨行)。
    fn prev_word_index(&self) -> usize {
        let (line, mut i) = self.cursor;
        let chars = &self.lines[line];
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_index {
            None => {
                self.draft = Some(self.text());
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.history_index = Some(index);
        let text = self.history[index].clone();
        self.set_text_quiet(&text);
    }

    fn history_next(&mut self) {
        let index = match self.history_index {
            None => return,
            Some(i) if i + 1 >= self.history.len() => {
                self.history_index = None;
                let draft = self.draft.take().unwrap_or_default();
                self.set_text_quiet(&draft);
                return;
            }
            Some(i) => i + 1,
        };
        self.history_index = Some(index);
        let text = self.history[index].clone();
        self.set_text_quiet(&text);
    }

    /// 历史替换不入 undo 栈。
    fn set_text_quiet(&mut self, text: &str) {
        self.lines = text
            .split('\n')
            .map(|line| line.chars().collect())
            .collect();
        if self.lines.is_empty() {
            self.lines = vec![Vec::new()];
        }
        let last = self.lines.len() - 1;
        self.cursor = (last, self.lines[last].len());
    }
}

/// 编辑器可视化视图。
pub struct EditorView {
    /// 视觉行(已折行,≤ max_rows)
    pub rows: Vec<String>,
    /// 光标在视觉行中的 (行, 显示列);相对返回的 rows
    pub cursor: Option<(usize, usize)>,
    /// 折行后的总视觉行数(窗口裁剪前)
    pub total_rows: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_and_backspace() {
        let mut editor = Editor::new();
        for k in [Key::Char('a'), Key::Char('b'), Key::Char('c')] {
            editor.handle_key(&k);
        }
        assert_eq!(editor.text(), "abc");
        assert_eq!(editor.cursor_pos(), (0, 3));
        editor.handle_key(&Key::Backspace);
        assert_eq!(editor.text(), "ab");
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let mut editor = Editor::new();
        editor.handle_key(&Key::Backspace);
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn multiline_newline_and_join() {
        let mut editor = Editor::new();
        editor.set_text("ab");
        editor.handle_key(&Key::ShiftEnter);
        editor.handle_key(&Key::Char('c'));
        assert_eq!(editor.text(), "ab\nc");
        assert_eq!(editor.cursor_pos(), (1, 1));
        // 行首回删合并上一行
        editor.handle_key(&Key::Home);
        editor.handle_key(&Key::Backspace);
        assert_eq!(editor.text(), "abc");
        assert_eq!(editor.cursor_pos(), (0, 2));
    }

    #[test]
    fn paste_preserves_newlines() {
        let mut editor = Editor::new();
        editor.handle_key(&Key::Paste("ab\ncd".into()));
        assert_eq!(editor.text(), "ab\ncd");
        assert_eq!(editor.line_count(), 2);
    }

    #[test]
    fn paste_middle_of_line_splits() {
        let mut editor = Editor::new();
        editor.set_text("ab");
        editor.handle_key(&Key::Left);
        editor.handle_key(&Key::Paste("X\nY".into()));
        assert_eq!(editor.text(), "aX\nYb");
    }

    #[test]
    fn cursor_moves_across_lines() {
        let mut editor = Editor::new();
        editor.set_text("ab\nc");
        editor.handle_key(&Key::Up);
        // 列保留(不重置到行首)
        assert_eq!(editor.cursor_pos(), (0, 1));
        editor.handle_key(&Key::End);
        assert_eq!(editor.cursor_pos(), (0, 2));
        editor.handle_key(&Key::Down);
        assert_eq!(editor.cursor_pos(), (1, 1));
        editor.handle_key(&Key::Left);
        assert_eq!(editor.cursor_pos(), (1, 0));
    }

    #[test]
    fn history_only_on_single_line() {
        let mut editor = Editor::new();
        editor.set_text("first");
        editor.commit_history();
        editor.set_text("second");
        editor.commit_history();
        editor.set_text("draft");

        editor.handle_key(&Key::Up);
        assert_eq!(editor.text(), "second");
        editor.handle_key(&Key::Up);
        assert_eq!(editor.text(), "first");
        editor.handle_key(&Key::Down);
        editor.handle_key(&Key::Down);
        assert_eq!(editor.text(), "draft");

        // 多行时 Up/Down 是行导航,不是历史
        editor.set_text("a\nb");
        editor.handle_key(&Key::Up);
        assert_eq!(editor.text(), "a\nb");
        assert_eq!(editor.cursor_pos(), (0, 1));
    }

    #[test]
    fn kill_yank_and_undo() {
        let mut editor = Editor::new();
        editor.set_text("hello world");
        editor.handle_key(&Key::Ctrl('a'));
        editor.handle_key(&Key::Ctrl('k'));
        assert_eq!(editor.text(), "");
        editor.handle_key(&Key::Ctrl('y'));
        assert_eq!(editor.text(), "hello world");
        editor.undo();
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn kill_word_backwards() {
        let mut editor = Editor::new();
        editor.set_text("foo bar");
        editor.handle_key(&Key::Ctrl('w'));
        assert_eq!(editor.text(), "foo ");
        editor.handle_key(&Key::Ctrl('y'));
        assert_eq!(editor.text(), "foo bar");
    }

    #[test]
    fn multibyte_insert_and_width() {
        let mut editor = Editor::new();
        editor.set_text("中文");
        editor.handle_key(&Key::Left);
        editor.handle_key(&Key::Char('x'));
        assert_eq!(editor.text(), "中x文");
        let view = editor.view(40, 10);
        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.cursor, Some((0, 3))); // 中(2) + x(1)
    }

    #[test]
    fn view_wraps_and_keeps_cursor_visible() {
        let mut editor = Editor::new();
        editor.set_text(&"x".repeat(50));
        editor.handle_key(&Key::Left); // 光标在末尾前一格
        let view = editor.view(10, 4);
        assert!(view.rows.len() <= 4);
        assert_eq!(view.total_rows, 5);
        let (row, col) = view.cursor.unwrap();
        assert!(row < view.rows.len());
        assert!(col <= 10);
    }

    #[test]
    fn view_multiline_cursor_rows() {
        let mut editor = Editor::new();
        editor.set_text("aaa\nb\nccc");
        editor.handle_key(&Key::Up);
        editor.handle_key(&Key::Up);
        let view = editor.view(10, 10);
        assert_eq!(view.rows, vec!["aaa", "b", "ccc"]);
        // 两次 Up:ccc → b → aaa,列保留
        assert_eq!(view.cursor, Some((0, 1)));
    }

    #[test]
    fn unconsumed_keys_return_false() {
        let mut editor = Editor::new();
        assert!(!editor.handle_key(&Key::Enter));
        assert!(!editor.handle_key(&Key::Esc));
        assert!(!editor.handle_key(&Key::Ctrl('c')));
    }
}
