//! 页面问答(fetch_content answer 模式;page-query.ts 的移植):
//! 小模型只依据页面内容作答,system prompt 防提示注入(逐字契约),
//! 输入预算 = 上下文窗口 × 60% × 3 字符/token,超限截断并附提示。

use tokio_util::sync::CancellationToken;

use crate::config::WebSearchConfig;
use crate::llm::{complete_text, LlmDeps};
use crate::prompts;

const OUTPUT_TOKENS: u32 = 2_000;
const INPUT_CONTEXT_FRACTION: f64 = 0.6;
const CHARS_PER_TOKEN: usize = 3;
const FALLBACK_CONTEXT_TOKENS: u64 = 80_000;
const SAFETY_TOKENS: u64 = 4_096;

pub struct PageAnswer {
    pub text: String,
    pub model: String,
    pub input_chars: usize,
    pub original_input_chars: usize,
    pub truncated: bool,
}

/// 页面预算截断 + LLM 问答。
pub async fn answer_from_page(
    deps: &LlmDeps,
    config: &WebSearchConfig,
    question: &str,
    page_text: &str,
    source_url: &str,
    cancel: &CancellationToken,
) -> Result<PageAnswer, String> {
    let model = crate::llm::resolve_llm_model(deps, config.summary_model.as_deref())?;
    let context_tokens = if model.context_window > 0 {
        model.context_window
    } else {
        FALLBACK_CONTEXT_TOKENS
    };
    let maximum_input_tokens = ((context_tokens as f64 * INPUT_CONTEXT_FRACTION) as u64)
        .min(context_tokens.saturating_sub(OUTPUT_TOKENS as u64 + SAFETY_TOKENS))
        .max(1);
    let maximum_input_chars = maximum_input_tokens as usize * CHARS_PER_TOKEN;
    let truncated_page: String = page_text.chars().take(maximum_input_chars).collect();
    let truncated = truncated_page.chars().count() < page_text.chars().count();
    let prompt = prompts::page_query_user_message(question, source_url, &truncated_page);

    let text = complete_text(
        deps,
        &model,
        prompts::PAGE_QUERY_SYSTEM_PROMPT,
        &prompt,
        OUTPUT_TOKENS,
        cancel,
        std::time::Duration::from_secs(120),
    )
    .await?;

    Ok(PageAnswer {
        text: if truncated {
            format!(
                "{text}{}",
                prompts::page_truncation_note(truncated_page.chars().count(), page_text.chars().count())
            )
        } else {
            text
        },
        model: format!("{}/{}", model.provider, model.id),
        input_chars: truncated_page.chars().count(),
        original_input_chars: page_text.chars().count(),
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_shapes() {
        // 小窗口模型:60% 且扣除 output+safety
        let model = latent_ai::Model::minimal("m", "mock", "mock");
        let mut model = model;
        model.context_window = 10_000;
        let maximum_input_tokens = ((model.context_window as f64 * 0.6) as u64)
            .min(model.context_window.saturating_sub(2_000 + 4_096))
            .max(1);
        assert_eq!(maximum_input_tokens, 3_904);
        assert_eq!(maximum_input_tokens as usize * CHARS_PER_TOKEN, 11_712);
    }
}
