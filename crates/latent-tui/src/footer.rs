//! 底部状态栏(pi components/footer.ts 的对应物,两行):
//! 1. 左侧 cwd(~/ 缩写)+(git 分支),右侧 token 段:↑prompt(缓存明细
//!    U/R 与命中率) │ ↓out │ ctx%(>70% warning 黄、>90% error 红)
//!    · 用量/窗口 │ $cost,每段独立着色;
//! 2. 左侧 agent 会话标记(`agent:main` 或当前子 agent)+ 模式标记
//!    (plan 黄 / confirm 默认 / full-access 红),右侧模型 · thinking。

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
    /// 会话模式标记(plan 粉 / confirm 默认 / full-access 红)
    pub mode: Option<String>,
    /// ctrl+o 全局展开态(第一行追加 `· expanded` 提示)
    pub expanded: bool,
    /// 存活的后台 subagent 运行数(0 = 不显示)
    pub subagent_active: usize,
    /// 当前激活的平行子 agent(/subagent 切换;None = 主会话)
    pub active_agent: Option<String>,
}

/// 两行 footer:路径+token 同行,agent/模式标记紧随其下一行。
pub fn lines(data: &FooterData, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let width = width.max(1);
    let dim = Style::new().fg(theme.dim);
    let mut first: Vec<Span<'static>> = vec![Span::styled(
        data.cwd.clone(),
        Style::new().fg(theme.footer_cwd),
    )];
    if let Some(branch) = &data.git_branch {
        first.push(Span::styled(format!(" ({branch})"), Style::new().fg(theme.warning)));
    }
    if data.expanded {
        first.push(Span::styled(" · expanded", dim));
    }
    let line1 = left_right_align(Line::from(first), usage_line(data, theme), width);

    // 第二行:左侧 agent 会话标记(主会话显示 agent:main)+ 模式标记,右侧模型
    // agent 标签双色:"agent:" 前缀弱化,值区分主会话(muted)与激活子 agent
    let (agent_value, agent_style) = match &data.active_agent {
        Some(name) => (name.clone(), Style::new().fg(theme.subagent)),
        None => ("main".to_string(), Style::new().fg(theme.muted)),
    };
    let mut left2: Vec<Span<'static>> = vec![
        Span::styled("agent:", dim),
        Span::styled(agent_value, agent_style),
    ];
    if let Some(mode) = &data.mode {
        // 模式标记:full-access 红色警示,plan 粉红提醒只读
        let style = match mode.as_str() {
            "full-access" => Style::new().fg(theme.error),
            "plan" => Style::new().fg(theme.mode_plan),
            _ => Style::new().fg(theme.dim),
        };
        left2.push(Span::styled(" │ ".to_string(), dim));
        left2.push(Span::styled(mode.clone(), style));
    }
    let mut right2: Vec<Span<'static>> = vec![
        Span::styled(
            data.model.clone(),
            // 模型行与 cwd 同色(此前 muted 过暗,与背景区分度不足)
            Style::new().fg(theme.footer_cwd),
        ),
        // thinking 段弱化(与 ctx 段的 (auto) 标记同色)
        Span::styled(format!(" · thinking:{}", data.thinking), dim),
    ];
    if data.subagent_active > 0 {
        // 后台 subagent 运行数(14 文档 §4.3 进度可见:footer 状态段,不做活组件)
        right2.push(Span::styled(
            format!(" · ⏷{} subagent", data.subagent_active),
            Style::new().fg(theme.warning),
        ));
    }
    let line2 = left_right_align(Line::from(left2), Line::from(right2), width);
    vec![line1, line2]
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

/// 单回合用量行参数(转录定稿后落盘):图标与配色与 footer 右侧 token 段
/// 一致,额外带 reasoning 段(footer 不显示);`speed` 为本回合的输出速度
/// 与首 token 延迟(Some 时置於行首,`> TPS 12.8 tok/s · TTFT 6.0s`,
/// TPS 与 ↓ 同色、TTFT 与 ↑ 同色)。
pub struct TurnUsageLine<'a> {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: Option<u64>,
    pub cost_total: f64,
    /// (输出速度 tok/s, 首 token 延迟 s);None = 不显示速度段
    pub speed: Option<(f64, f64)>,
    pub theme: &'a Theme,
}

pub fn turn_usage_line(params: TurnUsageLine<'_>) -> Line<'static> {
    let TurnUsageLine {
        input,
        output,
        cache_read,
        cache_write,
        reasoning,
        cost_total,
        speed,
        theme,
    } = params;
    let dim = Style::new().fg(theme.dim);
    let data = FooterData {
        input_tokens: input,
        output_tokens: output,
        cache_read,
        cache_write,
        cost_total,
        ..FooterData::default()
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some((tps, ttft)) = speed {
        // 速度段双色且与 token 段语义成对:TPS(输出速度)与 ↓ 同色,
        // TTFT(prefill 延迟)与 ↑ 同色
        spans.push(Span::styled(
            format!("> TPS {tps:.1} tok/s"),
            Style::new().fg(theme.usage_output),
        ));
        spans.push(Span::styled(
            format!(" · TTFT {ttft:.1}s"),
            Style::new().fg(theme.usage_input),
        ));
    }
    let segments = usage_segments(&data, theme);
    if !spans.is_empty() && !segments.is_empty() {
        spans.push(Span::styled(" │ ".to_string(), dim));
    }
    spans.extend(segments);
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

/// 左右双段对齐:左侧顶格、右侧贴右;合计超宽时整体按宽截断。
fn left_right_align(
    left: Line<'static>,
    right: Line<'static>,
    width: usize,
) -> Line<'static> {
    let left = truncate_line(left, width);
    let left_w = display_width(&line_text(&left));
    let right_w = display_width(&line_text(&right));
    let mut spans = left.spans;
    if left_w + right_w < width {
        spans.push(Span::raw(" ".repeat(width - left_w - right_w)));
    }
    spans.extend(right.spans);
    truncate_line(Line::from(spans), width)
}

/// token 数紧凑格式:≥1000 显示 k(1100 → 1.1k,128000 → 128k),≥1e6 显示 m。
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        let m = n as f64 / 1_000_000.0;
        let formatted = format!("{m:.1}");
        let formatted = formatted.strip_suffix(".0").unwrap_or(&formatted);
        format!("{formatted}m")
    } else if n >= 1000 {
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
    fn first_line_has_cwd_left_and_tokens_right() {
        let out = lines(&data(), 100, &theme());
        let first = line_text(&out[0]);
        assert!(first.starts_with("~/work (main)"), "{first}");
        assert!(first.ends_with("(auto)"), "{first}");
        assert_eq!(display_width(&first), 100);
        // git 分支黄色(warning),与绿主色 cwd 区分
        let branch_span = out[0]
            .spans
            .iter()
            .find(|s| s.content.contains("(main)"))
            .unwrap_or_else(|| panic!("分支应出现在第一行"));
        assert_eq!(branch_span.style.fg, Some(theme().warning));
    }

    #[test]
    fn expanded_state_shown_in_first_line() {
        let mut d = data();
        d.expanded = true;
        let first = line_text(&lines(&d, 100, &theme())[0]);
        assert!(first.starts_with("~/work (main) · expanded"), "{first}");
    }

    #[test]
    fn two_lines_tokens_and_agent_model() {
        let out = lines(&data(), 100, &theme());
        assert_eq!(out.len(), 2);
        let first = line_text(&out[0]);
        // 第一行左侧路径,右侧 token 段:↑ 为完整 prompt(11k+5.5k+2.2k),
        // 括号内缓存明细 U(未命中 13.2k)/ R(命中 5.5k)与命中率(≈ 29%)
        assert!(first.starts_with("~/work (main)"), "{first}");
        assert!(first.contains("↑ 18.7k (U 13.2k / R 5.5k · 29%)"), "{first}");
        assert!(first.contains("↓ 149"), "{first}");
        assert!(first.contains("ctx 10% (12.8k/128k) (auto)"), "{first}");
        assert!(first.contains("$0.0012"), "{first}");
        assert_eq!(display_width(&first), 100);
        // 第二行:左侧 agent:main │ 模式,右侧模型
        let second = line_text(&out[1]);
        assert!(second.starts_with("agent:main │ confirm"), "{second}");
        assert!(second.ends_with("mock/m1 · thinking:high"), "{second}");
        assert_eq!(display_width(&second), 100);
    }

    #[test]
    fn active_agent_shown_at_left_with_mode() {
        let theme = theme();
        let mut d = data();
        d.active_agent = Some("reviewer".into());
        d.mode = Some("plan".into());
        let out = lines(&d, 80, &theme);
        let second = line_text(&out[1]);
        assert!(second.starts_with("agent:reviewer │ plan"), "{second}");
        // agent 标签双色:"agent:" 前缀弱化(dim),激活子 agent 值用
        // subagent 色,主会话值用 muted(均非 warning 黄)
        let prefix = out[1]
            .spans
            .iter()
            .find(|s| s.content == "agent:")
            .unwrap_or_else(|| panic!("agent: 前缀应出现在第二行"));
        assert_eq!(prefix.style.fg, Some(theme.dim));
        let agent_span = out[1]
            .spans
            .iter()
            .find(|s| s.content == "reviewer")
            .unwrap_or_else(|| panic!("激活 agent 值应出现在第二行"));
        assert_eq!(agent_span.style.fg, Some(theme.subagent));
        let main = lines(&data(), 80, &theme);
        let main_span = main[1]
            .spans
            .iter()
            .find(|s| s.content == "main")
            .unwrap_or_else(|| panic!("主会话也应显示 agent:main"));
        assert_eq!(main_span.style.fg, Some(theme.muted));
    }

    #[test]
    fn mode_marker_renders_with_distinct_colors() {
        let theme = theme();
        // plan 标记(黄色提醒只读)
        let mut plan = data();
        plan.mode = Some("plan".into());
        let out = lines(&plan, 80, &theme);
        let plan_span = out[1]
            .spans
            .iter()
            .find(|s| s.content == "plan")
            .unwrap_or_else(|| panic!("plan 标记应出现在第二行"));
        assert_eq!(plan_span.style.fg, Some(theme.mode_plan), "plan 应为粉红");
        // full-access 标记(红色警示)
        let mut yolo = data();
        yolo.mode = Some("full-access".into());
        let out = lines(&yolo, 80, &theme);
        let yolo_span = out[1]
            .spans
            .iter()
            .find(|s| s.content == "full-access")
            .unwrap_or_else(|| panic!("full-access 标记应出现在第二行"));
        assert_eq!(yolo_span.style.fg, Some(theme.error));
        // 无模式标记时不渲染(分隔符也不出现)
        let mut no_mode = data();
        no_mode.mode = None;
        let out = lines(&no_mode, 80, &theme);
        assert!(!line_text(&out[1]).contains(" │ confirm"), "无模式标记时应省略分隔段");
        assert!(line_text(&out[1]).starts_with("agent:main"));
    }

    #[test]
    fn usage_colors_are_distinct() {
        let out = lines(&data(), 100, &theme());
        let theme = theme();
        let first = &out[0];
        let input_span = first
            .spans
            .iter()
            .find(|s| s.content.starts_with("↑ "))
            .unwrap();
        assert_eq!(input_span.style.fg, Some(theme.usage_input));
        let output_span = first
            .spans
            .iter()
            .find(|s| s.content.starts_with("↓ "))
            .unwrap();
        assert_eq!(output_span.style.fg, Some(theme.usage_output));
        let cost_span = first.spans.iter().find(|s| s.content.starts_with('$')).unwrap();
        assert_eq!(cost_span.style.fg, Some(theme.usage_cost));
        // 模型行与 cwd 同色(footer_cwd),不再用 muted
        let model_span = out[1]
            .spans
            .iter()
            .find(|s| s.content.contains("mock/m1"))
            .unwrap();
        assert_eq!(model_span.style.fg, Some(theme.footer_cwd));
        // thinking 段弱化,与 ctx 段的 (auto) 标记同色(dim)
        let thinking_span = out[1]
            .spans
            .iter()
            .find(|s| s.content.starts_with(" · thinking:"))
            .unwrap_or_else(|| panic!("thinking 段应独立成 span"));
        assert_eq!(thinking_span.style.fg, Some(theme.dim));
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
        let out = lines(&d, 44, &theme());
        assert_eq!(out.len(), 2);
        // 零用量也显示 ↑ 0 │ ↓ 0(token 段常驻,右对齐;无缓存读写不加明细括号)
        let first = line_text(&out[0]);
        assert!(first.contains("↑ 0"), "{first}");
        assert!(first.contains("↓ 0"), "{first}");
        assert!(!first.contains("hit"), "{first}");
        assert_eq!(display_width(&first), 44);
        let second = line_text(&out[1]);
        assert!(second.starts_with("agent:main"), "{second}");
        assert!(second.ends_with("mock/m1 · thinking:high"), "{second}");
        assert_eq!(display_width(&second), 44);
    }

    #[test]
    fn cache_hit_shown_even_at_zero_percent() {
        // cache_read = 0 但有 cache_write:括号明细仍展示(R 0,命中率 0%)
        let mut d = data();
        d.cache_read = 0;
        let out = lines(&d, 80, &theme());
        let first = line_text(&out[0]);
        assert!(first.contains("(U 13.2k / R 0 · 0%)"), "{first}");
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
        assert_eq!(format_tokens(999_999), "1000k");
        assert_eq!(format_tokens(1_000_000), "1m");
        assert_eq!(format_tokens(1_100_000), "1.1m");
        assert_eq!(format_tokens(12_800_000), "12.8m");
    }
}
