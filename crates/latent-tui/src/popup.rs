//! 弹窗圆角外框绘制:命令补全/文件补全/选择器面板共用的边框辅助。
//! 边框色取主题 `popup_border` 角色,背景透明(与补全弹窗一致);内容
//! 行截断到内宽并右侧补空格,使右边框逐行对齐。

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::text::{line_width, truncate_line};
use crate::theme::Theme;

/// 把内容行包进圆角外框(总宽 `width`,过窄按 4 处理;两侧边框各占 2 列)。
pub fn frame(content: Vec<Line<'static>>, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let width = width.max(4);
    // 内容量:两侧边框各占 2 列
    let inner_w = width - 4;
    let border = Style::new().fg(theme.popup_border);
    let mut lines = vec![Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(width - 2)),
        border,
    ))];
    for row in content {
        let row = truncate_line(row, inner_w);
        let trailing = " ".repeat(inner_w.saturating_sub(line_width(&row)));
        let mut spans = Vec::with_capacity(row.spans.len() + 3);
        spans.push(Span::styled("│ ".to_string(), border));
        spans.extend(row.spans);
        spans.push(Span::raw(trailing));
        spans.push(Span::styled(" │".to_string(), border));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(width - 2)),
        border,
    )));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::line_text;

    #[test]
    fn frames_content_with_rounded_border() {
        let theme = Theme::dark_ansi();
        let out = frame(vec![Line::raw("hello")], 20, &theme);
        assert_eq!(out.len(), 3);
        assert_eq!(line_text(&out[0]), format!("╭{}╮", "─".repeat(18)));
        assert_eq!(line_text(&out[1]), format!("│ hello{}│", " ".repeat(12)));
        assert_eq!(line_text(&out[2]), format!("╰{}╯", "─".repeat(18)));
        for line in &out {
            assert_eq!(line_width(line), 20);
        }
    }

    #[test]
    fn empty_content_is_borders_only() {
        let out = frame(Vec::new(), 10, &Theme::dark_ansi());
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn overlong_content_is_truncated_to_inner_width() {
        let out = frame(vec![Line::raw("x".repeat(30))], 10, &Theme::dark_ansi());
        assert_eq!(out.len(), 3);
        assert_eq!(line_width(&out[1]), 10);
        assert!(line_text(&out[1]).starts_with("│ xxxxxx │"));
    }

    #[test]
    fn cjk_content_pads_by_display_width() {
        let out = frame(vec![Line::raw("中文")], 12, &Theme::dark_ansi());
        // 中文占 4 列,内宽 8,补 4 空格
        assert_eq!(line_text(&out[1]), format!("│ 中文{} │", " ".repeat(4)));
        assert_eq!(line_width(&out[1]), 12);
    }
}
