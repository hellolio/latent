//! 接缝 #1(循环 ↔ provider):`Provider` trait 与按 api 字符串的开放注册表
//! (09 B5.1:`Api = KnownApi | string` 的开放性用运行时注册表实现)。

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;

use crate::types::{AssistantMessageEvent, Model, StreamOptions, TranscriptContext};

/// 统一事件流:失败编码进流(终态 error 事件),与 pi 契约一致(09 B3)。
pub type AssistantMessageEventStream =
    Pin<Box<dyn futures::Stream<Item = AssistantMessageEvent> + Send>>;

/// provider 统一契约(pi 的 ProviderStreams.stream;失败不抛异常)。
#[async_trait]
pub trait Provider: Send + Sync {
    async fn stream(
        &self,
        model: &Model,
        ctx: TranscriptContext,
        opts: StreamOptions,
    ) -> AssistantMessageEventStream;
}

/// 按 api 字符串查找适配器的注册表;扩展可注册任意新 API(pi 的开放集语义)。
#[derive(Default, Clone)]
pub struct ProviderRegistry {
    by_api: std::collections::HashMap<String, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 内置适配器注册表:anthropic-messages + openai-completions。
    pub fn with_builtin_adapters() -> Self {
        let mut registry = Self::new();
        registry.register("anthropic-messages", crate::adapters::anthropic::create_anthropic_adapter());
        registry.register(
            "openai-completions",
            crate::adapters::openai_completions::create_openai_completions_adapter(),
        );
        registry
    }

    pub fn register(&mut self, api: impl Into<String>, provider: Arc<dyn Provider>) {
        self.by_api.insert(api.into(), provider);
    }

    pub fn get(&self, api: &str) -> Option<Arc<dyn Provider>> {
        self.by_api.get(api).cloned()
    }
}

/// 便捷工厂:按 api 字符串从内置注册表创建 provider(M1 的入口;宿主也可以
/// 自建 `ProviderRegistry` 注入 Mock 或自定义适配器)。
pub fn create_provider(api: &str) -> Option<Arc<dyn Provider>> {
    ProviderRegistry::with_builtin_adapters().get(api)
}

/// 工厂:MockProvider(canned 流式回复,测试/自检用,不联网)。
pub fn create_mock_provider(reply: impl Into<String>) -> Arc<dyn Provider> {
    Arc::new(crate::mock::MockProvider::new(reply))
}

/// 工厂:按 provider id(如 "anthropic"/"openai")创建默认真实 provider,
/// 模型 baseUrl 取该 provider 的官方端点。未知 provider 返回 None。
pub fn create_default_provider(provider_id: &str) -> Option<Arc<dyn Provider>> {
    let (api, _) = default_provider_endpoint(provider_id)?;
    create_provider(api)
}

/// 内置 provider 表(id, api, baseUrl):`default_provider_endpoint` 与
/// `/model` 选择器的候选清单共用,新增 provider 只改这里。
pub const BUILTIN_PROVIDERS: &[(&str, &str, &str)] = &[
    ("anthropic", "anthropic-messages", "https://api.anthropic.com"),
    ("openai", "openai-completions", "https://api.openai.com/v1"),
    ("deepseek", "openai-completions", "https://api.deepseek.com"),
    ("groq", "openai-completions", "https://api.groq.com/openai/v1"),
    ("xai", "openai-completions", "https://api.x.ai/v1"),
    ("openrouter", "openai-completions", "https://openrouter.ai/api/v1"),
    ("zai", "openai-completions", "https://api.z.ai/api/paas/v4"),
    ("mistral", "openai-completions", "https://api.mistral.ai/v1"),
    ("moonshotai", "openai-completions", "https://api.moonshot.ai/v1"),
    ("together", "openai-completions", "https://api.together.xyz/v1"),
    ("fireworks", "openai-completions", "https://api.fireworks.ai/inference/v1"),
];

/// 内置 provider id 清单(顺序与 BUILTIN_PROVIDERS 一致)。
pub fn builtin_providers() -> impl Iterator<Item = &'static str> {
    BUILTIN_PROVIDERS.iter().map(|(id, _, _)| *id)
}

/// 已知 provider 的默认 (api, baseUrl)。M1 覆盖两大协议的官方端点。
pub fn default_provider_endpoint(provider_id: &str) -> Option<(&'static str, &'static str)> {
    BUILTIN_PROVIDERS
        .iter()
        .find(|(id, _, _)| *id == provider_id)
        .map(|(_, api, url)| (*api, *url))
}
