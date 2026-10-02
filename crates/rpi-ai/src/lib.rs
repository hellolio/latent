//! rpi-ai —— provider 抽象层(基座,零内部依赖)。
//!
//! 对外只暴露:工厂函数、`Provider` trait、纯类型。
//! 关键契约:**失败编码进流**——`Provider::stream` 返回的事件流以终态事件(error/done)
//! 收尾,不返回 `Result`(00 文档设计原则 5;02 文档流协议)。

// 内部实现模块:适配器/SSE/JSON 修复不直接暴露,经工厂出厂(方针文档 §2)
pub(crate) mod adapters;
pub(crate) mod json_parse;
pub(crate) mod sse;
// 文档 02 明确的 crate 表面:工厂、trait、类型、工具函数
pub mod env_keys;
pub mod mock;
pub mod overflow;
pub mod provider;
pub mod retry;
pub mod transcript;
pub mod types;

pub use adapters::{
    anthropic::create_anthropic_adapter, openai_completions::create_openai_completions_adapter,
};

pub use mock::{assistant_message, MockProvider, ScriptedProvider, ScriptedTurn};
pub use overflow::{is_context_overflow, is_recoverable_length};
pub use provider::{
    builtin_providers, create_default_provider, create_mock_provider, create_provider,
    default_provider_endpoint, AssistantMessageEventStream, Provider, ProviderRegistry,
};
pub use retry::{
    create_retrying_provider, is_retryable_assistant_error, retry_assistant_call, retry_delay_ms,
    NoopCallbacks, RetryCallbacks, RetryPolicy,
};
pub use transcript::{
    collapse_system_messages, get_current_system_message, get_current_system_prompt,
    get_current_tools, normalize_context, replace_images_with_placeholders, resolve_transcript,
    IMAGE_PLACEHOLDER,
};
pub use types::*;
