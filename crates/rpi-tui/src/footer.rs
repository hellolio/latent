//! 底部状态栏(pi components/footer.ts 的对应物,三行):
//! 1. cwd(~/ 缩写)+(git 分支)· session 名;
//! 2. 右对齐 token 段:↑in │ ↓out │ cache 命中率 │ ctx%(>70% warning 黄、
//!    >90% error 红)· 用量/窗口 │ $cost,每段独立着色;
//! 3. 右对齐模型 · thinking。

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::text::{line_text, truncate_line};
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
    /// 累计缓存读 token(命中率分子)
    pub cache_read: u64,
    /// 累计缓存写 token(命中率分母的一部分)
    pub cache_write: u64,
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

/// 三行 footer。
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

    let line2 = right_align(usage_line(data, theme), width);
    let line3 = right_align(
        Line::from(Span::styled(
            format!("{} · t:{}", data.model, data.thinking),
            Style::new().fg(theme.muted),
        )),
        width,
    );
    vec![line1, line2, line3]
}

/// token 用量段(右对齐显示;session 累计):↑in │ ↓out │ cache 命中率 │
/// ctx 用量 │ $cost。各段独立着色,分隔符 dim。
fn usage_line(data: &FooterData, theme: &Theme) -> Line<'static> {
    let dim = Style::new().fg(theme.dim);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let push = |spans: &mut Vec<Span<'static>>, sep: &mut bool, span: Span<'static>| {
        if *sep {
            spans.push(Span::styled(" │ ".to_string(), dim));
        }
        *sep = true;
        spans.push(span);
    };
    let mut sep = false;
    if data.input_tokens > 0 || data.output_tokens > 0 {
        push(
            &mut spans,
            &mut sep,
            Span::styled(
                format!("↑ {}", format_tokens(data.input_tokens)),
                Style::new().fg(theme.usage_input),
            ),
        );
        push(
            &mut spans,
            &mut sep,
            Span::styled(
                format!("↓ {}", format_tokens(data.output_tokens)),
                Style::new().fg(theme.usage_output),
            ),
        );
        // 命中率 = cache_read / (input + cache_read + cache_write)
        // (prompt 全量;input 为未缓存部分)。只要有用量就显示,0% 也显示。
        let prompt_total = data.input_tokens + data.cache_read + data.cache_write;
        if let Some(hit) = (data.cache_read * 100).checked_div(prompt_total) {
            push(
                &mut spans,
                &mut sep,
                Span::styled(
                    format!("cache {hit}%"),
                    Style::new().fg(theme.usage_cache),
                ),
            );
        }
        if data.cost_total > 0.0 {
            push(
                &mut spans,
                &mut sep,
                Span::styled(
                    format!("${:.4}", data.cost_total),
                    Style::new().fg(theme.usage_cost),
                ),
            );
        }
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
        push(
            &mut spans,
            &mut sep,
            Span::styled(
                format!(
                    "ctx {pct}% ({}/{})",
                    format_tokens(data.context_tokens),
                    format_window(data.context_window)
                ),
                pct_style,
            ),
        );
        if !auto.is_empty() {
            spans.push(Span::styled(auto.to_string(), dim));
        }
    }
    if spans.is_empty() {
        spans.push(Span::raw(""));
    }
    Line::from(spans)
}

/// 右对齐:左填充空格补满宽度;超宽时右侧截断。
fn right_align(line: Line<'static>, width: usize) -> Line<'static> {
    let line = truncate_line(line, width);
    let current = display_width(&line_text(&line));
    if current < width {
        let mut spans = vec![Span::raw(" ".repeat(width - current))];
        spans.extend(line.spans);
        return Line::from(spans);
    }
    line
}

/// token 数紧凑格式:≥1000 显示 k(1100 → 1.1k,128000 → 128k)。
fn format_tokens(n: u64) -> String {
    if n >= 1000 {
        let k = n as f64 / 1000.0;
        let formatted = format!("{k:.1}");
        let formatted = formatted.strip_suffix(".0").unwrap_or(&formatted);
        format!("{formatted}k")
    } else {
        n.to_string()
    }
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
            input_tokens: 11_000,
            output_tokens: 149,
            cache_read: 5_500,
            cache_write: 2_200,
            cost_total: 0.0012,
            context_window: 128_000,
            context_tokens: 12_800,
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
    fn three_lines_usage_right_model_right() {
        let out = lines(&data(), 80, &theme());
        assert_eq!(out.len(), 3);
        let second = line_text(&out[1]);
        // 右对齐 token 段:紧凑 k 格式 + 命中率 + ctx + cost
        assert!(second.contains("↑ 11k"), "{second}");
        assert!(second.contains("↓ 149"), "{second}");
        assert!(second.contains("cache 29%"), "{second}");
        assert!(second.contains("ctx 10% (12.8k/128k) (auto)"), "{second}");
        assert!(second.contains("$0.0012"), "{second}");
        assert_eq!(display_width(&second), 80);
        // 第三行右对齐模型
        let third = line_text(&out[2]);
        assert!(third.ends_with("mock/m1 · t:high"), "{third}");
        assert_eq!(display_width(&third), 80);
    }

    #[test]
    fn usage_colors_are_distinct() {
        let out = lines(&data(), 80, &theme());
        let theme = theme();
        let second = &out[1];
        let input_span = second
            .spans
            .iter()
            .find(|s| s.content.starts_with("↑ "))
            .unwrap();
        assert_eq!(input_span.style.fg, Some(theme.usage_input));
        let output_span = second
            .spans
            .iter()
            .find(|s| s.content.starts_with("↓ "))
            .unwrap();
        assert_eq!(output_span.style.fg, Some(theme.usage_output));
        let cost_span = second.spans.iter().find(|s| s.content.starts_with('$')).unwrap();
        assert_eq!(cost_span.style.fg, Some(theme.usage_cost));
        // 模型行用 muted,与 token 段区分
        let model_span = out[2]
            .spans
            .iter()
            .find(|s| s.content.contains("mock/m1"))
            .unwrap();
        assert_eq!(model_span.style.fg, Some(theme.muted));
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
    fn zero_usage_shows_empty_usage_line_right_aligned() {
        let mut d = data();
        d.input_tokens = 0;
        d.output_tokens = 0;
        d.cache_read = 0;
        d.cache_write = 0;
        d.context_tokens = 0;
        d.cost_total = 0.0;
        let out = lines(&d, 40, &theme());
        assert_eq!(out.len(), 3);
        // 无用量:第二行为空(仍占行,footer 高度稳定),第三行右对齐模型
        assert!(line_text(&out[1]).trim().is_empty());
        let third = line_text(&out[2]);
        assert!(third.ends_with("mock/m1 · t:high"), "{third}");
        assert_eq!(display_width(&third), 40);
    }

    #[test]
    fn cache_hit_shown_even_at_zero_percent() {
        let mut d = data();
        d.cache_read = 0;
        let out = lines(&d, 80, &theme());
        let second = line_text(&out[1]);
        assert!(second.contains("cache 0%"), "{second}");
    }

    #[test]
    fn abbreviate_home_replaces_prefix() {
        assert_eq!(abbreviate_home("/home/kin/a", Some("/home/kin")), "~/a");
        assert_eq!(abbreviate_home("/tmp/x", Some("/home/kin")), "/tmp/x");
        assert_eq!(abbreviate_home("/tmp/x", None), "/tmp/x");
    }

    #[test]
    fn format_tokens_compact() {
        assert_eq!(format_tokens(149), "149");
        assert_eq!(format_tokens(11_000), "11k");
        assert_eq!(format_tokens(1_100), "1.1k");
        assert_eq!(format_tokens(128_000), "128k");
    }
}
