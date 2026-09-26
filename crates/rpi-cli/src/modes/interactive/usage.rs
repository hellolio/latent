//! 用量追踪(T4):单回合用量行渲染 + 会话累计摘要(footer 侧)。

use ratatui::style::Style;
use ratatui::text::{Line, Span};

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

/// 单回合用量行(定稿后随转录落盘)。
pub fn usage_line(usage: &rpi_ai::Usage, theme: &rpi_tui::Theme) -> Line<'static> {
    let mut text = format!(
        "[tokens] in {} · out {} · cache {}/{}",
        usage.input, usage.output, usage.cache_read, usage.cache_write
    );
    if let Some(reasoning) = usage.reasoning {
        text.push_str(&format!(" · reasoning {reasoning}"));
    }
    text.push_str(&format!(" · ${:.6}", usage.cost.total));
    Line::from(Span::styled(text, Style::new().fg(theme.dim)))
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
        let line = usage_line(&usage(), &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("in 100"), "{text}");
        assert!(text.contains("out 20"), "{text}");
        assert!(text.contains("cache 5/10"), "{text}");
        assert!(text.contains("reasoning 8"), "{text}");
        assert!(text.contains("$0.001200"), "{text}");
    }
}
