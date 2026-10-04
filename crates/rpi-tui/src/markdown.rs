//! Markdown 渲染(pi components/markdown.ts 的 Rust 子集):标题、有序/
//! 无序列表、引用、分隔线、围栏代码块(syntect 高亮)、行内代码/粗体/斜体/
//! 链接。输出 ratatui `Line`,折行在 span 层完成(样式不因折行丢失)。

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::highlight::Highlighter;
use crate::text::{line_text, wrap_line};
use crate::theme::Theme;

/// Markdown 渲染器。
pub struct Markdown<'a> {
    theme: &'a Theme,
    highlighter: Option<&'static Highlighter>,
}

impl<'a> Markdown<'a> {
    pub fn new(theme: &'a Theme) -> Self {
        Markdown {
            theme,
            highlighter: None,
        }
    }

    /// 启用代码块语法高亮(传入 `Highlighter::shared(theme.is_dark)`)。
    pub fn with_highlight(mut self, highlighter: &'static Highlighter) -> Self {
        self.highlighter = Some(highlighter);
        self
    }

    /// 渲染整个文档。
    pub fn render(&self, source: &str, width: usize) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut fence: Option<String> = None;
        let mut code: Vec<String> = Vec::new();
        let lines: Vec<&str> = source.lines().collect();
        let mut i = 0usize;
        while i < lines.len() {
            let raw = lines[i];
            if fence.is_some() {
                if raw.trim_start().starts_with("```") {
                    let lang = fence.take().unwrap_or_default();
                    out.extend(self.render_code_block(&code, Some(&lang), width));
                    code.clear();
                } else {
                    code.push(raw.to_string());
                }
                i += 1;
                continue;
            }
            let trimmed = raw.trim_start();
            if let Some(rest) = trimmed.strip_prefix("```") {
                fence = Some(rest.trim().to_string());
                i += 1;
                continue;
            }
            // 表格(GFM 管道表):连续以 | 开头的行整体收集渲染
            if trimmed.starts_with('|') {
                let start = i;
                while i < lines.len() && lines[i].trim_start().starts_with('|') {
                    i += 1;
                }
                out.extend(self.render_table(&lines[start..i], width));
                continue;
            }
            // 空行:分段(连续空行折叠)
            if raw.trim().is_empty() {
                if out
                    .last()
                    .map(|line| !line_text(line).is_empty())
                    .unwrap_or(false)
                {
                    out.push(Line::raw(""));
                }
                i += 1;
                continue;
            }
            // 标题 #..######
            let hashes = raw.chars().take_while(|c| *c == '#').count();
            if (1..=6).contains(&hashes) {
                let rest = raw[hashes..].trim_start();
                if raw[hashes..].starts_with(' ') || rest.is_empty() {
                    // 标题前空一行(文档开头除外):章节层次更清晰
                    if out
                        .last()
                        .map(|line| !line_text(line).is_empty())
                        .unwrap_or(false)
                    {
                        out.push(Line::raw(""));
                    }
                    out.extend(self.wrap_styled(
                        vec![Span::styled(
                            rest.to_string(),
                            Style::new()
                                .fg(self.theme.md_heading)
                                .add_modifier(ratatui::style::Modifier::BOLD),
                        )],
                        width,
                    ));
                    i += 1;
                    continue;
                }
            }
            // 分隔线
            if is_hr(trimmed) {
                out.push(
                    Span::styled(
                        "─".repeat(width.min(80)),
                        Style::new().fg(self.theme.md_code_block_border),
                    )
                    .into(),
                );
                i += 1;
                continue;
            }
            // 引用
            if let Some(quote) = trimmed.strip_prefix("> ") {
                let mut spans = vec![Span::styled(
                    "▌ ",
                    Style::new().fg(self.theme.md_code_block_border),
                )];
                spans.extend(self.render_inline(quote));
                out.extend(self.wrap_styled(spans, width));
                i += 1;
                continue;
            }
            // 列表(无序 / 有序)
            if let Some((indent, marker, item)) = parse_list_item(raw) {
                let bullet = match marker {
                    ListMarker::Bullet => "• ".to_string(),
                    ListMarker::Ordered(n) => format!("{n}. "),
                };
                let mut spans = vec![Span::raw(" ".repeat(indent))];
                spans.push(Span::styled(bullet, Style::new().fg(self.theme.accent)));
                spans.extend(self.render_inline(item));
                out.extend(self.wrap_styled(spans, width));
                i += 1;
                continue;
            }
            // 普通段落
            let spans = self.render_inline(raw);
            out.extend(self.wrap_styled(spans, width));
            i += 1;
        }
        // 未闭合围栏:剩余内容照常渲染
        if !code.is_empty() {
            out.extend(self.render_code_block(&code, fence.as_deref(), width));
        }
        out
    }

    /// 表格(GFM 管道表):box-drawing 边框(mdCodeBlockBorder),表头加粗。
    /// 列宽按内容自适应,总宽超限时逐列截断(单元格内不折行)。
    fn render_table(&self, rows: &[&str], width: usize) -> Vec<Line<'static>> {
        let border = Style::new().fg(self.theme.md_code_block_border);
        let parse_cells = |row: &str| -> Vec<String> {
            let t = row.trim().trim_start_matches('|').trim_end_matches('|');
            t.split('|').map(|c| c.trim().to_string()).collect()
        };
        let is_separator = |cells: &[String]| {
            !cells.is_empty()
                && cells.iter().all(|c| {
                    let stripped = c.replace(':', "");
                    !stripped.is_empty() && stripped.chars().all(|ch| ch == '-')
                })
        };
        let parsed: Vec<Vec<String>> = rows.iter().map(|r| parse_cells(r)).collect();
        let ncols = parsed.iter().map(Vec::len).max().unwrap_or(0);
        if ncols == 0 {
            return Vec::new();
        }
        // 表头识别:首行下方为分隔行(|---|---|)时首行为表头
        let (header, body): (Option<&Vec<String>>, &[Vec<String>]) = match parsed.split_first() {
            Some((first, rest)) if is_separator(first) => (None, rest),
            Some((first, rest)) => match rest.first() {
                Some(second) if is_separator(second) => (Some(first), &rest[1..]),
                _ => (None, &parsed[..]),
            },
            None => return Vec::new(),
        };
        // 列宽 = 各列最大显示宽;超宽时按均摊上限截断
        let mut widths = vec![0usize; ncols];
        for row in parsed.iter().filter(|r| !is_separator(r)) {
            for (j, cell) in row.iter().enumerate() {
                widths[j] = widths[j].max(crate::width::display_width(cell));
            }
        }
        let cap = (width.saturating_sub(1) / ncols).saturating_sub(3).max(1);
        for w in &mut widths {
            *w = (*w).min(cap);
        }
        let hline = |left: &str, mid: &str, right: &str| -> Line<'static> {
            let mut s = String::from(left);
            for (j, w) in widths.iter().enumerate() {
                if j > 0 {
                    s.push_str(mid);
                }
                s.push_str(&"─".repeat(w + 2));
            }
            s.push_str(right);
            Line::from(Span::styled(s, border))
        };
        let render_row = |cells: &[String], style: Style| -> Line<'static> {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (j, w) in widths.iter().enumerate() {
                let cell = cells.get(j).map(String::as_str).unwrap_or("");
                let (trunc, _) = crate::width::truncate_to_width(cell, *w);
                let pad = w.saturating_sub(crate::width::display_width(&trunc));
                spans.push(Span::styled("│ ".to_string(), border));
                spans.push(Span::styled(
                    format!("{trunc}{}", " ".repeat(pad + 1)),
                    style,
                ));
            }
            spans.push(Span::styled("│".to_string(), border));
            Line::from(spans)
        };
        let header_style = Style::new()
            .fg(self.theme.assistant_text)
            .add_modifier(ratatui::style::Modifier::BOLD);
        let body_style = Style::new().fg(self.theme.assistant_text);
        let mut out = vec![hline("╭", "┬", "╮")];
        if let Some(header) = header {
            out.push(render_row(header, header_style));
            out.push(hline("├", "┼", "┤"));
        }
        for row in body {
            out.push(render_row(row, body_style));
        }
        out.push(hline("╰", "┴", "╯"));
        out
    }

    /// 代码块:语言标注行 + 高亮正文 + 边框(pi mdCodeBlockBorder)。
    fn render_code_block(
        &self,
        code: &[String],
        lang: Option<&str>,
        width: usize,
    ) -> Vec<Line<'static>> {
        let border = Style::new().fg(self.theme.md_code_block_border);
        let mut out = Vec::new();
        let label = match lang {
            Some(lang) if !lang.is_empty() => format!("── {lang} "),
            _ => "── ".to_string(),
        };
        let used = 1 + label.chars().count(); // ╭ + label
        let mut top = vec![Span::styled("╭", border), Span::styled(label, border)];
        if used < width {
            top.push(Span::styled("─".repeat(width - used), border));
        }
        out.push(Line::from(top));
        let inner = width.saturating_sub(4).max(1); // "│ " 前缀 + 右侧留白
        let source = code.join("\n");
        let highlighted = match self.highlighter {
            Some(highlighter) => highlighter.highlight(&source, lang),
            None => vec![Line::from(source)],
        };
        for line in highlighted {
            for wrapped in wrap_line(&line, inner) {
                let mut spans = vec![Span::styled("│ ", border)];
                spans.extend(wrapped.spans);
                out.push(Line::from(spans));
            }
        }
        out.push(Line::from(Span::styled("╰".to_string(), border)));
        out
    }

    /// 行内标记 → spans(`` `code` ``、**粗体**、*斜体*、[文本](链接))。
    fn render_inline(&self, text: &str) -> Vec<Span<'static>> {
        let chars: Vec<char> = text.chars().collect();
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut plain = String::new();
        let mut i = 0usize;
        let push_plain = |spans: &mut Vec<Span<'static>>, plain: &mut String| {
            if !plain.is_empty() {
                spans.push(Span::raw(std::mem::take(plain)));
            }
        };
        while i < chars.len() {
            let c = chars[i];
            if c == '`' {
                if let Some(close) = find_char(&chars, i + 1, '`') {
                    let code: String = chars[i + 1..close].iter().collect();
                    push_plain(&mut spans, &mut plain);
                    spans.push(Span::styled(code, Style::new().fg(self.theme.md_code)));
                    i = close + 1;
                    continue;
                }
            }
            if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
                if let Some(close) = find_pattern(&chars, i + 2, "**") {
                    let bold: String = chars[i + 2..close].iter().collect();
                    push_plain(&mut spans, &mut plain);
                    spans.push(Span::styled(
                        bold,
                        Style::new()
                            .fg(self.theme.assistant_text)
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    ));
                    i = close + 2;
                    continue;
                }
            }
            if c == '*' {
                if let Some(close) = find_char(&chars, i + 1, '*') {
                    if close > i + 1 {
                        let italic: String = chars[i + 1..close].iter().collect();
                        push_plain(&mut spans, &mut plain);
                        spans.push(Span::styled(
                            italic,
                            Style::new()
                                .fg(self.theme.assistant_text)
                                .add_modifier(ratatui::style::Modifier::ITALIC),
                        ));
                        i = close + 1;
                        continue;
                    }
                }
            }
            if c == '[' {
                if let Some((text_end, link_end)) = parse_link(&chars, i) {
                    let label: String = chars[i + 1..text_end].iter().collect();
                    push_plain(&mut spans, &mut plain);
                    spans.push(Span::styled(
                        label,
                        Style::new()
                            .fg(self.theme.md_link)
                            .add_modifier(ratatui::style::Modifier::UNDERLINED),
                    ));
                    i = link_end + 1;
                    continue;
                }
            }
            plain.push(c);
            i += 1;
        }
        push_plain(&mut spans, &mut plain);
        spans
    }

    /// 带 spans 的行折行到宽度。
    fn wrap_styled(&self, spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
        wrap_line(&Line::from(spans), width.max(1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ListMarker {
    Bullet,
    Ordered(u64),
}

/// 解析列表项:返回 (缩进空格数, 标记, 内容)。
fn parse_list_item(raw: &str) -> Option<(usize, ListMarker, &str)> {
    let indent = raw.len() - raw.trim_start().len();
    let trimmed = raw.trim_start();
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        return Some((indent, ListMarker::Bullet, rest));
    }
    let digits: usize = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let rest = &trimmed[digits..];
        let number: u64 = trimmed[..digits].parse().ok()?;
        if let Some(item) = rest.strip_prefix(". ") {
            return Some((indent, ListMarker::Ordered(number), item));
        }
    }
    None
}

fn is_hr(trimmed: &str) -> bool {
    (trimmed.chars().all(|c| c == '-') && trimmed.chars().count() >= 3)
        || (trimmed.chars().all(|c| c == '*') && trimmed.chars().count() >= 3)
}

/// 从 `from` 下标起找目标字符(含 from)。必须真正从 from 起扫:闭合
/// 字符与起始字符相同时(`` `code` ``、`*斜体*`),若从 0 扫会先命中
/// 起始字符自身,行内标记永远配不上对。
fn find_char(chars: &[char], from: usize, target: char) -> Option<usize> {
    chars[from..]
        .iter()
        .position(|c| *c == target)
        .map(|pos| pos + from)
}

fn find_pattern(chars: &[char], from: usize, pattern: &str) -> Option<usize> {
    let pat: Vec<char> = pattern.chars().collect();
    let mut i = from;
    while i + pat.len() <= chars.len() {
        if chars[i..i + pat.len()] == pat[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 解析 `[label](url)`,返回 (']' 下标, ')' 下标)。
fn parse_link(chars: &[char], start: usize) -> Option<(usize, usize)> {
    let text_end = find_char(chars, start + 1, ']')?;
    if chars.get(text_end + 1) != Some(&'(') {
        return None;
    }
    let url_end = find_char(chars, text_end + 2, ')')?;
    Some((text_end, url_end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::line_text;

    fn theme() -> Theme {
        Theme::dark_ansi()
    }

    fn render(source: &str, width: usize) -> Vec<String> {
        Markdown::new(&theme())
            .render(source, width)
            .iter()
            .map(line_text)
            .collect()
    }

    #[test]
    fn headings_and_lists() {
        let out = render("# Title\n- item one\n- item two\n3. third", 40);
        assert_eq!(out[0], "Title");
        assert_eq!(out[1], "• item one");
        assert_eq!(out[2], "• item two");
        assert_eq!(out[3], "3. third");
    }

    #[test]
    fn inline_code_bold_italic_link() {
        let out = render("use `x` and **bold** and *it* and [l](u)", 80);
        assert_eq!(out.len(), 1);
        let text = &out[0];
        assert!(text.contains("x") && text.contains("bold") && text.contains("it"));
        assert!(text.contains("l"));
    }

    #[test]
    fn inline_code_strips_backticks_and_takes_code_color() {
        let t = theme();
        let lines = Markdown::new(&t).render("`/help` — 显示", 80);
        assert_eq!(lines.len(), 1);
        // 反引号必须被剥掉;行首 code span(闭合符与起始符同字符)也不能漏配对
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "/help — 显示", "{text:?}");
        let code_span = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "/help")
            .expect("code span 应独立存在");
        assert_eq!(code_span.style.fg, Some(t.md_code));
    }

    #[test]
    fn italic_takes_italic_style() {
        let t = theme();
        let lines = Markdown::new(&t).render("*it* ok", 80);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "it ok", "{text:?}");
        let span = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "it")
            .expect("italic span 应独立存在");
        assert!(
            span.style.add_modifier.contains(ratatui::style::Modifier::ITALIC),
            "斜体应有 ITALIC 修饰: {span:?}"
        );
    }

    #[test]
    fn code_block_with_language_and_border() {
        let t = theme();
        let md = Markdown::new(&t).with_highlight(Highlighter::shared(true));
        let out: Vec<String> = md
            .render("```rust\nlet a = 1;\nlet b = 2;\n```", 40)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(out.len(), 4, "{out:?}"); // 顶边框 + 2 行代码 + 底边框
        assert!(out[0].starts_with("╭── rust"), "{out:?}");
        assert!(out[1].starts_with("│ "));
        assert!(out[1].contains("let a = 1;"));
        assert!(out[3].starts_with("╰"), "{out:?}");
    }

    #[test]
    fn code_block_wraps_long_lines() {
        let t = theme();
        let md = Markdown::new(&t).with_highlight(Highlighter::shared(true));
        let out = md.render(&format!("```text\n{}\n```", "x".repeat(60)), 20);
        // 顶 + 至少 3 行折行 + 底
        assert!(out.len() >= 5, "{out:?}");
        for line in &out {
            let plain = line_text(line);
            assert!(plain.chars().count() <= 22, "line too wide: {plain:?}");
        }
    }

    #[test]
    fn paragraph_wraps_to_width() {
        let out = render("aaaa bbbb cccc dddd", 9);
        assert_eq!(out.len(), 2, "{out:?}");
        for line in &out {
            assert!(crate::width::display_width(line) <= 9);
        }
    }

    #[test]
    fn blockquote_and_hr() {
        let out = render("> quoted\n---", 20);
        assert!(out[0].starts_with("▌ "));
        assert!(out[0].contains("quoted"));
        assert!(out[1].starts_with("───"));
    }

    #[test]
    fn gfm_table_renders_box_drawing() {
        let out = render(
            "| 区块 | 内容 |\n|---|---|\n| 一 | 参数 |\n| 二 | 结果 |",
            40,
        );
        // 顶边框 + 表头 + 分隔 + 2 行正文 + 底边框
        assert_eq!(out.len(), 6, "{out:?}");
        assert!(out[0].starts_with("╭") && out[0].contains('┬'), "{out:?}");
        assert!(out[1].contains("区块") && out[1].contains("内容"));
        assert!(out[2].starts_with("├") && out[2].contains('┼'), "{out:?}");
        assert!(out[3].contains("一"), "{out:?}");
        assert!(out[4].contains("结果"), "{out:?}");
        assert!(out[5].starts_with("╰"), "{out:?}");
        // 不残留原始管道符文本形态(行首 | 已转为边框)
        for line in &out {
            assert!(!line.starts_with("| "), "{line:?}");
        }
    }

    #[test]
    fn table_without_header_separator_renders_all_rows() {
        let out = render("| a | b |\n| c | d |", 30);
        // 无分隔行:顶 + 2 行 + 底
        assert_eq!(out.len(), 4, "{out:?}");
    }

    #[test]
    fn blank_lines_collapse_between_paragraphs() {
        let out = render("a\n\n\n\nb", 10);
        assert_eq!(out, vec!["a", "", "b"]);
    }

    #[test]
    fn unclosed_fence_renders_rest_as_code() {
        let t = theme();
        let md = Markdown::new(&t).with_highlight(Highlighter::shared(true));
        let out = md.render("```rust\nlet x = 1;", 40);
        assert!(out.len() >= 3);
    }
}
