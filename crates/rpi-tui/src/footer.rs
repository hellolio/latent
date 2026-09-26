//! 底部状态栏(pi components/footer.ts 的对应物,两行):
//! 1. cwd(~/ 缩写)+(git 分支)· session 名;
//! 2. 左:token 用量与 ctx%(>70% warning 黄、>90% error 红),右:模型 · thinking。

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::text::{join_right, truncate_line};
use crate::theme::Theme;
use crate::width::display_width;

/// footer 渲染所需的数据快照。
#[derive(Debug, Clone, Default)]
pub struct FooterData {
    /// 已做 ~ 缩写的当前目录
    pub cwd: String,
    pub git_branch: Option<String>,
    pub session_label: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_total: f64,
    /// 上下文窗口(0 = 未知,不显示 ctx%)
    pub context_window: u64,
    /// 上下文 token 估计
    pub context_tokens: u64,
    pub model: String,
    /// thinking 级别名
    pub thinking: String,
    /// auto-compact 开启标记(ctx% 后缀 `(auto)`)
    pub auto_compact: bool,
}

/// 两行 footer。
pub fn lines(data: &FooterData, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let width = width.max(1);
    let dim = Style::new().fg(theme.dim);
    let mut first: Vec<Span<'static>> = vec![Span::styled(
        data.cwd.clone(),
        Style::new().fg(theme.footer_cwd),
    )];
    if let Some(branch) = &data.git_branch {
        first.push(Span::styled(format!(" ({branch})"), dim));
    }
    if let Some(session) = &data.session_label {
        first.push(Span::styled(format!(" • {session}"), dim));
    }
    let line1 = truncate_line(Line::from(first), width);

    // 左段:tokens + cost + ctx%
    let mut left: Vec<Span<'static>> = Vec::new();
    if data.input_tokens > 0 || data.output_tokens > 0 {
        left.push(Span::styled(
            format!("↑{} ↓{}", data.input_tokens, data.output_tokens),
            dim,
        ));
        if data.cost_total > 0.0 {
            left.push(Span::styled(format!(" ${:.4}", data.cost_total), dim));
        }
        left.push(Span::raw("  "));
    }
    if data.context_window > 0 && data.context_tokens > 0 {
        let pct = ((data.context_tokens * 100) / data.context_window).min(100);
        let pct_style = if pct > 90 {
            Style::new().fg(theme.error)
        } else if pct > 70 {
            Style::new().fg(theme.warning)
        } else {
            dim
        };
        let auto = if data.auto_compact { " (auto)" } else { "" };
        left.push(Span::styled(
            format!("{pct}%/{}", format_window(data.context_window)),
            pct_style,
        ));
        left.push(Span::styled(auto.to_string(), dim));
    }
    if left.is_empty() {
        left.push(Span::raw(""));
    }

    // 右段:model · thinking
    let right = Line::from(Span::styled(
        format!("{} · t:{}", data.model, data.thinking),
        dim,
    ));
    let mut line2 = join_right(Line::from(left), right, width);
    if display_width(&crate::text::line_text(&line2)) < width {
        // join_right 返回的行可能短于宽度(左段为空等),右对齐补齐
        line2 = pad_to_width(line2, width);
    }
    vec![line1, line2]
}

/// ctx% 段(pi footer 阈值)的独立格式化:0 窗口/无用量返回空。
/// 供状态行等复用;>70% warning、>90% error。
pub fn ctx_segment(window: u64, tokens: u64, theme: &Theme) -> Option<Line<'static>> {
    if window == 0 || tokens == 0 {
        return None;
    }
    let pct = ((tokens * 100) / window).min(100);
    let style = if pct > 90 {
        Style::new().fg(theme.error)
    } else if pct > 70 {
        Style::new().fg(theme.warning)
    } else {
        Style::new().fg(theme.dim)
    };
    Some(Line::from(Span::styled(format!("ctx {pct}%"), style)))
}

/// `$HOME` 前缀 → `~`。
pub fn abbreviate_home(path: &str, home: Option<&str>) -> String {
    match home {
        Some(home) if !home.is_empty() && path.starts_with(home) => {
            let rest = &path[home.len()..];
            format!("~{rest}")
        }
        _ => path.to_string(),
    }
}

fn format_window(window: u64) -> String {
    if window >= 1000 && window.is_multiple_of(1000) {
        format!("{}k", window / 1000)
    } else {
        window.to_string()
    }
}

fn pad_to_width(mut line: Line<'static>, width: usize) -> Line<'static> {
    let current = display_width(&crate::text::line_text(&line));
    if current < width {
        line.spans.push(Span::raw(" ".repeat(width - current)));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::line_text;

    fn theme() -> Theme {
        Theme::dark_ansi()
    }

    fn data() -> FooterData {
        FooterData {
            cwd: "~/work".into(),
            git_branch: Some("main".into()),
            session_label: Some("fix-bug".into()),
            input_tokens: 100,
            output_tokens: 20,
            cost_total: 0.0012,
            context_window: 128_000,
            context_tokens: 1280,
            model: "mock/m1".into(),
            thinking: "high".into(),
            auto_compact: true,
        }
    }

    #[test]
    fn first_line_has_cwd_branch_session() {
        let out = lines(&data(), 60, &theme());
        let first = line_text(&out[0]);
        assert_eq!(first, "~/work (main) • fix-bug", "{first}");
    }

    #[test]
    fn second_line_left_tokens_ctx_right_model() {
        let out = lines(&data(), 60, &theme());
        let second = line_text(&out[1]);
        assert!(second.contains("↑100 ↓20"), "{second}");
        assert!(second.contains("$0.0012"));
        assert!(second.contains("1%/128k (auto)"), "{second}");
        assert!(second.ends_with("mock/m1 · t:high"), "{second}");
        // 右对齐:模型段贴近行尾(行宽补满)
        assert_eq!(display_width(&second), 60);
    }

    #[test]
    fn ctx_threshold_colors() {
        let theme = theme();
        // >90% error
        let line = ctx_segment(1000, 950, &theme).unwrap();
        assert_eq!(line.spans[0].style.fg, Some(theme.error));
        assert!(line_text(&line).contains("95%"));
        // >70% warning
        let line = ctx_segment(1000, 750, &theme).unwrap();
        assert_eq!(line.spans[0].style.fg, Some(theme.warning));
        // 正常 dim
        let line = ctx_segment(1000, 500, &theme).unwrap();
        assert_eq!(line.spans[0].style.fg, Some(theme.dim));
        // 未知窗口/无用量:不显示
        assert!(ctx_segment(0, 100, &theme).is_none());
        assert!(ctx_segment(1000, 0, &theme).is_none());
    }

    #[test]
    fn zero_usage_shows_placeholder_left() {
        let mut d = data();
        d.input_tokens = 0;
        d.output_tokens = 0;
        d.context_tokens = 0;
        let out = lines(&d, 40, &theme());
        // 左段为空:右段贴行尾(左填充 40-16=24 空格)
        let second = line_text(&out[1]);
        assert_eq!(display_width(&second), 40);
        assert!(second.ends_with("mock/m1 · t:high"));
        assert!(second.starts_with(' '));
    }

    #[test]
    fn abbreviate_home_replaces_prefix() {
        assert_eq!(abbreviate_home("/home/kin/a", Some("/home/kin")), "~/a");
        assert_eq!(abbreviate_home("/tmp/x", Some("/home/kin")), "/tmp/x");
        assert_eq!(abbreviate_home("/tmp/x", None), "/tmp/x");
    }
}
