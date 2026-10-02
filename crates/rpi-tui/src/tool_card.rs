//! 工具调用卡片(pi components/tool-execution.ts 的子集):
//! 状态背景色卡片(成功绿/失败红/运行中中性色,无外框),`⏺ 名称 参数`
//! 命令行与输出同块,输出默认折叠、ctrl+o 展开。

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::text::line_text;
use crate::theme::Theme;
use crate::width::display_width;

/// 工具执行状态(标题行 `⏺` 前景与卡片背景的取色依据)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    Success,
    Error,
}

impl ToolStatus {
    fn color(&self, theme: &Theme) -> Color {
        match self {
            ToolStatus::Pending => theme.tool_pending,
            ToolStatus::Success => theme.tool_success,
            ToolStatus::Error => theme.tool_error,
        }
    }

    /// 卡片背景色(pi toolPendingBg/toolSuccessBg/toolErrorBg)。
    fn bg(&self, theme: &Theme) -> Color {
        match self {
            ToolStatus::Pending => theme.tool_pending_bg,
            ToolStatus::Success => theme.tool_success_bg,
            ToolStatus::Error => theme.tool_error_bg,
        }
    }
}

/// 默认折叠时保留的输出行数(pi 同款;折叠行 + 1 行余量提示)。
pub const COLLAPSED_OUTPUT_ROWS: usize = 4;

/// 背景色块:每行所有 span 就地铺上 `bg`,并在行尾补齐空格铺满整行宽度
/// (与 user_block 同手法,保证块内背景无终端底色缝隙)。
pub fn bg_block(lines: Vec<Line<'static>>, width: usize, bg: Color) -> Vec<Line<'static>> {
    let width = width.max(1);
    lines
        .into_iter()
        .map(|line| {
            let used = display_width(&line_text(&line));
            let pad = width.saturating_sub(used);
            let mut spans: Vec<Span<'static>> = line
                .spans
                .into_iter()
                .map(|mut span| {
                    span.style = span.style.bg(bg);
                    span
                })
                .collect();
            if pad > 0 {
                spans.push(Span::styled(" ".repeat(pad), Style::new().bg(bg)));
            }
            Line::from(spans)
        })
        .collect()
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

/// 工具调用卡片标题段:`⏺ name args` 命令行(整行铺状态背景色)。
/// 输出行由 ToolResult 条目续画,两段拼成同一个背景块。
/// 收起时参数单行截断预览;ctrl+o 展开时参数完整折行。
pub fn tool_box_top(
    name: &str,
    args: &str,
    status: ToolStatus,
    expanded: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let dot_style = Style::new().fg(status.color(theme));
    let name_style = Style::new()
        .fg(theme.tool_title)
        .add_modifier(Modifier::BOLD);
    // 命令参数用正文白(muted 过暗,暗色终端上不易辨认)
    let args_style = Style::new().fg(theme.assistant_text);

    let name_w = display_width(name);
    // 参数可用宽度 = 整行 - 「⏺ 」 - 工具名 - 间隔空格
    let args_room = width.saturating_sub(2 + name_w + 1);
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
    let mut out: Vec<Line<'static>> = Vec::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::new();
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
        out.push(Line::from(spans));
    }
    if chunks.is_empty() {
        out.push(Line::from(vec![
            Span::styled("⏺ ".to_string(), dot_style),
            Span::styled(name.to_string(), name_style),
        ]));
    }
    bg_block(out, width, status.bg(theme))
}

/// 工具调用卡片输出段:输出行整行铺成功/失败背景色(折叠逻辑同 bash),
/// 与标题段拼成同一个背景块。
pub fn tool_box_bottom(
    output: &str,
    is_error: bool,
    expanded: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let bg = if is_error {
        theme.tool_error_bg
    } else {
        theme.tool_success_bg
    };
    let (rows, hint) = collapsed_rows(output, width, expanded);
    let mut lines: Vec<Line<'static>> = rows
        .into_iter()
        .map(|text| Line::from(Span::styled(text, Style::new().fg(theme.tool_output))))
        .collect();
    if let Some(hint) = hint {
        lines.push(Line::from(Span::styled(hint, Style::new().fg(theme.dim))));
    }
    bg_block(lines, width, bg)
}

/// `!` bash 透传卡片:命令与输出同块,状态背景色(成功绿/失败红)。
/// 折叠逻辑与工具输出一致:保留前 `COLLAPSED_OUTPUT_ROWS` 行输出 +
/// 余量提示,ctrl+o 展开全部。
pub fn bash_box(
    command: &str,
    output: &str,
    is_error: bool,
    expanded: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let bg = if is_error {
        theme.tool_error_bg
    } else {
        theme.tool_success_bg
    };
    let bang_style = Style::new().fg(if is_error {
        theme.tool_error
    } else {
        theme.tool_success
    });
    let command_style = Style::new()
        .fg(theme.assistant_text)
        .add_modifier(Modifier::BOLD);

    // 命令行:首行带 "! " 前缀,超宽折行(展开时完整可见)
    let marked = format!("! {command}");
    let command_rows = crate::width::wrap_to_width(&marked, width);
    let mut out: Vec<Line<'static>> = Vec::new();
    for text in &command_rows {
        if let Some(rest) = text.strip_prefix("! ") {
            out.push(Line::from(vec![
                Span::styled("! ".to_string(), bang_style),
                Span::styled(rest.to_string(), command_style),
            ]));
        } else {
            out.push(Line::from(Span::styled(
                text.clone(),
                command_style,
            )));
        }
    }

    // 输出行:折叠保留前 N 行 + 提示,展开全量
    let (rows, hint) = collapsed_rows(output, width, expanded);
    for text in &rows {
        out.push(Line::from(Span::styled(
            text.clone(),
            Style::new().fg(theme.tool_output),
        )));
    }
    if let Some(hint) = &hint {
        out.push(Line::from(Span::styled(hint.clone(), Style::new().fg(theme.dim))));
    }
    bg_block(out, width, bg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::line_text;

    fn theme() -> Theme {
        Theme::dark_ansi()
    }

    #[test]
    fn bg_block_pads_full_width_and_patches_spans() {
        let bg = Color::Rgb(0x25, 0x41, 0x31);
        let content = vec![Line::from(vec![
            Span::styled("he", Style::new().fg(Color::White)),
            Span::raw("llo"),
        ])];
        let lines = bg_block(content, 40, bg);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts.len(), 1);
        assert!(texts[0].starts_with("hello"));
        // 整行铺满宽度(含行尾补齐)
        assert_eq!(display_width(&texts[0]), 40);
        // 所有 span(含补齐)都铺上 bg
        for span in &lines[0].spans {
            assert_eq!(span.style.bg, Some(bg), "{span:?}");
        }
    }

    #[test]
    fn bg_block_keeps_narrower_than_width() {
        let lines = bg_block(vec![Line::raw("x")], 4, Color::Red);
        assert_eq!(display_width(&line_text(&lines[0])), 4);
    }

    #[test]
    fn tool_box_top_shows_name_args_and_status_dot() {
        let top = tool_box_top("bash", "ls -la", ToolStatus::Success, false, 40, &theme());
        let text = line_text(&top[0]);
        assert!(text.contains("⏺ bash"), "{text}");
        assert!(text.contains("ls -la"));
        // 无边框:任何行都不再以 ╭ 开头
        assert!(!top.iter().any(|l| line_text(l).starts_with('╭')));
        // 标题行整行铺成功背景色
        assert_eq!(top.len(), 1);
        for span in &top[0].spans {
            assert_eq!(span.style.bg, Some(theme().tool_success_bg), "{span:?}");
        }
        // ⏺ 前景保留状态色
        assert_eq!(top[0].spans[0].style.fg, Some(theme().tool_success));
    }

    #[test]
    fn tool_box_top_narrow_omits_args() {
        // 整行铺满后参数可用宽度 = width - (2 + 名称宽 + 1);宽 10 时 < 4,
        // 参数不渲染
        let top = tool_box_top("bash", "ls", ToolStatus::Pending, false, 10, &theme());
        assert!(!line_text(&top[0]).contains("ls"), "{:?}", top);
        assert_eq!(top[0].spans[0].style.bg, Some(theme().tool_pending_bg));
    }

    #[test]
    fn tool_box_top_expands_args_multiline() {
        let args = "arg1 arg2 arg3 arg4 arg5";
        let collapsed = tool_box_top("bash", args, ToolStatus::Success, false, 40, &theme());
        assert_eq!(collapsed.len(), 1, "收起时参数单行: {collapsed:?}");
        // 展开时参数折成多行(这里参数足够长)
        let long_args = "word ".repeat(30);
        let expanded = tool_box_top("bash", &long_args, ToolStatus::Success, true, 40, &theme());
        assert!(expanded.len() > 1, "{expanded:?}");
        // 续行整行铺背景
        for line in &expanded {
            for span in &line.spans {
                assert_eq!(span.style.bg, Some(theme().tool_success_bg), "{span:?}");
            }
        }
    }

    #[test]
    fn tool_box_bottom_collapses_with_hint_and_expands() {
        let output = (1..=6)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let collapsed = tool_box_bottom(&output, false, false, 40, &theme());
        assert_eq!(collapsed.len(), 5, "{collapsed:?}"); // 4 行 + 提示(无底边框)
        assert!(line_text(&collapsed[4]).contains("+2 lines"));
        assert!(line_text(&collapsed[4]).contains("ctrl+o"));

        let expanded = tool_box_bottom(&output, false, true, 40, &theme());
        assert_eq!(expanded.len(), 6);
        assert!(line_text(&expanded[5]).contains("line6"));
    }

    #[test]
    fn tool_box_bottom_bg_follows_status() {
        let t = Theme::dark();
        let ok = tool_box_bottom("ok", false, false, 40, &t);
        let err = tool_box_bottom("boom", true, false, 40, &t);
        assert_eq!(ok[0].spans[0].style.bg, Some(t.tool_success_bg));
        assert_eq!(err[0].spans[0].style.bg, Some(t.tool_error_bg));
    }

    #[test]
    fn tool_box_top_and_bottom_form_one_block() {
        let t = Theme::dark();
        let mut lines = tool_box_top("bash", "ls", ToolStatus::Success, false, 40, &t);
        lines.extend(tool_box_bottom("file1", false, false, 40, &t));
        // 无边框:首行不以 ╭ 开头、末行不以 ╰ 开头
        assert!(!line_text(&lines[0]).starts_with('╭'));
        assert!(!line_text(lines.last().unwrap()).starts_with('╰'));
        // 所有行宽铺满(背景块闭合)
        for text in lines.iter().map(line_text) {
            assert_eq!(display_width(&text), 40, "{text:?}");
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
        // 命令与输出同块,无上下封边
        assert!(texts[0].starts_with("! ls -la"), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("file1")), "{texts:?}");
        // 整行铺满宽度(背景块闭合)
        for text in &texts {
            assert_eq!(display_width(text), 40, "{text:?}");
        }
    }

    #[test]
    fn bash_box_bg_follows_status() {
        let t = Theme::dark();
        let ok = bash_box("ls", "", false, false, 40, &t);
        let err = bash_box("ls", "boom", true, false, 40, &t);
        assert_eq!(ok[0].spans[0].style.bg, Some(t.tool_success_bg));
        assert_eq!(err[0].spans[0].style.bg, Some(t.tool_error_bg));
        // `!` 前缀保留状态色前景
        assert_eq!(err[0].spans[0].style.fg, Some(t.tool_error));
    }

    #[test]
    fn bash_box_collapses_with_hint_and_expands() {
        let output = (1..=6)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let collapsed = bash_box("ls", &output, false, false, 40, &theme());
        // 命令 1 + 输出 4 + 提示 1
        assert_eq!(collapsed.len(), 6, "{:?}", collapsed);
        assert!(
            collapsed
                .iter()
                .any(|l| line_text(l).contains("+2 lines")),
            "{collapsed:?}"
        );
        let expanded = bash_box("ls", &output, false, true, 40, &theme());
        assert_eq!(expanded.len(), 7); // 命令 1 + 输出 6
        assert!(expanded.iter().any(|l| line_text(l).contains("line6")));
    }
}
