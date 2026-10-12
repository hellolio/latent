//! Markdown 渲染(pi components/markdown.ts 的 Rust 子集):标题、有序/
//! 无序列表、引用、分隔线、围栏代码块(syntect 高亮)、行内代码/粗体/斜体/
//! 链接。输出 ratatui `Line`,折行在 span 层完成(样式不因折行丢失)。

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::highlight::Highlighter;
use crate::text::{line_text, wrap_line};
use crate::theme::Theme;

/// 最长不可断词参与列宽下限计算的封顶(pi maxUnbrokenWordWidth)。
const MAX_UNBROKEN_WORD_WIDTH: usize = 30;

/// 表格单元格布局:行内标记渲染后的 spans + 宽度指标。
struct TableCell {
    spans: Vec<Span<'static>>,
    /// 渲染后可见宽
    visible: usize,
    /// 最长不可断词宽(封顶 [`MAX_UNBROKEN_WORD_WIDTH`],下限 1)
    min_word: usize,
}

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
    /// 列宽分配对齐 pi markdown.ts:总自然宽放得下用自然宽;放不下时从各列
    /// 最小宽(最长不可断词,封顶)按伸缩空间加权收缩,舍入余量逐列补齐。
    /// 单元格内容按列宽折行(词边界优先,内容零丢失,行高取各格折行数最大
    /// 值);窄到无法成表时回退渲染原始管道文本。
    fn render_table(&self, rows: &[&str], width: usize) -> Vec<Line<'static>> {
        let border = Style::new().fg(self.theme.md_code_block_border);
        let parsed: Vec<Vec<String>> = rows.iter().map(|r| split_row_cells(r)).collect();
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
        let text_fg = self.theme.assistant_text;
        let header_cell = header
            .map(|h| h.iter().map(|c| self.layout_cell(c, text_fg, true)).collect::<Vec<_>>());
        let body_cell: Vec<Vec<TableCell>> = body
            .iter()
            .map(|r| r.iter().map(|c| self.layout_cell(c, text_fg, false)).collect())
            .collect();
        // 各列自然宽(渲染后可见宽)与最小宽(最长不可断词)
        let mut natural = vec![0usize; ncols];
        let mut min_word = vec![1usize; ncols];
        for row in header_cell.iter().chain(body_cell.iter()) {
            for (j, cell) in row.iter().enumerate() {
                natural[j] = natural[j].max(cell.visible);
                min_word[j] = min_word[j].max(cell.min_word);
            }
        }
        // 边框开销:每列 "│ "(2) + 尾随空格(1),末列后再补一个 "│"(1)
        let border_overhead = 3 * ncols + 1;
        if width.saturating_sub(border_overhead) < ncols {
            // 窄到无法稳定成表:回退渲染原始管道文本(pi 同款兜底)
            return rows
                .iter()
                .flat_map(|r| crate::width::wrap_to_width(r, width.max(1)))
                .map(Line::raw)
                .collect();
        }
        let widths = table_column_widths(width, border_overhead, &natural, &min_word);
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
        let body_style = Style::new().fg(self.theme.assistant_text);
        let header_style = body_style.add_modifier(ratatui::style::Modifier::BOLD);
        let wrap_cells = |cells: &[TableCell]| -> Vec<Vec<Line<'static>>> {
            cells
                .iter()
                .enumerate()
                .map(|(j, cell)| wrap_line(&Line::from(cell.spans.clone()), widths[j]))
                .collect()
        };
        // 行高 = 各格折行数最大值;不足的格留空,逐行按列宽右补齐
        let render_row_lines = |cells: &[Vec<Line<'static>>], pad_style: Style| -> Vec<Line<'static>> {
            let height = cells.iter().map(Vec::len).max().unwrap_or(1);
            (0..height)
                .map(|i| {
                    let mut spans: Vec<Span<'static>> = Vec::new();
                    for (j, w) in widths.iter().enumerate() {
                        spans.push(Span::styled("│ ".to_string(), border));
                        let content = cells
                            .get(j)
                            .and_then(|lines| lines.get(i))
                            .map(|l| l.spans.clone())
                            .unwrap_or_default();
                        let used: usize = content
                            .iter()
                            .map(|s| crate::width::display_width(s.content.as_ref()))
                            .sum();
                        spans.extend(content);
                        let pad = w.saturating_sub(used) + 1;
                        spans.push(Span::styled(" ".repeat(pad), pad_style));
                    }
                    spans.push(Span::styled("│".to_string(), border));
                    Line::from(spans)
                })
                .collect()
        };
        let mut out = vec![hline("╭", "┬", "╮")];
        if let Some(header) = &header_cell {
            out.extend(render_row_lines(&wrap_cells(header), header_style));
            out.push(hline("├", "┼", "┤"));
        }
        for row in &body_cell {
            out.extend(render_row_lines(&wrap_cells(row), body_style));
        }
        out.push(hline("╰", "┴", "╯"));
        out
    }

    /// 单元格布局:行内标记渲染,无前景色的 span 补基础色;header 整格加粗。
    fn layout_cell(&self, raw: &str, fg: ratatui::style::Color, bold: bool) -> TableCell {
        let mut spans = self.render_inline(raw);
        for span in &mut spans {
            if span.style.fg.is_none() {
                span.style.fg = Some(fg);
            }
            if bold {
                span.style = span.style.add_modifier(ratatui::style::Modifier::BOLD);
            }
        }
        let visible = spans
            .iter()
            .map(|s| crate::width::display_width(s.content.as_ref()))
            .sum();
        let plain: String = spans.iter().map(|s| s.content.as_ref()).collect();
        let min_word = plain
            .split_whitespace()
            .map(crate::width::display_width)
            .max()
            .unwrap_or(1)
            .clamp(1, MAX_UNBROKEN_WORD_WIDTH);
        TableCell {
            spans,
            visible,
            min_word,
        }
    }

    /// 代码块:语言标注行 + 高亮正文 + 边框(pi mdCodeBlockBorder)。
    fn render_code_block(
        &self,
        code: &[String],
        lang: Option<&str>,
        width: usize,
    ) -> Vec<Line<'static>> {
        code_block(&code.join("\n"), lang, self.theme, self.highlighter, width)
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

/// 独立代码块渲染(thinking 围栏等无 Markdown 文档上下文的场景复用):
/// 语言标注行 + 高亮正文 + 边框,与 `Markdown::render_code_block` 同款。
pub fn code_block(
    source: &str,
    lang: Option<&str>,
    theme: &Theme,
    highlighter: Option<&Highlighter>,
    width: usize,
) -> Vec<Line<'static>> {
    let border = Style::new().fg(theme.md_code_block_border);
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
    let highlighted = match highlighter {
        Some(highlighter) => highlighter.highlight(source, lang),
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

/// 分隔行判定:所有单元格仅由 `-` 与对齐冒号组成。
fn is_separator(cells: &[String]) -> bool {
    !cells.is_empty()
        && cells.iter().all(|c| {
            let stripped = c.replace(':', "");
            !stripped.is_empty() && stripped.chars().all(|ch| ch == '-')
        })
}

/// 表格切列:按未转义且不在 code span 内的 `|` 切分;`\|` 还原为字面
/// 管道符,code span 内的 `|` 不切分。首尾未转义管道符按定界符剔除。
fn split_row_cells(row: &str) -> Vec<String> {
    let t = row.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = if t.ends_with('|') && !ends_with_escaped_pipe(t) {
        &t[..t.len() - 1]
    } else {
        t
    };
    let chars: Vec<char> = t.chars().collect();
    let mut cells: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_code = false;
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '`' => {
                in_code = !in_code;
                cur.push('`');
                i += 1;
            }
            '|' if !in_code => {
                cells.push(std::mem::take(&mut cur));
                i += 1;
            }
            c => {
                if c == '\\' && !in_code {
                    // 反斜杠串:奇数个且紧跟 | 时,最后一个把 | 转义
                    let run_start = i;
                    while i < chars.len() && chars[i] == '\\' {
                        cur.push('\\');
                        i += 1;
                    }
                    if (i - run_start) % 2 == 1 && chars.get(i) == Some(&'|') {
                        cur.pop();
                        cur.push('|');
                        i += 1;
                    }
                    continue;
                }
                cur.push(c);
                i += 1;
            }
        }
    }
    cells.push(cur);
    cells.into_iter().map(|c| c.trim().to_string()).collect()
}

/// 行尾管道符是否被转义:前置连续反斜杠数为奇数(`\\\|` 的管道不转义)。
/// 反斜杠与管道均为 ASCII,按字节扫描安全。
fn ends_with_escaped_pipe(t: &str) -> bool {
    let bytes = t.as_bytes();
    if bytes.last() != Some(&b'|') {
        return false;
    }
    let mut n = 0usize;
    while n < bytes.len() - 1 && bytes[bytes.len() - 2 - n] == b'\\' {
        n += 1;
    }
    n % 2 == 1
}

/// 表格列宽分配(pi renderTable 对齐):总自然宽放得下用自然宽(不低于最
/// 小词宽);否则从各列最小词宽起,按「自然宽-最小宽」占比分摊余量,舍入
/// 误差逐列补齐(只补到自然宽为止)。调用方保证 available ≥ 列数。
fn table_column_widths(
    width: usize,
    border_overhead: usize,
    natural: &[usize],
    min_word: &[usize],
) -> Vec<usize> {
    let ncols = natural.len();
    let available = width.saturating_sub(border_overhead);
    let min_total: usize = min_word.iter().sum();
    if min_total > available {
        // 最小宽都放不下:按权重(词宽-1)分摊可用宽,每列至少 1;走到这里
        // 必有词宽 > 1(否则 min_total == 列数 ≤ available),max(1) 仅为除零防御
        let remaining = available - ncols;
        let total_weight: usize = min_word
            .iter()
            .map(|w| w.saturating_sub(1))
            .sum::<usize>()
            .max(1);
        let mut widths: Vec<usize> = vec![1; ncols];
        let mut allocated = 0usize;
        for (w, slot) in min_word.iter().zip(widths.iter_mut()) {
            let grow = w.saturating_sub(1) * remaining / total_weight;
            *slot += grow;
            allocated += grow;
        }
        let mut leftover = remaining - allocated;
        let mut j = 0;
        while leftover > 0 {
            widths[j % ncols] += 1;
            leftover -= 1;
            j += 1;
        }
        return widths;
    }
    let natural_total: usize = natural.iter().sum();
    if natural_total + border_overhead <= width {
        return natural
            .iter()
            .zip(min_word)
            .map(|(n, m)| (*n).max(*m))
            .collect();
    }
    // 需收缩:从最小词宽起按伸缩空间占比分摊
    let extra = available - min_total;
    let total_grow: usize = natural
        .iter()
        .zip(min_word)
        .map(|(n, m)| n.saturating_sub(*m))
        .sum();
    let mut widths: Vec<usize> = min_word
        .iter()
        .enumerate()
        .map(|(j, m)| {
            let grow = (natural[j].saturating_sub(*m) * extra)
                .checked_div(total_grow)
                .unwrap_or(0);
            m + grow
        })
        .collect();
    // 舍入余量逐列补齐(未达自然宽为止)
    let mut remaining = available.saturating_sub(widths.iter().sum());
    loop {
        let mut grew = false;
        for (j, w) in widths.iter_mut().enumerate() {
            if remaining > 0 && *w < natural[j] {
                *w += 1;
                remaining -= 1;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    widths
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

    #[test]
    fn table_wraps_long_cells_without_loss() {
        // 列宽受限时长单元格折行不截断:内容零丢失,行高取各格最大折行数
        let long = "这是一段特别长的单元格内容,用来验证表格折行不丢字";
        let out = render(&format!("| A | B |\n|---|---|\n| {long} | x |"), 30);
        let joined: String = out.concat();
        assert!(joined.contains("这是一段特别长的"), "{out:?}");
        assert!(joined.contains("验证表格折行"), "{out:?}");
        assert!(joined.contains('x'), "{out:?}");
        // 顶 + 表头 + 分隔 + 折成多行的正文 + 底
        assert!(out.len() > 6, "{out:?}");
        // 每行不超终端宽度
        for line in &out {
            assert!(
                crate::width::display_width(line) <= 30,
                "line too wide: {line:?}"
            );
        }
    }

    #[test]
    fn table_escaped_pipe_renders_literal() {
        // \| 不切列,还原为字面管道符(顶+表头+分隔+1行正文+底 = 5 行)
        let out = render("| a | b |\n|---|---|\n| x\\|y | z |", 40);
        assert_eq!(out.len(), 5, "{out:?}");
        assert!(out[3].contains("x|y"), "{out:?}");
    }

    #[test]
    fn table_pipe_inside_code_span_does_not_split() {
        let out = render("| a | b |\n|---|---|\n| `x|y` | z |", 40);
        assert!(out[3].contains("x|y"), "{out:?}");
        assert!(!out[3].contains('`'), "{out:?}");
    }

    #[test]
    fn table_inline_markdown_rendered_in_cells() {
        let out = render("| a | b |\n|---|---|\n| **bold** | `code` |", 40);
        let body = &out[3];
        assert!(!body.contains("**") && !body.contains('`'), "{body:?}");
        assert!(body.contains("bold") && body.contains("code"), "{body:?}");
    }

    #[test]
    fn table_too_narrow_falls_back_to_raw_text() {
        // 3 列在 12 宽下放不下(边框开销 10 + 每列至少 1 > 12):回退原始文本
        let out = render("| a | b | c |\n|---|---|---|\n| 1 | 2 | 3 |", 12);
        assert!(!out.iter().any(|l| l.contains('╭')), "{out:?}");
        assert!(out.iter().any(|l| l.contains('|')), "{out:?}");
        for line in &out {
            assert!(
                crate::width::display_width(line) <= 12,
                "line too wide: {line:?}"
            );
        }
    }

    #[test]
    fn column_widths_fit_naturally() {
        let widths = table_column_widths(50, 7, &[5, 20], &[2, 7]);
        assert_eq!(widths, vec![5, 20]);
    }

    #[test]
    fn column_widths_shrink_proportionally() {
        // 自然宽超限:从最小词宽按伸缩空间占比分摊,余量补齐到自然宽
        // available = 43,min_total = 8,extra = 35,total_grow = 42
        // grow = [4, 30],remaining = 1 → 补到第 1 列
        let widths = table_column_widths(50, 7, &[10, 40], &[5, 3]);
        assert_eq!(widths, vec![10, 33]);
        assert!(widths.iter().sum::<usize>() <= 43);
    }

    #[test]
    fn column_widths_floor_at_min_words() {
        // 连最小宽都放不下:按权重分摊,每列至少 1,总宽恰好用满
        let widths = table_column_widths(20, 7, &[30, 30], &[30, 30]);
        assert_eq!(widths.iter().sum::<usize>(), 13);
        assert!(widths.iter().all(|w| *w >= 1));
    }
}
