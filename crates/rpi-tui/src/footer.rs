//! 底部状态栏(pi components/footer.ts 的对应物,三行):
//! 1. cwd(~/ 缩写)+(git 分支);
//! 2. 右对齐 token 段:↑prompt(缓存明细 U/R 与命中率) │ ↓out │ ctx%
//!    (>70% warning 黄、>90% error 红)· 用量/窗口 │ $cost,每段独立着色;
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
    /// 会话模式标记(13 文档 §10.3:plan 黄 / confirm 默认 / full-access 红)
    pub mode: Option<String>,
    /// ctrl+o 全局展开态(第一行追加 `· expanded` 提示)
    pub expanded: bool,
    /// 存活的后台 subagent 运行数(0 = 不显示)
    pub subagent_active: usize,
    /// 当前激活的平行子 agent(/subagent 切换;None = 主会话)
    pub active_agent: Option<String>,
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
    if data.expanded {
        first.push(Span::styled(" · expanded", dim));
    }
    let line1 = truncate_line(Line::from(first), width);

    let line2 = right_align(usage_line(data, theme), width);
    let mut line3_spans: Vec<Span<'static>> = Vec::new();
    if let Some(mode) = &data.mode {
        // 模式标记:full-access 红色警示,plan 黄色提醒只读
        let style = match mode.as_str() {
            "full-access" => Style::new().fg(theme.error),
            "plan" => Style::new().fg(theme.warning),
            _ => Style::new().fg(theme.dim),
        };
        line3_spans.push(Span::styled(format!("[{mode}] "), style));
    }
    line3_spans.push(Span::styled(
        format!("{} · t:{}", data.model, data.thinking),
        // 模型行与 cwd 同色(此前 muted 过暗,与背景区分度不足)
        Style::new().fg(theme.footer_cwd),
    ));
    if let Some(agent) = &data.active_agent {
        line3_spans.push(Span::styled(
            format!(" · agent:{agent}"),
            Style::new().fg(theme.warning),
        ));
    }
    if data.subagent_active > 0 {
        // 后台 subagent 运行数(14 文档 §4.3 进度可见:footer 状态段,不做活组件)
        line3_spans.push(Span::styled(
            format!(" · ⏷{} subagent", data.subagent_active),
            Style::new().fg(theme.warning),
        ));
    }
    let line3 = right_align(Line::from(line3_spans), width);
    vec![line1, line2, line3]
}

/// token 用量段(右对齐显示;session 累计):↑prompt(hit/miss 明细) │
/// ctx 用量 │ $cost。各段独立着色,分隔符 dim;零用量也显示 ↑ 0 │ ↓ 0。
fn usage_line(data: &FooterData, theme: &Theme) -> Line<'static> {
    let dim = Style::new().fg(theme.dim);
    let mut spans = usage_segments(data, theme);
    let ctx_pct = data
        .context_tokens
        .checked_mul(100)
        .and_then(|total| total.checked_div(data.context_window))
        .map(|pct| pct.min(100));
    if let Some(pct) = ctx_pct {
        let pct_style = if pct > 90 {
            Style::new().fg(theme.error)
        } else if pct > 70 {
            Style::new().fg(theme.warning)
        } else {
            Style::new().fg(theme.usage_ctx)
        };
        let auto = if data.auto_compact { " (auto)" } else { "" };
        if !spans.is_empty() {
            spans.push(Span::styled(" │ ".to_string(), dim));
        }
        spans.push(Span::styled(
            format!(
                "ctx {pct}% ({}/{})",
                format_tokens(data.context_tokens),
                format_window(data.context_window)
            ),
            pct_style,
        ));
        if !auto.is_empty() {
            spans.push(Span::styled(auto.to_string(), dim));
        }
    }
    if spans.is_empty() {
        spans.push(Span::raw(""));
    }
    Line::from(spans)
}

/// ↑prompt(hit/miss) │ ↓out │ $cost 分段(footer 与转录单回合用量行共用)。
/// 零用量也显示 `↑ 0 │ ↓ 0`(footer 要求 token 段常驻)。
fn usage_segments(data: &FooterData, theme: &Theme) -> Vec<Span<'static>> {
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
    {
        // ↑ 显示完整 prompt 规模(input + cache 读 + cache 写),括号内给出
        // 缓存明细与命中率:U = input + cache_write(未命中,本次新发送),
        // R = cache_read(命中),命中率 = R / ↑ 总量。
        // 无缓存读写(如不带 prompt cache 的 provider)时退化为裸 ↑ input。
        let prompt_total = data.input_tokens + data.cache_read + data.cache_write;
        let miss_total = data.input_tokens + data.cache_write;
        let up = if data.cache_read + data.cache_write > 0 {
            let hit_pct = (data.cache_read * 100).checked_div(prompt_total).unwrap_or(0);
            format!(
                "↑ {} (U {} / R {} · {}%)",
                format_tokens(prompt_total),
                format_tokens(miss_total),
                format_tokens(data.cache_read),
                hit_pct
            )
        } else {
            format!("↑ {}", format_tokens(data.input_tokens))
        };
        push(
            &mut spans,
            &mut sep,
            Span::styled(up, Style::new().fg(theme.usage_input)),
        );
        // 命中/未命中明细随 ↑ 段的括号展示(替代旧 cache N% 段:数量信息
        // 更完整且 80 列内放得下);无缓存读写的 provider 不展示
        push(
            &mut spans,
            &mut sep,
            Span::styled(
                format!("↓ {}", format_tokens(data.output_tokens)),
                Style::new().fg(theme.usage_output),
            ),
        );
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
    spans
}

/// 单回合用量行(转录定稿后落盘):图标与配色与 footer 右侧 token 段一致,
/// 额外带 reasoning 段(footer 不显示)。
pub fn turn_usage_line(
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    reasoning: Option<u64>,
    cost_total: f64,
    theme: &Theme,
) -> Line<'static> {
    let dim = Style::new().fg(theme.dim);
    let data = FooterData {
        input_tokens: input,
        output_tokens: output,
        cache_read,
        cache_write,
        cost_total,
        ..FooterData::default()
    };
    let mut spans = usage_segments(&data, theme);
    if let Some(reasoning) = reasoning.filter(|r| *r > 0) {
        if !spans.is_empty() {
            spans.push(Span::styled(" │ ".to_string(), dim));
        }
        spans.push(Span::styled(
            format!("reasoning {}", format_tokens(reasoning)),
            Style::new().fg(theme.usage_reasoning),
        ));
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
        Style::new().fg(theme.usage_ctx)
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
            expanded: false,
            subagent_active: 0,
            active_agent: None,
            mode: Some("confirm".into()),
        }
    }

    #[test]
    fn first_line_has_cwd_branch() {
        let out = lines(&data(), 60, &theme());
        let first = line_text(&out[0]);
        assert_eq!(first, "~/work (main)", "{first}");
    }

    #[test]
    fn expanded_state_shown_in_first_line() {
        let mut d = data();
        d.expanded = true;
        let first = line_text(&lines(&d, 60, &theme())[0]);
        assert_eq!(first, "~/work (main) · expanded", "{first}");
    }

    #[test]
    fn three_lines_usage_right_model_right() {
        let out = lines(&data(), 80, &theme());
        assert_eq!(out.len(), 3);
        let second = line_text(&out[1]);
        // 右对齐 token 段:↑ 为完整 prompt(11k+5.5k+2.2k),括号内缓存明细
        // U(未命中 13.2k)/ R(命中 5.5k)与命中率(5.5/18.7 ≈ 29%)
        assert!(second.contains("↑ 18.7k (U 13.2k / R 5.5k · 29%)"), "{second}");
        assert!(second.contains("↓ 149"), "{second}");
        assert!(second.contains("ctx 10% (12.8k/128k) (auto)"), "{second}");
        assert!(second.contains("$0.0012"), "{second}");
        assert_eq!(display_width(&second), 80);
        // 第三行右对齐模型
        let third = line_text(&out[2]);
        assert!(third.ends_with("mock/m1 · t:high"), "{third}");
        assert_eq!(display_width(&third), 80);
    }

    #[test]
    fn mode_marker_renders_with_distinct_colors() {
        let theme = theme();
        // plan 标记(黄色提醒只读)
        let mut plan = data();
        plan.mode = Some("plan".into());
        let out = lines(&plan, 80, &theme);
        let plan_span = out[2]
            .spans
            .iter()
            .find(|s| s.content.starts_with("[plan]"))
            .unwrap_or_else(|| panic!("plan 标记应出现在第三行"));
        assert_eq!(plan_span.style.fg, Some(theme.warning));
        // full-access 标记(红色警示)
        let mut yolo = data();
        yolo.mode = Some("full-access".into());
        let out = lines(&yolo, 80, &theme);
        let yolo_span = out[2]
            .spans
            .iter()
            .find(|s| s.content.starts_with("[full-access]"))
            .unwrap_or_else(|| panic!("full-access 标记应出现在第三行"));
        assert_eq!(yolo_span.style.fg, Some(theme.error));
        // 无模式标记时不渲染
        let mut no_mode = data();
        no_mode.mode = None;
        let out = lines(&no_mode, 80, &theme);
        assert!(!line_text(&out[2]).contains("["));
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
        // 模型行与 cwd 同色(footer_cwd),不再用 muted
        let model_span = out[2]
            .spans
            .iter()
            .find(|s| s.content.contains("mock/m1"))
            .unwrap();
        assert_eq!(model_span.style.fg, Some(theme.footer_cwd));
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
        // 正常态:usage_ctx 彩色(不再用 dim)
        let line = ctx_segment(1000, 500, &theme).unwrap();
        assert_eq!(line.spans[0].style.fg, Some(theme.usage_ctx));
        // 未知窗口/无用量:不显示
        assert!(ctx_segment(0, 100, &theme).is_none());
        assert!(ctx_segment(1000, 0, &theme).is_none());
    }

    #[test]
    fn zero_usage_shows_zero_tokens_right_aligned() {
        let mut d = data();
        d.input_tokens = 0;
        d.output_tokens = 0;
        d.cache_read = 0;
        d.cache_write = 0;
        d.context_tokens = 0;
        d.cost_total = 0.0;
        let out = lines(&d, 40, &theme());
        assert_eq!(out.len(), 3);
        // 零用量也显示 ↑ 0 │ ↓ 0(token 段常驻,右对齐;无缓存读写不加明细括号)
        let second = line_text(&out[1]);
        assert!(second.contains("↑ 0"), "{second}");
        assert!(second.contains("↓ 0"), "{second}");
        assert!(!second.contains("hit"), "{second}");
        assert_eq!(display_width(&second), 40);
        let third = line_text(&out[2]);
        assert!(third.ends_with("mock/m1 · t:high"), "{third}");
        assert_eq!(display_width(&third), 40);
    }

    #[test]
    fn cache_hit_shown_even_at_zero_percent() {
        // cache_read = 0 但有 cache_write:括号明细仍展示(R 0,命中率 0%)
        let mut d = data();
        d.cache_read = 0;
        let out = lines(&d, 80, &theme());
        let second = line_text(&out[1]);
        assert!(second.contains("(U 13.2k / R 0 · 0%)"), "{second}");
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
