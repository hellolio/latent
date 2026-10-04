//! 一次性 LLM 调用(摘要 / answer 模式):复用注入的 `Arc<dyn Provider>`,
//! 调用模式与装配层 ProviderSummarizer 一致(stream → 收集 Done/Error)。

use std::sync::Arc;
use std::time::Duration;

use latent_ai::{AssistantMessageEvent, Message, TranscriptContext};
use tokio_util::sync::CancellationToken;

/// "provider/model-id" → Model(models.json + 内置表)。
pub type ModelResolveFn = Arc<dyn Fn(&str) -> Result<latent_ai::Model, String> + Send + Sync>;

/// 工具内 LLM 能力的注入面(装配层构造)。
#[derive(Clone)]
pub struct LlmDeps {
    pub provider: Arc<dyn latent_ai::Provider>,
    pub resolve_model: ModelResolveFn,
    /// 当前主模型(摘要/answer 未显式配置时的回退)
    pub current_model: Arc<dyn Fn() -> Option<latent_ai::Model> + Send + Sync>,
}

/// 摘要/answer 模型解析:显式配置 → 当前主模型(取舍已确认)。
pub fn resolve_llm_model(deps: &LlmDeps, configured: Option<&str>) -> Result<latent_ai::Model, String> {
    match configured {
        Some(spec) if !spec.trim().is_empty() => (deps.resolve_model)(spec.trim()),
        _ => (deps.current_model)().ok_or_else(|| "No model available for LLM call".to_string()),
    }
}

/// 单轮补全:system + user → 文本。失败返回 Err(错误消息)。
pub async fn complete_text(
    deps: &LlmDeps,
    model: &latent_ai::Model,
    system_prompt: &str,
    user_text: &str,
    max_tokens: u32,
    cancel: &CancellationToken,
    timeout: Duration,
) -> Result<String, String> {
    let messages = vec![
        Message::system(system_prompt),
        Message::user_text(user_text),
    ];
    let options = latent_ai::StreamOptions {
        max_tokens: Some(max_tokens),
        cancel: Some(cancel.clone()),
        ..Default::default()
    };
    let mut stream = deps
        .provider
        .stream(model, TranscriptContext { messages }, options)
        .await;
    let result = tokio::time::timeout(timeout, async {
        let mut text: Option<String> = None;
        while let Some(event) = futures::StreamExt::next(&mut stream).await {
            match event {
                AssistantMessageEvent::Done(assistant) => {
                    let content = assistant.text_content();
                    text = Some(content);
                    break;
                }
                AssistantMessageEvent::Error(error) => {
                    return Err(error
                        .error_message
                        .unwrap_or_else(|| "LLM call failed".to_string()));
                }
                _ => {}
            }
            if cancel.is_cancelled() {
                return Err("Aborted".to_string());
            }
        }
        match text {
            Some(text) if !text.trim().is_empty() => Ok(text.trim().to_string()),
            _ => Err("LLM returned an empty response".to_string()),
        }
    })
    .await
    .map_err(|_| "LLM call timed out".to_string())?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completes_with_mock_provider() {
        let deps = LlmDeps {
            provider: latent_ai::create_mock_provider("mock summary"),
            resolve_model: Arc::new(|spec| {
                Ok(latent_ai::Model::minimal(spec, "mock", "mock"))
            }),
            current_model: Arc::new(|| None),
        };
        let model = resolve_llm_model(&deps, Some("mock/mock")).unwrap();
        let text = complete_text(
            &deps,
            &model,
            "system",
            "user",
            1000,
            &CancellationToken::new(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(text, "mock summary");
    }

    #[test]
    fn model_resolution_prefers_configured() {
        let deps = LlmDeps {
            provider: latent_ai::create_mock_provider("x"),
            resolve_model: Arc::new(|spec| Ok(latent_ai::Model::minimal(spec, "mock", "mock"))),
            current_model: Arc::new(|| Some(latent_ai::Model::minimal("main", "mock", "mock"))),
        };
        // resolve 闭包原样返回 spec;Model::minimal(spec,...).id = spec
        assert_eq!(
            resolve_llm_model(&deps, Some("cfg/model")).unwrap().id,
            "cfg/model"
        );
        assert_eq!(resolve_llm_model(&deps, None).unwrap().id, "main");
        // 空白配置 = 未配置 → 回退主模型
        assert_eq!(resolve_llm_model(&deps, Some("  ")).unwrap().id, "main");
    }
}
