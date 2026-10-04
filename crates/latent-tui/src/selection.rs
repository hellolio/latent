//! 鼠标选择与复制(fullscreen 模式):鼠标捕获开启后终端不再提供原生
//! 选择,这里实现应用层选区——单元格坐标的行选区间、按显示宽度切片
//! (CJK 感知)、纯文本提取与 OSC 52 剪贴板写入(macOS cmd+C / Linux
//! Ctrl+Shift+C 触发;拖选松开时自动复制,对齐 pi fullscreenCopyOnSelect)。

use ratatui::text::Line;

use crate::width::char_width;

/// 单元格列坐标(0 起)。
pub type Col = u16;

/// 选区锚点/终点:**帧行号**(0 = committed 首行;≥ committed 长度的值
/// 表示尾部行)+ 列。行号锚定内容而非屏幕 —— 视口滚动后高亮与复制
/// 仍跟随原文字。
pub type SelPoint = (usize, Col);

/// 左键鼠标手势(按下/拖动/抬起;SGR 坐标经 crossterm 解析)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    /// `extend` = 修饰键(Shift 或 Alt)点击:扩展现有选区到点击处
    /// (锚点不变)。Shift 被多数终端截留为原生选择,Alt 是可靠替代
    Down { col: u16, row: u16, extend: bool },
    Drag { col: u16, row: u16 },
    Up { col: u16, row: u16 },
}

/// 给定锚点与终点,计算第 `row` 帧行被选中的列区间(半开区间,终端惯例
/// 的行选择:单行 = 两端列之间;首行 = 起点列到行尾;末行 = 行首到终点列;
/// 中间行整行)。该行未被选中返回 None。
pub fn row_range(anchor: SelPoint, end: SelPoint, row: usize, width: u16) -> Option<(u16, u16)> {
    let (first, last) = if (anchor.0, anchor.1) <= (end.0, end.1) {
        (anchor, end)
    } else {
        (end, anchor)
    };
    if row < first.0 || row > last.0 {
        return None;
    }
    let (start, end) = if first.0 == last.0 {
        (first.1.min(last.1), last.1.max(first.1).saturating_add(1))
    } else if row == first.0 {
        (first.1, width)
    } else if row == last.0 {
        (0, last.1.saturating_add(1))
    } else {
        (0, width)
    };
    Some((start.min(width), end.min(width)))
}

/// 按显示宽度切分字符串:前 `cells` 个单元格宽度的部分与其余部分
/// (宽字符不跨切分点)。
pub fn split_at_width(s: &str, cells: usize) -> (&str, &str) {
    let mut off = 0usize;
    for (idx, c) in s.char_indices() {
        if off >= cells {
            return s.split_at(idx);
        }
        off += char_width(c);
    }
    (s, "")
}

/// 取单元格区间 `[from, to)` 内的文本(宽字符起点落在区间内即收录)。
pub fn slice_cells(text: &str, from: usize, to: usize) -> String {
    let mut out = String::new();
    let mut off = 0usize;
    for c in text.chars() {
        if off >= to {
            break;
        }
        if off >= from {
            out.push(c);
        }
        off += char_width(c);
    }
    out
}

/// 行的纯文本(拼接全部 span,不含样式)。
pub fn line_plain(line: &Line<'_>) -> String {
    let mut out = String::new();
    for span in &line.spans {
        out.push_str(span.content.as_ref());
    }
    out
}

/// 选区文本(多行,`\n` 连接;每行按选区列区间切片后去行尾空白)。
pub fn selection_text(rows: &[(String, (u16, u16))]) -> String {
    let mut lines = Vec::new();
    for (text, (from, to)) in rows {
        let sliced = slice_cells(text, *from as usize, *to as usize);
        lines.push(sliced.trim_end().to_string());
    }
    while matches!(lines.last(), Some(l) if l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// OSC 52 剪贴板写入序列(base64;BEL 结尾,主流终端均支持,不支持的
/// 终端静默忽略)。
pub fn osc52_clipboard(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()))
}

/// 标准 base64(无外部依赖,20 行)。
pub fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    #[test]
    fn row_range_covers_single_and_multi_row_selections() {
        let width = 10;
        // 单行:两端列之间(含拖动终点单元格)
        assert_eq!(row_range((2, 3), (2, 5), 2, width), Some((3, 6)));
        assert_eq!(row_range((2, 5), (2, 3), 2, width), Some((3, 6)));
        // 多行:首行从起点列到行尾,中间整行,末行从行首到终点列
        assert_eq!(row_range((2, 3), (4, 5), 2, width), Some((3, 10)));
        assert_eq!(row_range((2, 3), (4, 5), 3, width), Some((0, 10)));
        assert_eq!(row_range((2, 3), (4, 5), 4, width), Some((0, 6)));
        // 行外
        assert_eq!(row_range((2, 3), (4, 5), 1, width), None);
        assert_eq!(row_range((2, 3), (4, 5), 5, width), None);
        // 反向拖动
        assert_eq!(row_range((4, 5), (2, 3), 2, width), Some((3, 10)));
        assert_eq!(row_range((4, 5), (2, 3), 4, width), Some((0, 6)));
    }

    #[test]
    fn slice_cells_is_width_aware() {
        // CJK 每字 2 格:取 [1,5) = 第 2 格起的两个整字
        assert_eq!(slice_cells("汉a字b", 1, 5), "a字");
        assert_eq!(slice_cells("汉a字b", 0, 2), "汉");
        assert_eq!(slice_cells("abc", 1, 2), "b");
        assert_eq!(slice_cells("abc", 0, 10), "abc");
        assert_eq!(slice_cells("abc", 2, 2), "");
    }

    #[test]
    fn split_at_width_splits_on_char_boundary() {
        let (head, tail) = split_at_width("汉a字", 2);
        assert_eq!(head, "汉");
        assert_eq!(tail, "a字");
        assert_eq!(split_at_width("abc", 0), ("", "abc"));
        assert_eq!(split_at_width("abc", 5), ("abc", ""));
    }

    #[test]
    fn line_plain_concatenates_spans() {
        let line = Line::from(vec![
            Span::raw("ab"),
            Span::styled("cd", ratatui::style::Style::new()),
        ]);
        assert_eq!(line_plain(&line), "abcd");
    }

    #[test]
    fn selection_text_trims_row_ends_and_drops_trailing_blanks() {
        let rows = vec![
            ("  hello   ".to_string(), (0u16, 9u16)),
            ("world".to_string(), (0u16, 5u16)),
            ("      ".to_string(), (0u16, 6u16)),
        ];
        assert_eq!(selection_text(&rows), "  hello\nworld");
        // 列区间切片生效
        let rows = vec![("abcdef".to_string(), (2u16, 4u16))];
        assert_eq!(selection_text(&rows), "cd");
    }

    #[test]
    fn base64_and_osc52_format() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b""), "");
        assert_eq!(osc52_clipboard("hi"), "\x1b]52;c;aGk=\x07");
    }
}
