//! 启动区(pi interactive-mode header 的对应物):横幅 + 快捷键提示 +
//! 已加载资源列表。折叠态一行提示,`ctrl+o` 展开完整帮助与资源明细。

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::text::{truncate_line, wrap_line};
use crate::theme::Theme;

/// 横幅 + 提示行(折叠态):
/// `rpi v0.1.0`(accent bold + dim 版本)→ 键位提示 → onboarding 行。
/// 展开态追加快捷键明细与说明。
pub fn banner(version: &str, expanded: bool, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(vec![
        Span::styled(
            "rpi",
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" v{version}"), Style::new().fg(theme.dim)),
    ])];
    // 折叠态一行键位提示(键名 dim · 描述 muted)
    let hint = Line::from(hint_spans(
        &[
            ("esc", "interrupt"),
            ("ctrl+c/ctrl+d", "clear/exit"),
            ("/", "slash 补全"),
            ("!", "bash"),
            ("ctrl+o", "more"),
        ],
        theme,
    ));
    out.extend(wrap_line(&hint, width.max(1)));
    if expanded {
        for (key, desc) in [
            ("enter", "send · shift+enter/ctrl+j newline"),
            ("ctrl+o", "expand/collapse tool output & startup help"),
            ("esc", "interrupt current run"),
            ("ctrl+c", "clear input · press twice to exit"),
            ("ctrl+d", "exit (empty input)"),
            ("/help", "list commands"),
            ("!cmd", "run bash, output joins context"),
            ("!!cmd", "run bash without context"),
        ] {
            out.push(truncate_line(
                Line::from(vec![
                    Span::styled(format!("  {key:<28}"), Style::new().fg(theme.dim)),
                    Span::styled(desc.to_string(), Style::new().fg(theme.muted)),
                ]),
                width,
            ));
        }
    } else {
        out.push(truncate_line(
            Line::from(Span::styled(
                "Press ctrl+o to show full startup help and loaded resources.".to_string(),
                Style::new().fg(theme.dim),
            )),
            width,
        ));
    }
    out
}

/// 已加载资源分节(`[Extensions]` 等;节标题 mdHeading 黄、内容 dim)。
/// 折叠态紧凑逗号列表,展开态逐行。
pub fn resources(
    sections: &[(&str, Vec<String>)],
    expanded: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (title, items) in sections {
        if items.is_empty() {
            continue;
        }
        out.push(truncate_line(
            Line::from(Span::styled(
                format!("[{title}]"),
                Style::new().fg(theme.md_heading),
            )),
            width,
        ));
        if expanded {
            for item in items {
                out.push(truncate_line(
                    Line::from(Span::styled(
                        format!("  {item}"),
                        Style::new().fg(theme.dim),
                    )),
                    width,
                ));
            }
        } else {
            out.push(truncate_line(
                Line::from(Span::styled(
                    format!("  {}", items.join(", ")),
                    Style::new().fg(theme.dim),
                )),
                width,
            ));
        }
    }
    out
}

/// 全宽分隔线(pi 消息区与输入区的视觉分界;用 dim 弱化,不与边框争抢)。
pub fn separator(width: usize, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width.max(1)),
        Style::new().fg(theme.dim),
    ))
}

fn hint_spans(pairs: &[(&str, &str)], theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, (key, desc)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::new().fg(theme.dim)));
        }
        spans.push(Span::styled(key.to_string(), Style::new().fg(theme.dim)));
        spans.push(Span::styled(
            format!(" {desc}"),
            Style::new().fg(theme.muted),
        ));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::line_text;

    fn theme() -> Theme {
        Theme::dark_ansi()
    }

    #[test]
    fn collapsed_banner_is_three_lines() {
        let out = banner("0.1.0", false, 80, &theme());
        assert_eq!(out.len(), 3);
        assert_eq!(line_text(&out[0]), "rpi v0.1.0");
        assert!(line_text(&out[1]).contains("ctrl+o more"));
        assert!(line_text(&out[2]).starts_with("Press ctrl+o"));
    }

    #[test]
    fn expanded_banner_lists_keys() {
        let out = banner("0.1.0", true, 80, &theme());
        assert!(out.len() > 5);
        assert!(out.iter().any(|line| line_text(line).contains("!cmd")));
    }

    #[test]
    fn banner_wraps_to_width() {
        for line in banner("0.1.0", false, 30, &theme()) {
            assert!(crate::width::display_width(&line_text(&line)) <= 30);
        }
    }

    #[test]
    fn resources_sections_skip_empty() {
        let sections = vec![
            ("Skills", vec!["dev-loop".to_string()]),
            ("Extensions", vec![]),
        ];
        let collapsed = resources(&sections, false, 40, &theme());
        assert_eq!(
            collapsed.iter().map(line_text).collect::<Vec<_>>(),
            vec!["[Skills]", "  dev-loop"]
        );
        let expanded = resources(&sections, true, 40, &theme());
        assert_eq!(expanded.len(), 2);
    }

    #[test]
    fn separator_spans_full_width() {
        assert_eq!(line_text(&separator(10, &theme())), "──────────");
    }
}
