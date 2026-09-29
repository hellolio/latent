//! 有界输出(index.ts boundSearchPresentation 的移植):
//! 把模型可见文本截到 maxInlineContentChars(默认 30,000,上限 200,000),
//! 预留截断 marker 空间;截断时附 get_search_content 检索指引。

pub const DEFAULT_MAX_INLINE_CONTENT_CHARS: usize = 30_000;
pub const MAX_INLINE_CONTENT_CHARS_CAP: usize = 200_000;

/// 生效上限:配置 clamp 到 [1, 200000],未配置 = 30000。
pub fn effective_max_inline_chars(configured: Option<usize>) -> usize {
    match configured {
        Some(value) => value.clamp(1, MAX_INLINE_CONTENT_CHARS_CAP),
        None => DEFAULT_MAX_INLINE_CONTENT_CHARS,
    }
}

/// 截断 marker(index.ts:802 的契约文案)。
pub const TRUNCATION_MARKER: &str = "\n\n---\n[Output truncated.]";

#[derive(Debug, Clone, PartialEq)]
pub struct BoundPresentation {
    pub text: String,
    pub truncated: bool,
    pub original_chars: usize,
    pub returned_chars: usize,
}

/// 组装 + 截断(index.ts:792-813):
/// `text + guidance` 不超限 → 原样;超限 → `text 前缀 + "\n\n---\n[Output truncated.]" +
/// truncation_guidance`(与未截断指引不同:即使 get_search_content 未激活也给出启用提示)。
pub fn bound_search_presentation(
    text: &str,
    guidance: &str,
    truncation_guidance: &str,
    max_chars: usize,
) -> BoundPresentation {
    let original_chars = text.chars().count();
    let full_text = format!("{text}{guidance}");
    if full_text.chars().count() <= max_chars {
        let returned_chars = full_text.chars().count();
        return BoundPresentation {
            text: full_text,
            truncated: false,
            original_chars,
            returned_chars,
        };
    }
    let marker = format!("{TRUNCATION_MARKER}{truncation_guidance}");
    let budget = max_chars.saturating_sub(marker.chars().count());
    let bounded = format!("{}{marker}", text.chars().take(budget).collect::<String>());
    BoundPresentation {
        returned_chars: bounded.chars().count(),
        truncated: true,
        original_chars,
        text: bounded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_short_text_with_guidance() {
        let result = bound_search_presentation("body", "\n\n---\nguidance", "trunc", 1000);
        assert!(!result.truncated);
        assert_eq!(result.text, "body\n\n---\nguidance");
    }

    #[test]
    fn truncates_with_truncation_guidance() {
        let body = "a".repeat(10_000);
        let guidance = "\n\n---\nguidance";
        let truncation_guidance = " Enable get_search_content to retrieve the full stored results.";
        let result = bound_search_presentation(&body, guidance, truncation_guidance, 1_000);
        assert!(result.truncated);
        assert_eq!(result.original_chars, 10_000);
        assert!(result.text.contains(TRUNCATION_MARKER));
        assert!(result.text.ends_with(truncation_guidance));
        assert!(result.returned_chars <= 1_000);
    }

    #[test]
    fn default_and_cap() {
        assert_eq!(effective_max_inline_chars(None), 30_000);
        assert_eq!(effective_max_inline_chars(Some(999_999)), 200_000);
        assert_eq!(effective_max_inline_chars(Some(0)), 1);
    }
}
