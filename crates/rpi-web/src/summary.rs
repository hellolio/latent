//! 摘要工作流(summary-review.ts 的 auto-summary 子集移植):
//! 摘要 prompt(逐字契约)→ 30s deadline 竞速 → 失败/超时回退
//! 确定性摘要。模型来源:配置 summaryModel → 当前主模型(取舍已确认;
//! 上游的硬编码候选链在 rpi 模型注册表下不可移植)。

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::prompts;
use crate::storage::QueryResultData;
use crate::tools::WebContext;

pub const SUMMARY_GENERATION_DEADLINE: Duration = Duration::from_secs(30);

/// (输出文本, summary meta details)
pub async fn generate_summary(
    context: &WebContext,
    results: &[QueryResultData],
    cancel: &CancellationToken,
) -> (String, serde_json::Value) {
    let started = std::time::Instant::now();
    let prompt = prompts::build_summary_prompt(results);

    let generated = tokio::time::timeout(
        SUMMARY_GENERATION_DEADLINE,
        async {
            let model = crate::llm::resolve_llm_model(
                &context.llm,
                context.config.summary_model.as_deref(),
            )?;
            let text = crate::llm::complete_text(
                &context.llm,
                &model,
                // 摘要 prompt 是单条 user 消息(与上游一致);system 位留空指令
                "You are a precise summarizer.",
                &prompt,
                4_096,
                cancel,
                SUMMARY_GENERATION_DEADLINE,
            )
            .await?;
            Ok::<String, String>(text)
        },
    )
    .await;

    match generated {
        Ok(Ok(summary)) => {
            let token_estimate = (summary.trim().chars().count() / 4).max(1);
            (
                summary,
                serde_json::json!({
                    "model": context.config.summary_model.clone().unwrap_or_else(|| "current-model".to_string()),
                    "durationMs": started.elapsed().as_millis() as u64,
                    "tokenEstimate": token_estimate,
                    "fallbackUsed": false,
                    "phase": "summary-model",
                }),
            )
        }
        Ok(Err(error)) => deterministic_fallback(results, format!("summary-model-unavailable: {error}"), started),
        Err(_timeout) => deterministic_fallback(results, "summary-generation-timeout".to_string(), started),
    }
}

fn deterministic_fallback(
    results: &[QueryResultData],
    reason: String,
    started: std::time::Instant,
) -> (String, serde_json::Value) {
    let summary = prompts::build_deterministic_summary(results);
    let token_estimate = (summary.chars().count() / 4).max(1);
    (
        summary,
        serde_json::json!({
            "model": serde_json::Value::Null,
            "durationMs": started.elapsed().as_millis() as u64,
            "tokenEstimate": token_estimate,
            "fallbackUsed": true,
            "fallbackReason": reason,
            "phase": "deterministic-fallback",
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn falls_back_deterministically_without_llm() {
        // current_model 返回 None → 解析失败 → 确定性回退
        let context = WebContext {
            config: crate::config::WebSearchConfig::default(),
            cache_limits: Default::default(),
            llm: crate::llm::LlmDeps {
                provider: rpi_ai::create_mock_provider("should not be used"),
                resolve_model: std::sync::Arc::new(|spec| {
                    Ok(rpi_ai::Model::minimal(spec, "mock", "mock"))
                }),
                current_model: std::sync::Arc::new(|| None),
            },
            activator: None,
            notifier: None,
            cwd: std::env::temp_dir(),
        };
        let results = vec![QueryResultData {
            query: "q".into(),
            answer: "answer text".into(),
            results: vec![crate::types::SearchResult {
                title: "t".into(),
                url: "https://a.com".into(),
                snippet: String::new(),
            }],
            error: None,
            provider: Some("brave".into()),
            providers: vec!["brave".into()],
        }];
        let (text, meta) = generate_summary(&context, &results, &CancellationToken::new()).await;
        assert!(meta.get("fallbackUsed").and_then(|v| v.as_bool()).unwrap());
        assert!(text.contains("Summary based on the currently selected search results."));
    }

    #[tokio::test]
    async fn uses_llm_when_available() {
        let context = WebContext {
            config: crate::config::WebSearchConfig::default(),
            cache_limits: Default::default(),
            llm: crate::llm::LlmDeps {
                provider: rpi_ai::create_mock_provider("LLM generated summary"),
                resolve_model: std::sync::Arc::new(|spec| {
                    Ok(rpi_ai::Model::minimal(spec, "mock", "mock"))
                }),
                current_model: std::sync::Arc::new(|| {
                    Some(rpi_ai::Model::minimal("main", "mock", "mock"))
                }),
            },
            activator: None,
            notifier: None,
            cwd: std::env::temp_dir(),
        };
        let (text, meta) = generate_summary(&context, &[], &CancellationToken::new()).await;
        assert_eq!(text, "LLM generated summary");
        assert!(!meta.get("fallbackUsed").and_then(|v| v.as_bool()).unwrap());
    }
}
