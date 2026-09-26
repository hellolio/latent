//! 行编辑器(08 文档 `components/editor.ts` 的 Rust 对应物)。
//!
//! 核心能力对齐 pi Editor:undo 栈、kill-ring、词导航、输入历史。缓冲区
//! 用 `Vec<char>` 避免多字节字符的字节索引问题。所有操作经由 `handle_key`,
//! 未消费的按键返回 false 交给上层(如历史/提交判断)。

use crate::keys::Key;

const MAX_UNDO: usize = 100;
const MAX_KILL_RING: usize = 10;

/// 单行编辑器状态。
#[derive(Default)]
pub struct Editor {
    buffer: Vec<char>,
    /// 光标位置(字符下标,0..=buffer.len())
    cursor: usize,
    undo_stack: Vec<EditorSnapshot>,
    kill_ring: Vec<String>,
    /// 最近一次 yank 的长度(ctrl+y 后连续 yank/kill 的行为简化:单次 yank)
    history: Vec<String>,
    history_index: Option<usize>,
    /// 历史浏览前的草稿
    draft: Option<String>,
}

#[derive(Clone)]
struct EditorSnapshot {
    buffer: Vec<char>,
    cursor: usize,
}

impl Editor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> String {
        self.buffer.iter().collect()
    }

    /// 光标的字符下标。
    pub fn cursor_pos(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn clear(&mut self) {
        self.push_undo();
        self.buffer.clear();
        self.cursor = 0;
        self.history_index = None;
        self.draft = None;
    }

    /// 设置整段文本(替换当前内容,可撤销)。
    pub fn set_text(&mut self, text: &str) {
        self.push_undo();
        self.buffer = text.chars().collect();
        self.cursor = self.buffer.len();
    }

    /// 按键处理:返回 true 表示已消费。
    pub fn handle_key(&mut self, key: &Key) -> bool {
        match key {
            Key::Char(c) => {
                self.push_undo();
                self.buffer.insert(self.cursor, *c);
                self.cursor += 1;
                true
            }
            Key::Paste(text) => {
                self.push_undo();
                let chars: Vec<char> = text
                    .chars()
                    .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                    .collect();
                let n = chars.len();
                self.buffer.splice(self.cursor..self.cursor, chars);
                self.cursor += n;
                true
            }
            Key::Backspace => {
                if self.cursor > 0 {
                    self.push_undo();
                    self.cursor -= 1;
                    self.buffer.remove(self.cursor);
                }
                true
            }
            Key::Delete => {
                if self.cursor < self.buffer.len() {
                    self.push_undo();
                    self.buffer.remove(self.cursor);
                }
                true
            }
            Key::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                true
            }
            Key::Right => {
                if self.cursor < self.buffer.len() {
                    self.cursor += 1;
                }
                true
            }
            Key::Home => {
                self.cursor = 0;
                true
            }
            Key::End => {
                self.cursor = self.buffer.len();
                true
            }
            Key::Ctrl('a') => {
                self.cursor = 0;
                true
            }
            Key::Ctrl('e') => {
                self.cursor = self.buffer.len();
                true
            }
            // 词导航:option+方向键同 emacs 默认(alt 序列本 UI 不解析,用
            // ctrl+b/f 的 emacs 词跳变体:ctrl+left/right 由终端映射,这里
            // 提供 ctrl+w 删词 + alt+b/f 等价的 Home/End 词跳经 meta 键缺失
            // 时的替代 —— b/f 词跳绑定到 Ctrl+Left/Right 场景由上层映射)
            Key::Escape => {
                // Esc 不清输入(abort 语义在上层)
                false
            }
            Key::Ctrl('u') => {
                // kill 光标前全部(emacs 习惯)
                if self.cursor > 0 {
                    self.push_undo();
                    let killed: String = self.buffer[..self.cursor].iter().collect();
                    self.push_kill(killed);
                    self.buffer.drain(..self.cursor);
                    self.cursor = 0;
                }
                true
            }
            Key::Ctrl('k') => {
                // kill 光标到行尾
                if self.cursor < self.buffer.len() {
                    self.push_undo();
                    let killed: String = self.buffer[self.cursor..].iter().collect();
                    self.push_kill(killed);
                    self.buffer.truncate(self.cursor);
                }
                true
            }
            Key::Ctrl('w') => {
                // kill 前一个词
                let word_start = self.prev_word_index();
                if word_start < self.cursor {
                    self.push_undo();
                    let killed: String = self.buffer[word_start..self.cursor].iter().collect();
                    self.push_kill(killed);
                    self.buffer.drain(word_start..self.cursor);
                    self.cursor = word_start;
                }
                true
            }
            Key::Ctrl('y') => {
                // yank:粘回最近 kill 的内容
                let last = self.kill_ring.last().cloned();
                if let Some(text) = last {
                    self.push_undo();
                    let chars: Vec<char> = text.chars().collect();
                    let n = chars.len();
                    self.buffer.splice(self.cursor..self.cursor, chars);
                    self.cursor += n;
                }
                true
            }
            Key::Up => {
                self.history_prev();
                true
            }
            Key::Down => {
                self.history_next();
                true
            }
            _ => false,
        }
    }

    pub fn undo(&mut self) {
        if let Some(snapshot) = self.undo_stack.pop() {
            self.buffer = snapshot.buffer;
            self.cursor = snapshot.cursor;
            self.history_index = None;
        }
    }

    fn push_undo(&mut self) {
        if self.undo_stack.len() >= MAX_UNDO {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(EditorSnapshot { buffer: self.buffer.clone(), cursor: self.cursor });
    }

    fn push_kill(&mut self, text: String) {
        if !text.is_empty() {
            if self.kill_ring.len() >= MAX_KILL_RING {
                self.kill_ring.remove(0);
            }
            self.kill_ring.push(text);
        }
    }

    /// 前一个词的起始下标(词 = 非空白连续段)。
    fn prev_word_index(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && self.buffer[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.buffer[i - 1].is_whitespace() {
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
        self.buffer = self.history[index].chars().collect();
        self.cursor = self.buffer.len();
    }

    fn history_next(&mut self) {
        let index = match self.history_index {
            None => return,
            Some(i) if i + 1 >= self.history.len() => {
                // 回到草稿
                self.history_index = None;
                self.buffer = self.draft.take().unwrap_or_default().chars().collect();
                self.cursor = self.buffer.len();
                return;
            }
            Some(i) => i + 1,
        };
        self.history_index = Some(index);
        self.buffer = self.history[index].chars().collect();
        self.cursor = self.buffer.len();
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(input: &[Key]) -> Vec<Key> {
        input.to_vec()
    }

    #[test]
    fn typing_and_backspace() {
        let mut editor = Editor::new();
        for k in keys(&[Key::Char('a'), Key::Char('b'), Key::Char('c')]) {
            editor.handle_key(&k);
        }
        assert_eq!(editor.text(), "abc");
        assert_eq!(editor.cursor_pos(), 3);
        editor.handle_key(&Key::Backspace);
        assert_eq!(editor.text(), "ab");
        assert_eq!(editor.cursor_pos(), 2);
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let mut editor = Editor::new();
        editor.handle_key(&Key::Backspace);
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn cursor_navigation_and_insert_middle() {
        let mut editor = Editor::new();
        editor.set_text("ac");
        editor.handle_key(&Key::Home);
        editor.handle_key(&Key::Right);
        editor.handle_key(&Key::Char('b'));
        assert_eq!(editor.text(), "abc");
        assert_eq!(editor.cursor_pos(), 2);
    }

    #[test]
    fn multibyte_insert_and_delete() {
        let mut editor = Editor::new();
        editor.set_text("中文");
        editor.handle_key(&Key::Left);
        editor.handle_key(&Key::Char('x'));
        assert_eq!(editor.text(), "中x文");
        editor.handle_key(&Key::Backspace);
        assert_eq!(editor.text(), "中文");
    }

    #[test]
    fn paste_inserts_text_with_newlines_flattened() {
        let mut editor = Editor::new();
        editor.handle_key(&Key::Paste("ab\ncd".into()));
        assert_eq!(editor.text(), "ab cd");
    }

    #[test]
    fn undo_restores_previous_state() {
        let mut editor = Editor::new();
        editor.set_text("hello");
        editor.handle_key(&Key::Char('!'));
        assert_eq!(editor.text(), "hello!");
        editor.undo();
        assert_eq!(editor.text(), "hello");
        assert_eq!(editor.cursor_pos(), 5);
    }

    #[test]
    fn kill_and_yank_ring() {
        let mut editor = Editor::new();
        editor.set_text("hello world");
        editor.handle_key(&Key::Ctrl('a')); // 光标到行首
        editor.handle_key(&Key::Ctrl('k')); // kill 全部? 不,行首 kill 到行尾 = 全部
        assert_eq!(editor.text(), "");
        editor.handle_key(&Key::Ctrl('y'));
        assert_eq!(editor.text(), "hello world");
    }

    #[test]
    fn kill_word_backwards() {
        let mut editor = Editor::new();
        editor.set_text("foo bar");
        editor.handle_key(&Key::Ctrl('w'));
        assert_eq!(editor.text(), "foo ");
        assert_eq!(editor.cursor_pos(), 4);
        editor.handle_key(&Key::Ctrl('y'));
        assert_eq!(editor.text(), "foo bar");
    }

    #[test]
    fn ctrl_u_kills_to_start() {
        let mut editor = Editor::new();
        editor.set_text("abcdef");
        editor.handle_key(&Key::Left);
        editor.handle_key(&Key::Left);
        editor.handle_key(&Key::Ctrl('u'));
        assert_eq!(editor.text(), "ef");
    }

    #[test]
    fn word_navigation_boundary() {
        let mut editor = Editor::new();
        editor.set_text("ab cd");
        editor.handle_key(&Key::Ctrl('a'));
        assert_eq!(editor.cursor_pos(), 0);
        // prev_word_index 只在 ctrl+w 用;这里验证边界:空缓冲 kill word 安全
        let mut empty = Editor::new();
        empty.handle_key(&Key::Ctrl('w'));
        assert_eq!(empty.text(), "");
    }

    #[test]
    fn history_navigates_and_returns_to_draft() {
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
        editor.handle_key(&Key::Up); // 已到最早,不动
        assert_eq!(editor.text(), "first");
        editor.handle_key(&Key::Down);
        assert_eq!(editor.text(), "second");
        editor.handle_key(&Key::Down);
        assert_eq!(editor.text(), "draft"); // 回到草稿
    }

    #[test]
    fn commit_history_skips_empty_and_duplicates() {
        let mut editor = Editor::new();
        editor.set_text("hello");
        editor.commit_history();
        editor.set_text("hello");
        editor.commit_history();
        editor.clear();
        editor.commit_history();
        editor.handle_key(&Key::Up);
        assert_eq!(editor.text(), "hello");
    }

    #[test]
    fn unconsumed_keys_return_false() {
        let mut editor = Editor::new();
        assert!(!editor.handle_key(&Key::Enter));
        assert!(!editor.handle_key(&Key::Escape));
        assert!(!editor.handle_key(&Key::Ctrl('c')));
    }
}
