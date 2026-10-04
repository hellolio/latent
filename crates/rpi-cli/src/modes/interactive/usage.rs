//! 用量追踪(T4):单回合用量行渲染 + 会话累计摘要(footer 侧)。

use ratatui::text::Line;

/// T4 用量追踪:每回合终态(TurnEnd)推送一次 usage。
#[derive(Default)]
pub struct UsageTracker {
    pub total: rpi_ai::Usage,
}

impl UsageTracker {
    /// 记录一回合用量并返回累计摘要(footer;无用量时空串)。
    pub fn push(&mut self, usage: &rpi_ai::Usage) {
        accumulate(&mut self.total, usage);
    }

    #[cfg(test)]
    pub fn summary(&self) -> String {
        if self.total.total_tokens == 0 && self.total.cost.total == 0.0 {
            return String::new();
        }
        format!(
            " · Σ {} tok ${:.6}",
            self.total.total_tokens, self.total.cost.total
        )
    }
}

/// 单回合用量行(定稿后随转录落盘,裸行无外框):样式与 footer 右侧
/// token 段一致(同图标同配色,见 rpi_tui::footer::turn_usage_line)。
/// `speed` = 本回合 (输出速度 tok/s, 首 token 延迟 s),置於行首。
pub fn usage_line(
    usage: &rpi_ai::Usage,
    speed: Option<(f64, f64)>,
    theme: &rpi_tui::Theme,
) -> Line<'static> {
    rpi_tui::footer::turn_usage_line(rpi_tui::footer::TurnUsageLine {
        input: usage.input,
        output: usage.output,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
        reasoning: usage.reasoning,
        cost_total: usage.cost.total,
        speed,
        theme,
    })
}

/// 最后一次请求的完整上下文规模(footer ctx% 的分母侧估计):pi footer 同源
/// 算法(usage.input + output + cacheRead + cacheWrite)。
pub fn context_tokens_of(usage: &rpi_ai::Usage) -> u64 {
    usage.input + usage.output + usage.cache_read + usage.cache_write
}

fn accumulate(total: &mut rpi_ai::Usage, usage: &rpi_ai::Usage) {
    total.input += usage.input;
    total.output += usage.output;
    total.cache_read += usage.cache_read;
    total.cache_write += usage.cache_write;
    total.cache_write_1h = match (total.cache_write_1h, usage.cache_write_1h) {
        (Some(a), Some(b)) => Some(a + b),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    total.reasoning = match (total.reasoning, usage.reasoning) {
        (Some(a), Some(b)) => Some(a + b),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    total.total_tokens += usage.total_tokens;
    total.cost.input += usage.cost.input;
    total.cost.output += usage.cost.output;
    total.cost.cache_read += usage.cost.cache_read;
    total.cost.cache_write += usage.cost.cache_write;
    total.cost.total += usage.cost.total;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage() -> rpi_ai::Usage {
        rpi_ai::Usage {
            input: 100,
            output: 20,
            cache_read: 5,
            cache_write: 10,
            cache_write_1h: None,
            reasoning: Some(8),
            total_tokens: 135,
            cost: rpi_ai::Cost {
                total: 0.0012,
                ..Default::default()
            },
        }
    }

    #[test]
    fn accumulate_and_summary() {
        let mut tracker = UsageTracker::default();
        tracker.push(&usage());
        tracker.push(&usage());
        let summary = tracker.summary();
        assert!(summary.contains("270 tok"), "{summary}");
        assert!(summary.contains("$0.002400"), "{summary}");
    }

    #[test]
    fn zero_usage_has_empty_summary() {
        let tracker = UsageTracker::default();
        assert_eq!(tracker.summary(), "");
    }

    #[test]
    fn usage_line_contains_fields() {
        let theme = rpi_tui::Theme::dark_ansi();
        let line = usage_line(&usage(), None, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        // 与 footer token 段同格式:↑ 完整 prompt + 缓存明细 U/R 与命中率 +
        // $cost + reasoning
        // ↑ = 100 + 5 + 10 = 115;U = 100 + 10 = 110;R = cache_read 5;
        // 命中率 = 5/115 ≈ 4%
        assert!(text.contains("↑ 115 (U 110 / R 5 · 4%)"), "{text}");
        assert!(text.contains("↓ 20"), "{text}");
        assert!(text.contains("reasoning 8"), "{text}");
        assert!(text.contains("$0.0012"), "{text}");
    }

    #[test]
    fn usage_line_speed_prefix() {
        let theme = rpi_tui::Theme::dark_ansi();
        // 有计时数据:行首为速度段,与后续 token 段以 │ 分隔
        let line = usage_line(&usage(), Some((12.84, 6.03)), &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with("> TPS 12.8 tok/s · TTFT 6.0s"), "{text}");
        assert!(text.contains(" │ ↑ 115"), "{text}");
        // 速度段双色:TPS 与 ↓ 段同色(usage_output),TTFT 与 ↑ 段同色
        // (usage_input),不再是全灰
        let tps = line.spans.first().unwrap();
        assert_eq!(tps.content, "> TPS 12.8 tok/s");
        assert_eq!(tps.style.fg, Some(theme.usage_output));
        let ttft = line.spans.get(1).unwrap();
        assert_eq!(ttft.content, " · TTFT 6.0s");
        assert_eq!(ttft.style.fg, Some(theme.usage_input));
    }

    #[test]
    fn usage_line_spans_share_footer_colors() {
        let theme = rpi_tui::Theme::dark_ansi();
        let line = usage_line(&usage(), None, &theme);
        let input = line
            .spans
            .iter()
            .find(|s| s.content.starts_with("↑ "))
            .unwrap();
        assert_eq!(input.style.fg, Some(theme.usage_input));
        let output = line
            .spans
            .iter()
            .find(|s| s.content.starts_with("↓ "))
            .unwrap();
        assert_eq!(output.style.fg, Some(theme.usage_output));
        let cost = line.spans.iter().find(|s| s.content.starts_with('$')).unwrap();
        assert_eq!(cost.style.fg, Some(theme.usage_cost));
    }
}
