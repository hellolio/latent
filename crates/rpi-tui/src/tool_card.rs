//! 工具调用卡片(pi components/tool-execution.ts 的子集):
//! 状态色边框卡片(成功绿/失败红),`⏺ 名称 参数` 命令行与输出同框,
//! 输出默认折叠、ctrl+o 展开。

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::text::line_text;
use crate::theme::Theme;
use crate::width::display_width;

/// 工具执行状态(标题行 `⏺` 与边框着色依据)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    Success,
    Error,
}

impl ToolStatus {
    fn color(&self, theme: &Theme) -> ratatui::style::Color {
        match self {
            ToolStatus::Pending => theme.tool_pending,
            ToolStatus::Success => theme.tool_success,
            ToolStatus::Error => theme.tool_error,
        }
    }
}

/// 默认折叠时保留的输出行数(pi 同款;折叠行 + 1 行余量提示)。
pub const COLLAPSED_OUTPUT_ROWS: usize = 4;

/// 边框横线(╭─…─╮ / ╰─…─╯)。
fn box_edges(width: usize, border_style: Style, left: &str, right: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("{left}{}{right}", "─".repeat(width.saturating_sub(2))),
        border_style,
    ))
}

/// 边框内容行:`│ 文本补齐 │`,整行铺满宽度。
fn box_row(text: &str, style: Style, inner: usize, border_style: Style) -> Line<'static> {
    let pad = inner.saturating_sub(display_width(text));
    Line::from(vec![
        Span::styled("│ ".to_string(), border_style),
        Span::styled(format!("{text}{}", " ".repeat(pad)), style),
        Span::styled(" │".to_string(), border_style),
    ])
}

/// 输出行折叠:展开全量;收起保留前 `COLLAPSED_OUTPUT_ROWS` 行 + 余量提示。
fn collapsed_rows(output: &str, inner: usize, expanded: bool) -> (Vec<String>, Option<String>) {
    let mut rows: Vec<String> = output
        .lines()
        .flat_map(|line| crate::width::wrap_to_width(line, inner))
        .collect();
    let mut hint = None;
    if !rows.is_empty() && !expanded && rows.len() > COLLAPSED_OUTPUT_ROWS {
        let more = rows.len() - COLLAPSED_OUTPUT_ROWS;
        rows.truncate(COLLAPSED_OUTPUT_ROWS);
        hint = Some(format!("  … +{more} lines (ctrl+o to expand)"));
    }
    (rows, hint)
}

/// 工具调用卡片上半:顶部边框 + `⏺ name args` 命令行(命令框进框内)。
/// 输出行与底边由 ToolResult 条目续画,两段拼成同一个框。
/// 收起时参数单行截断预览;ctrl+o 展开时参数完整折行。
pub fn tool_box_top(
    name: &str,
    args: &str,
    status: ToolStatus,
    expanded: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let color = status.color(theme);
    let border_style = Style::new().fg(color);
    let dot_style = Style::new().fg(color);
    let name_style = Style::new()
        .fg(theme.tool_title)
        .add_modifier(Modifier::BOLD);
    let args_style = Style::new().fg(theme.muted);
    let inner = width.saturating_sub(4).max(1);

    let mut out = vec![box_edges(width, border_style, "╭", "╮")];
    let name_w = display_width(name);
    let args_room = inner.saturating_sub(2 + name_w + 1);
    // 首行:⏺ + 工具名 + 参数首段;后续行:参数续段(对齐参数起始列)。
    // 收起时参数单行截断;展开时完整折行;无参数/过窄时只有工具名。
    let chunks: Vec<String> = if !args.is_empty() && args_room >= 4 {
        if expanded {
            crate::width::wrap_to_width(args, args_room)
        } else {
            vec![crate::width::truncate_to_width(args, args_room).0]
        }
    } else {
        Vec::new()
    };
    for (i, chunk) in chunks.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let used = 2 + name_w + 1 + display_width(chunk);
        if i == 0 {
            spans.push(Span::styled("⏺ ".to_string(), dot_style));
            spans.push(Span::styled(name.to_string(), name_style));
            spans.push(Span::styled(" ".to_string(), args_style));
            spans.push(Span::styled(chunk.clone(), args_style));
        } else {
            let indent = format!("{} ", " ".repeat(2 + name_w));
            spans.push(Span::styled(indent, args_style));
            spans.push(Span::styled(chunk.clone(), args_style));
        }
        let pad = inner.saturating_sub(used);
        out.push(Line::from({
            let mut all = vec![Span::styled("│ ".to_string(), border_style)];
            all.extend(spans);
            if pad > 0 {
                all.push(Span::styled(" ".repeat(pad), Style::new()));
            }
            all.push(Span::styled(" │".to_string(), border_style));
            all
        }));
    }
    if chunks.is_empty() {
        let pad = inner.saturating_sub(2 + name_w);
        out.push(Line::from({
            let mut all = vec![
                Span::styled("│ ".to_string(), border_style),
                Span::styled("⏺ ".to_string(), dot_style),
                Span::styled(name.to_string(), name_style),
            ];
            if pad > 0 {
                all.push(Span::styled(" ".repeat(pad), Style::new()));
            }
            all.push(Span::styled(" │".to_string(), border_style));
            all
        }));
    }
    out
}

/// 工具调用卡片下半:输出行(折叠逻辑同 bash)+ 底部边框。
pub fn tool_box_bottom(
    output: &str,
    is_error: bool,
    expanded: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let border = if is_error {
        theme.tool_error
    } else {
        theme.tool_success
    };
    let border_style = Style::new().fg(border);
    let inner = width.saturating_sub(4).max(1);
    let (rows, hint) = collapsed_rows(output, inner, expanded);
    let mut out: Vec<Line<'static>> = rows
        .iter()
        .map(|text| box_row(text, Style::new().fg(theme.user_text), inner, border_style))
        .collect();
    if let Some(hint) = &hint {
        out.push(box_row(hint, Style::new().fg(theme.dim), inner, border_style));
    }
    out.push(box_edges(width, border_style, "╰", "╯"));
    out
}

/// AI 输出卡片:把已渲染的行(如 markdown)包进彩色渐变边框。边框字符按
/// 列位置在主题光谱上取色(左→右渐变,右缘回卷到左缘色);ANSI 兜底
/// (非 Rgb 主题)退化为 accent 单色。内容行须按 `width - 4` 预渲染,
/// 本函数负责边框与行尾补齐。
pub fn box_around(lines: Vec<Line<'static>>, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let width = width.max(4);
    let anchors = theme.gradient_anchors();
    let border_color = |col: usize| {
        let t = if width > 2 {
            col as f32 / (width - 2) as f32 * 0.999
        } else {
            0.0
        };
        crate::theme::gradient_at(&anchors, t).unwrap_or(theme.accent)
    };
    let border_span = |text: &str, col: usize| {
        Span::styled(text.to_string(), Style::new().fg(border_color(col)))
    };
    let inner = width.saturating_sub(4).max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    // 顶边:╭ + ─×(width-2) + ╮,逐字符按列渐变着色
    let mut top = vec![border_span("╭", 0)];
    for col in 1..width - 1 {
        top.push(border_span("─", col));
    }
    top.push(border_span("╮", width - 1));
    out.push(Line::from(top));
    // 内容行:`│ ` + 原样内容 + 行尾补齐 + ` │`
    for line in lines {
        let used = display_width(&line_text(&line));
        let pad = inner.saturating_sub(used);
        let mut spans = vec![border_span("│ ", 0)];
        spans.extend(line.spans);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), Style::new()));
        }
        spans.push(border_span(" │", width - 1));
        out.push(Line::from(spans));
    }
    // 底边:╰…╯(与顶边同色谱)
    let mut bottom = vec![border_span("╰", 0)];
    for col in 1..width - 1 {
        bottom.push(border_span("─", col));
    }
    bottom.push(border_span("╯", width - 1));
    out.push(Line::from(bottom));
    out
}

/// `!` bash 透传卡片:命令与输出同框,状态色边框(成功绿/失败红,正统
/// 状态色不做淡色混合)。折叠逻辑与工具输出一致:保留前
/// `COLLAPSED_OUTPUT_ROWS` 行输出 + 余量提示,ctrl+o 展开全部。
pub fn bash_box(
    command: &str,
    output: &str,
    is_error: bool,
    expanded: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let border_style = Style::new().fg(if is_error {
        theme.tool_error
    } else {
        theme.tool_success
    });
    let command_style = Style::new()
        .fg(theme.assistant_text)
        .add_modifier(Modifier::BOLD);
    let inner = width.saturating_sub(4).max(1);

    // 命令行:首行带 "! " 前缀,超宽折行(展开时完整可见)
    let mut out = vec![box_edges(width, border_style, "╭", "╮")];
    let marked = format!("! {command}");
    let command_rows = crate::width::wrap_to_width(&marked, inner);
    for (i, text) in command_rows.iter().enumerate() {
        if i == 0 {
            let rest = text.strip_prefix("! ").unwrap_or(text);
            let used = 2 + display_width(rest);
            let pad = inner.saturating_sub(used);
            out.push(Line::from(vec![
                Span::styled("│ ".to_string(), border_style),
                Span::styled("! ".to_string(), border_style),
                Span::styled(format!("{rest}{}", " ".repeat(pad)), command_style),
                Span::styled(" │".to_string(), border_style),
            ]));
        } else {
            out.push(box_row(text, command_style, inner, border_style));
        }
    }

    // 输出行:折叠保留前 N 行 + 提示,展开全量(下半含底边)
    let (rows, hint) = collapsed_rows(output, inner, expanded);
    for text in &rows {
        out.push(box_row(text, Style::new().fg(theme.user_text), inner, border_style));
    }
    if let Some(hint) = &hint {
        out.push(box_row(hint, Style::new().fg(theme.dim), inner, border_style));
    }
    out.push(box_edges(width, border_style, "╰", "╯"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::line_text;

    fn theme() -> Theme {
        Theme::dark_ansi()
    }

    #[test]
    fn box_around_encloses_rendered_lines() {
        let t = Theme::dark(); // 真彩色主题,光谱可用
        let content = vec![Line::raw("hello"), Line::raw("world")];
        let lines = box_around(content, 40, &t);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts.len(), 4, "{texts:?}");
        assert!(texts[0].starts_with('╭'), "{texts:?}");
        assert!(texts[0].ends_with('╮'), "{texts:?}");
        assert!(texts.iter().any(|x| x.contains("│ hello")), "{texts:?}");
        assert!(texts.iter().any(|x| x.contains("│ world")), "{texts:?}");
        assert!(texts[3].starts_with('╰'), "{texts:?}");
        // 整行铺满宽度(边框闭合)
        for text in &texts {
            assert_eq!(display_width(text), 40, "{text:?}");
        }
        // 渐变:顶边两端 span 颜色不同(光谱起止色相回卷,中段必不同)
        let top = &lines[0];
        let first = top.spans[0].style.fg;
        let mid = top.spans[top.spans.len() / 2].style.fg;
        assert_ne!(first, mid, "边框应呈渐变而非单色: {top:?}");
    }

    #[test]
    fn box_around_ansi_theme_falls_back_to_accent() {
        let t = Theme::dark_ansi(); // 非 Rgb,光谱不可用
        let lines = box_around(vec![Line::raw("x")], 20, &t);
        for span in &lines[0].spans {
            assert_eq!(span.style.fg, Some(t.accent), "{span:?}");
        }
    }

    #[test]
    fn box_around_content_at_inner_width_keeps_border_closed() {
        let t = Theme::dark_ansi();
        // 内容按 inner = width - 4 预渲染时,边框闭合整行铺满
        let lines = box_around(vec![Line::raw("x".repeat(16))], 20, &t);
        assert_eq!(display_width(&line_text(&lines[1])), 20);
    }

    #[test]
    fn tool_box_top_shows_name_args_and_status_dot() {
        let top = tool_box_top("bash", "ls -la", ToolStatus::Success, false, 40, &theme());
        let text = line_text(&top[1]);
        assert!(text.contains("⏺ bash"), "{text}");
        assert!(text.contains("ls -la"));
        // 顶部边框开框、无底边(由 tool_box_bottom 续画)
        assert!(line_text(&top[0]).starts_with('╭'), "{top:?}");
        assert_eq!(top.len(), 2);
    }

    #[test]
    fn tool_box_top_narrow_omits_args() {
        let top = tool_box_top("bash", "ls", ToolStatus::Pending, false, 12, &theme());
        assert!(!line_text(&top[1]).contains("ls "), "{:?}", top);
    }

    #[test]
    fn tool_box_top_expands_args_multiline() {
        let args = "arg1 arg2 arg3 arg4 arg5";
        let collapsed = tool_box_top("bash", args, ToolStatus::Success, false, 40, &theme());
        assert_eq!(collapsed.len(), 2, "收起时参数单行: {collapsed:?}");
        let expanded = tool_box_top("bash", args, ToolStatus::Success, false, 40, &theme());
        let _ = expanded;
        // 展开时参数折成多行(这里参数足够长)
        let long_args = "word ".repeat(30);
        let expanded = tool_box_top("bash", &long_args, ToolStatus::Success, true, 40, &theme());
        assert!(expanded.len() > 2, "{expanded:?}");
    }

    #[test]
    fn tool_box_bottom_collapses_with_hint_and_expands() {
        let output = (1..=6)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let collapsed = tool_box_bottom(&output, false, false, 40, &theme());
        assert_eq!(collapsed.len(), 6, "{collapsed:?}"); // 4 行 + 提示 + 底边
        assert!(line_text(&collapsed[4]).contains("+2 lines"));
        assert!(line_text(&collapsed[4]).contains("ctrl+o"));
        assert!(line_text(&collapsed[5]).starts_with('╰'));

        let expanded = tool_box_bottom(&output, false, true, 40, &theme());
        assert_eq!(expanded.len(), 7);
        assert!(line_text(&expanded[5]).contains("line6"));
    }

    #[test]
    fn tool_box_bottom_border_color_follows_status() {
        let t = Theme::dark();
        let ok = tool_box_bottom("ok", false, false, 40, &t);
        let err = tool_box_bottom("boom", true, false, 40, &t);
        assert_eq!(ok[0].spans[0].style.fg, Some(t.tool_success));
        assert_eq!(err[0].spans[0].style.fg, Some(t.tool_error));
    }

    #[test]
    fn tool_box_top_and_bottom_form_one_box() {
        let t = Theme::dark();
        let mut lines = tool_box_top("bash", "ls", ToolStatus::Success, false, 40, &t);
        lines.extend(tool_box_bottom("file1", false, false, 40, &t));
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(texts[0].starts_with('╭'), "{texts:?}");
        assert!(texts.last().unwrap().starts_with('╰'), "{texts:?}");
        // 上下两段边框色一致,行宽铺满
        for text in &texts {
            assert_eq!(display_width(text), 40, "{text:?}");
        }
    }

    #[test]
    fn cjk_output_wraps_within_width() {
        let output = "汉".repeat(30);
        let lines = tool_box_bottom(&output, false, true, 20, &theme());
        for line in &lines {
            assert!(display_width(&line_text(line)) <= 20, "{}", line_text(line));
        }
        // 与 wrap_to_width 一致:硬切处不插入空格
        assert!(!lines.iter().any(|line| line_text(line).contains("汉 汉")));
    }

    #[test]
    fn bash_box_encloses_command_and_output() {
        let lines = bash_box("ls -la", "file1\nfile2", false, false, 40, &theme());
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        // 命令在框内,输出同框,上下封边
        assert!(texts[0].starts_with('╭'), "{texts:?}");
        assert!(texts.last().unwrap().starts_with('╰'), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("│ ! ls -la")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("file1")), "{texts:?}");
        // 整行铺满宽度(边框闭合)
        for text in &texts {
            assert_eq!(display_width(text), 40, "{text:?}");
        }
    }

    #[test]
    fn bash_box_border_color_follows_status() {
        let t = Theme::dark();
        let ok = bash_box("ls", "", false, false, 40, &t);
        let err = bash_box("ls", "boom", true, false, 40, &t);
        assert_eq!(ok[0].spans[0].style.fg, Some(t.tool_success));
        assert_eq!(err[0].spans[0].style.fg, Some(t.tool_error));
    }

    #[test]
    fn bash_box_collapses_with_hint_and_expands() {
        let output = (1..=6)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let collapsed = bash_box("ls", &output, false, false, 40, &theme());
        // 顶边 + 命令 1 + 输出 4 + 提示 1 + 底边
        assert_eq!(collapsed.len(), 8, "{:?}", collapsed);
        assert!(
            collapsed
                .iter()
                .any(|l| line_text(l).contains("+2 lines")),
            "{collapsed:?}"
        );
        let expanded = bash_box("ls", &output, false, true, 40, &theme());
        assert_eq!(expanded.len(), 9); // 顶边 + 命令 1 + 输出 6 + 底边
        assert!(expanded.iter().any(|l| line_text(l).contains("line6")));
    }
}
