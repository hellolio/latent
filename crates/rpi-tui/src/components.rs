//! 组件库(08 文档 §3 components/ 的 Rust 子集):`render(width)` 纯函数,
//! 输入宽度输出行数组,不做任何 I/O。
//!
//! 覆盖交互模式需要的四个:`Text`(折行文本)、`Markdown`(轻量内联渲染:
//! 标题/粗体/斜体/行内代码/代码围栏)、`SelectList`(高亮单选列表)、以及
//! `Component` trait 本身。`Loader`/`Image`/复杂布局等 M5 未消费的组件暂缺,
//! 需要时按同一 trait 扩展。

use crate::ansi::style;
use crate::width::{display_width, wrap_to_width};

/// 组件接口(08 文档 `Component`:核心是 `render(width): string[]`)。
pub trait Component {
    /// 在给定显示宽度下渲染为若干行(不含 ANSI 时每行显示宽度 ≤ width;
    /// 含 ANSI 转义的行由调用方按纯文本宽度对待)。
    fn render(&self, width: usize) -> Vec<String>;
}

/// 折行文本组件。
pub struct Text {
    pub text: String,
    pub color: Option<&'static str>,
}

impl Text {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into(), color: None }
    }

    pub fn colored(text: impl Into<String>, color: &'static str) -> Self {
        Self { text: text.into(), color: Some(color) }
    }
}

impl Component for Text {
    fn render(&self, width: usize) -> Vec<String> {
        let wrapped = wrap_to_width(&self.text, width.max(1));
        match self.color {
            Some(color) => wrapped
                .into_iter()
                .map(|line| format!("{color}{line}{}", style::RESET))
                .collect(),
            None => wrapped,
        }
    }
}

/// 轻量 Markdown 渲染:标题、无序列表、行内代码、粗体、斜体、代码围栏。
/// 不覆盖表格/嵌套列表/图片(pi 用 1015 行的完整实现;聊天场景这些够用)。
pub struct Markdown {
    pub source: String,
}

impl Markdown {
    pub fn new(source: impl Into<String>) -> Self {
        Self { source: source.into() }
    }
}

impl Component for Markdown {
    fn render(&self, width: usize) -> Vec<String> {
        let width = width.max(1);
        let mut lines = Vec::new();
        let mut in_fence = false;
        for raw in self.source.lines() {
            if let Some(rest) = raw.strip_prefix("```") {
                in_fence = !in_fence;
                lines.push(if in_fence {
                    format!("{}┌─ code ─{}", style::DIM, style::RESET)
                } else {
                    format!("{}└───────{}", style::DIM, style::RESET)
                });
                let _ = rest;
                continue;
            }
            if in_fence {
                for line in wrap_to_width(raw, width.saturating_sub(2)) {
                    lines.push(format!("{}│ {line}{}", style::DIM, style::RESET));
                }
                continue;
            }
            if let Some(header) = raw.strip_prefix("# ") {
                lines.extend(
                    render_inline(header, width)
                        .into_iter()
                        .map(|line| format!("{}{}{}", style::BOLD, line, style::RESET)),
                );
                continue;
            }
            if let Some(header) = raw.strip_prefix("## ") {
                lines.extend(
                    render_inline(header, width)
                        .into_iter()
                        .map(|line| format!("{}{}{}", style::UNDERLINE, line, style::RESET)),
                );
                continue;
            }
            if let Some(item) = raw.strip_prefix("- ").or_else(|| raw.strip_prefix("* ")) {
                for (i, line) in wrap_to_width(item, width.saturating_sub(2)).into_iter().enumerate()
                {
                    let bullet = if i == 0 { "• " } else { "  " };
                    lines.push(format!("{bullet}{line}"));
                }
                continue;
            }
            for line in render_inline(raw, width) {
                lines.push(line);
            }
        }
        lines
    }
}

/// 行内标记 → ANSI(`` `code` ``、`**bold**`、`*italic*`);折行在标记解析后
/// 按纯文本粗略进行(ANSI 长度不计入宽度)。
fn render_inline(text: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`' {
            if let Some(close) = chars[i + 1..].iter().position(|c| *c == '`') {
                let code: String = chars[i + 1..i + 1 + close].iter().collect();
                out.push_str(&format!("{}{code}{}", style::CYAN, style::RESET));
                i += close + 2;
                continue;
            }
        }
        if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(close) = find_pattern(&chars, i + 2, &['*', '*']) {
                let bold: String = chars[i + 2..close].iter().collect();
                out.push_str(&format!("{}{bold}{}", style::BOLD, style::RESET));
                i = close + 2;
                continue;
            }
        }
        if chars[i] == '*' {
            if let Some(close) = chars[i + 1..].iter().position(|c| *c == '*') {
                if close > 0 {
                    let italic: String = chars[i + 1..i + 1 + close].iter().collect();
                    out.push_str(&format!("{}{italic}{}", style::ITALIC, style::RESET));
                    i += close + 2;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    wrap_plain_with_ansi(&out, width)
}

fn find_pattern(chars: &[char], from: usize, pattern: &[char]) -> Option<usize> {
    if pattern.is_empty() {
        return None;
    }
    let mut i = from;
    while i + pattern.len() <= chars.len() {
        if chars[i..i + pattern.len()] == *pattern {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 折行:忽略 ANSI 转义序列宽度,按可见字符断行,断点处原样保留转义。
fn skip_ansi(chars: &mut std::iter::Peekable<std::str::Chars>, current: &mut String) {
    // CSI 形如 ESC [ params final(0x40-0x7E);'[' 本身在区间内,先单独消费
    current.push('\x1b');
    if chars.peek() == Some(&'[') {
        current.push('[');
        chars.next();
    }
    while let Some(&n) = chars.peek() {
        current.push(n);
        chars.next();
        if ('\x40'..='\x7e').contains(&n) && n != '[' {
            break;
        }
    }
}

fn wrap_plain_with_ansi(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut visible = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            skip_ansi(&mut chars, &mut current);
            continue;
        }
        if visible >= width {
            lines.push(std::mem::take(&mut current));
            visible = 0;
        }
        current.push(c);
        visible += crate::width::char_width(c);
    }
    lines.push(current);
    lines
}

/// 高亮单选列表(ExtensionUi select 的渲染基础)。
pub struct SelectList {
    pub options: Vec<String>,
    pub selected: usize,
}

impl SelectList {
    pub fn new(options: Vec<String>) -> Self {
        Self { options, selected: 0 }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.options.len() {
            self.selected += 1;
        }
    }

    pub fn selected_option(&self) -> Option<&String> {
        self.options.get(self.selected)
    }
}

impl Component for SelectList {
    fn render(&self, width: usize) -> Vec<String> {
        let width = width.max(1);
        self.options
            .iter()
            .enumerate()
            .map(|(i, option)| {
                let marker = if i == self.selected { "❯ " } else { "  " };
                let line = format!("{marker}{option}");
                let plain_width = display_width(&line);
                if plain_width > width {
                    let (cut, _) = crate::width::truncate_to_width(&line, width);
                    return cut;
                }
                if i == self.selected {
                    format!("{}{line}{}", style::REVERSE, style::RESET)
                } else {
                    line
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible_len(s: &str) -> usize {
        display_width(&strip_ansi(s))
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // CSI:ESC [ params final;'[' 需先消费再找终止字节
                if chars.peek() == Some(&'[') {
                    chars.next();
                }
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if ('\x40'..='\x7e').contains(&n) && n != '[' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn text_wraps_to_width() {
        let text = Text::new("hello world");
        assert_eq!(text.render(5), vec!["hello", "world"]);
    }

    #[test]
    fn text_color_wraps_lines_individually() {
        let text = Text::colored("ab cd", style::RED);
        let lines = text.render(2);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with(style::RED));
        assert!(lines[0].ends_with(style::RESET));
    }

    #[test]
    fn markdown_headers_and_lists() {
        let md = Markdown::new("# Title\n- item one\n- item two");
        let lines = md.render(40);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with(style::BOLD));
        assert_eq!(strip_ansi(&lines[0]), "Title");
        assert_eq!(strip_ansi(&lines[1]), "• item one");
        assert_eq!(strip_ansi(&lines[2]), "• item two");
    }

    #[test]
    fn markdown_inline_code_and_bold() {
        let md = Markdown::new("use `x` and **bold** and *it*");
        let lines = md.render(80);
        assert_eq!(lines.len(), 1);
        let plain = strip_ansi(&lines[0]);
        assert!(plain.contains("x") && plain.contains("bold") && plain.contains("it"));
    }

    #[test]
    fn markdown_code_fence() {
        let md = Markdown::new("```rust\nlet a = 1;\n```");
        let lines = md.render(40);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("code"));
        assert!(strip_ansi(&lines[1]).starts_with("│ let a = 1;"));
    }

    #[test]
    fn markdown_wraps_long_lines_to_width() {
        let md = Markdown::new("aaaa bbbb cccc dddd");
        for line in md.render(9) {
            assert!(visible_len(&line) <= 9, "line too wide: {line:?}");
        }
    }

    #[test]
    fn select_list_highlight_and_navigation() {
        let mut list = SelectList::new(vec!["a".into(), "b".into(), "c".into()]);
        let lines = list.render(20);
        assert_eq!(lines.len(), 3);
        assert_eq!(strip_ansi(&lines[0]), "❯ a");
        assert_eq!(strip_ansi(&lines[1]), "  b");
        assert!(lines[0].starts_with(style::REVERSE));
        list.move_down();
        assert_eq!(list.selected_option().map(String::as_str), Some("b"));
        list.move_down();
        list.move_down(); // 到底不再下移
        assert_eq!(list.selected_option().map(String::as_str), Some("c"));
        list.move_up();
        assert_eq!(list.selected_option().map(String::as_str), Some("b"));
        list.move_up();
        list.move_up(); // 到顶不再上移
        assert_eq!(list.selected_option().map(String::as_str), Some("a"));
    }

    #[test]
    fn select_list_empty_is_empty() {
        let mut list = SelectList::new(vec![]);
        assert!(list.render(10).is_empty());
        list.move_down(); // 边界:空列表不动
        assert_eq!(list.selected, 0);
    }
}
