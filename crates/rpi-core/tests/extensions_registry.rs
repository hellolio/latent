//! 编译期扩展注册的隔离语义(07 §8.5):init 失败 = 跳过 + 收集诊断,
//! 已注册工具整体丢弃,session 照常创建(pi loader 的 continue + warning)。

use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::{AgentMessage, Tool, ToolCall, ToolError, ToolOutput, ToolUpdater};
use rpi_core::{
    create_agent_session, AgentSessionConfig, CoreError, Extension, ExtensionApi,
    ExtensionRegistry, NoopUi, SystemPromptOptions,
};
use tokio_util::sync::CancellationToken;

fn model() -> rpi_ai::Model {
    rpi_ai::Model::minimal("mock-1", "mock", "mock")
}

struct StubTool(String);

#[async_trait]
impl Tool for StubTool {
    fn name(&self) -> &str {
        &self.0
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _call: ToolCall,
        _cancel: CancellationToken,
        _updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("ok"))
    }
}

/// init 失败的扩展:注册一个工具后返回 Err(工具必须整体丢弃)。
struct FailingExtension {
    name: String,
}

#[async_trait]
impl Extension for FailingExtension {
    fn name(&self) -> &str {
        &self.name
    }
    async fn init(&self, api: &mut ExtensionApi<'_>) -> Result<(), CoreError> {
        api.register_tool(Arc::new(StubTool(format!("{}__leaked", self.name))));
        Err(CoreError::Extension {
            name: self.name.clone(),
            message: "boom in init".into(),
        })
    }
}

/// init 成功的扩展。
struct GoodExtension;

#[async_trait]
impl Extension for GoodExtension {
    fn name(&self) -> &str {
        "good"
    }
    async fn init(&self, api: &mut ExtensionApi<'_>) -> Result<(), CoreError> {
        api.register_tool(Arc::new(StubTool("good__tool".into())));
        Ok(())
    }
}

#[tokio::test]
async fn failing_extension_is_skipped_with_diagnostic_and_partial_tools_dropped() {
    let mut extensions = ExtensionRegistry::default();
    extensions.register(Arc::new(FailingExtension { name: "bad".into() }));
    extensions.register(Arc::new(GoodExtension));

    let session = create_agent_session(AgentSessionConfig {
        provider: rpi_ai::create_mock_provider("ok"),
        model: model(),
        hooks: Arc::new(rpi_agent::PassthroughHooks),
        ui: Arc::new(NoopUi),
        extensions,
        tools: vec![],
        active_tool_names: None,
        system_prompt: SystemPromptOptions::default(),
        limits: rpi_agent::TurnLimits::default(),
        stream_options: Default::default(),
        subscribers: None,
        permission: None,
        session_sink: None,
        compactor: None,
        seed_messages: Vec::new(),
    })
    .await
    .expect("init 失败不应阻断 session 创建(07 §8.5)");

    // 诊断:失败扩展被记录
    let diagnostics = session.extension_diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].extension, "bad");
    assert!(diagnostics[0].message.contains("boom in init"));

    // 工具集:好扩展的工具在,坏扩展的泄漏工具不在
    session
        .set_active_tools_by_name(&["good__tool".to_string()])
        .await
        .expect("good 工具应已知");
    assert!(
        session
            .set_active_tools_by_name(&["bad__leaked".to_string()])
            .await
            .is_err(),
        "失败扩展的已注册工具必须整体丢弃"
    );
    let _ = AgentMessage::user("unused");
}
