//! 工具调用卡片(pi components/tool-execution.ts 的子集):
//! `⏺ 名称 参数` 标题行按状态着色,输出默认折叠、ctrl+o 展开。

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::text::truncate_line;
use crate::theme::Theme;
use crate::width::display_width;

/// 工具执行状态(标题行 `⏺` 与正文着色依据)。
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

/// 默认折叠时保留的输出行数(pi 同款)。
pub const COLLAPSED_OUTPUT_ROWS: usize = 3;

/// 标题行:`⏺ name args…`(状态色圆点 + 加粗工具名 + muted 参数预览)。
pub fn title_line(
    name: &str,
    args: &str,
    status: ToolStatus,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let mut spans = vec![
        Span::styled("⏺ ", Style::new().fg(status.color(theme))),
        Span::styled(
            name.to_string(),
            Style::new()
                .fg(theme.tool_title)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let used = 2 + name.chars().count() + 1;
    // 参数预览需要至少 4 列才有意义,否则省略
    if !args.is_empty() && used + 4 <= width {
        let (preview, _) = crate::width::truncate_to_width(args, width - used);
        spans.push(Span::raw(" "));
        spans.push(Span::styled(preview, Style::new().fg(theme.muted)));
    }
    truncate_line(Line::from(spans), width)
}

/// 工具输出正文:折叠时保留前 COLLAPSED_OUTPUT_ROWS 行 + 余量提示;
/// 展开时全部折行输出。空输出返回空。
/// 整行铺淡色背景(用户块背景上叠 14% 状态色:成功浅绿/失败浅红),
/// 正文保持可读的 user_text 色,背景只做状态暗示。
pub fn output_lines(
    output: &str,
    status: ToolStatus,
    expanded: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let bg = crate::theme::blend_rgb(theme.user_bg, status.color(theme), 0.14);
    let body_style = Style::new().fg(theme.user_text).bg(bg);
    let hint_style = Style::new().fg(theme.dim).bg(bg);
    let inner = width.saturating_sub(4).max(1);
    let mut wrapped: Vec<String> = Vec::new();
    for raw in output.lines() {
        for line in crate::width::wrap_to_width(raw, inner) {
            wrapped.push(line);
        }
    }
    if wrapped.is_empty() {
        return Vec::new();
    }
    let render_row = |text: &str, style: Style| -> Line<'static> {
        // 内容 + 行尾补齐:整行铺满背景(无终端底色缝隙)
        let pad = width.saturating_sub(display_width(text));
        Line::from(Span::styled(
            format!("{text}{}", " ".repeat(pad)),
            style,
        ))
    };
    if expanded || wrapped.len() <= COLLAPSED_OUTPUT_ROWS {
        let mut out: Vec<Line<'static>> = wrapped
            .iter()
            .map(|row| render_row(row, body_style))
            .collect();
        // 折叠阈值内的完整输出后附展开提示(仅当确实可折叠时)
        if !expanded && wrapped.len() > COLLAPSED_OUTPUT_ROWS {
            out.push(hint_line(wrapped.len(), width, hint_style));
        }
        return out;
    }
    let mut out: Vec<Line<'static>> = wrapped[..COLLAPSED_OUTPUT_ROWS]
        .iter()
        .map(|row| render_row(row, body_style))
        .collect();
    out.push(hint_line(wrapped.len(), width, hint_style));
    out
}

fn hint_line(total: usize, width: usize, style: Style) -> Line<'static> {
    let more = total.saturating_sub(COLLAPSED_OUTPUT_ROWS);
    let text = format!("  … +{more} lines (ctrl+o to expand)");
    let pad = width.saturating_sub(display_width(&text));
    Line::from(Span::styled(
        format!("{text}{}", " ".repeat(pad)),
        style,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::line_text;

    fn theme() -> Theme {
        Theme::dark_ansi()
    }

    #[test]
    fn title_line_shows_name_args_and_status_dot() {
        let line = title_line("bash", "ls -la", ToolStatus::Success, 40, &theme());
        let text = line_text(&line);
        assert!(text.starts_with("⏺ bash"), "{text}");
        assert!(text.contains("ls -la"));
        // 参数预览 muted(与工具名不同色)
        assert_ne!(line.spans[1].style, line.spans[3].style);
    }

    #[test]
    fn title_line_narrow_omits_args() {
        let line = title_line("bash", "ls", ToolStatus::Pending, 8, &theme());
        assert_eq!(line_text(&line), "⏺ bash");
    }

    #[test]
    fn output_collapses_with_hint_and_expands() {
        let output = (1..=6)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let collapsed = output_lines(&output, ToolStatus::Success, false, 40, &theme());
        assert_eq!(collapsed.len(), 4, "{collapsed:?}"); // 3 行 + 提示
        assert!(line_text(&collapsed[3]).contains("+3 lines"));
        assert!(line_text(&collapsed[3]).contains("ctrl+o"));

        let expanded = output_lines(&output, ToolStatus::Success, true, 40, &theme());
        assert_eq!(expanded.len(), 6);
        assert!(line_text(&expanded[5]).contains("line6"));
    }

    #[test]
    fn short_output_needs_no_hint() {
        let collapsed = output_lines("a\nb", ToolStatus::Success, false, 40, &theme());
        assert_eq!(collapsed.len(), 2);
    }

    #[test]
    fn output_rows_tinted_by_status() {
        // 真彩色主题:失败浅红背景、成功浅绿背景(用户块背景叠 14% 状态色),
        // 正文 fg 保持 user_text
        let t = Theme::dark();
        let err = output_lines("boom", ToolStatus::Error, false, 40, &t);
        let expected_err = crate::theme::blend_rgb(t.user_bg, t.tool_error, 0.14);
        assert_eq!(err[0].spans[0].style.bg, Some(expected_err));
        assert_eq!(err[0].spans[0].style.fg, Some(t.user_text));
        let ok = output_lines("ok", ToolStatus::Success, false, 40, &t);
        let expected_ok = crate::theme::blend_rgb(t.user_bg, t.tool_success, 0.14);
        assert_eq!(ok[0].spans[0].style.bg, Some(expected_ok));
        // 整行铺满背景(内容 + 行尾补齐)
        assert_eq!(crate::width::display_width(&line_text(&ok[0])), 40);
        // ANSI 兜底主题(非 Rgb 背景色)不调色,保持原背景
        let ansi = output_lines("boom", ToolStatus::Error, false, 40, &theme());
        assert_eq!(ansi[0].spans[0].style.bg, Some(theme().user_bg));
    }

    #[test]
    fn cjk_output_wraps_within_width() {
        let output = "汉".repeat(30);
        let lines = output_lines(&output, ToolStatus::Success, true, 20, &theme());
        for line in &lines {
            assert!(display_width(&line_text(line)) <= 20, "{}", line_text(line));
        }
        // 与 wrap_to_width 一致:硬切处不插入空格
        assert!(!lines.iter().any(|line| line_text(line).contains("汉 汉")));
    }
}
