//! 端到端验收(07 §8.7 步骤 5):`--mock` 链路上以真实子进程
//! (`latent --mcp-mock-server`,CARGO_BIN_EXE)跑 MCP 扩展,完成
//! "注册工具 + 拦截危险 bash + elicitation 确认"全链路。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use latent_agent::AgentEvent;
use latent::assembly::{run_session, SessionRequest};
use latent::mcp_mock::MOCK_EXTENSION_NAME;
use latent_core::{McpServerSpec, SessionSharedSubscriber, SessionSubscriber};

fn mock_spec() -> McpServerSpec {
    let bin = env!("CARGO_BIN_EXE_latent");
    McpServerSpec {
        name: Some(MOCK_EXTENSION_NAME.into()),
        command: bin.into(),
        args: vec!["--mcp-mock-server".into()],
        env: Default::default(),
    }
}

/// 收集 tool 结果与最终 assistant 文本,供断言。
#[derive(Default)]
struct ToolResultCollector {
    outputs: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl SessionSubscriber for ToolResultCollector {
    async fn on_session_event(&self, event: &latent_core::AgentSessionEvent) {
        let latent_core::AgentSessionEvent::Agent(agent_event) = event else {
            return;
        };
        if let AgentEvent::ToolExecutionEnd {
            tool_name, output, ..
        } = agent_event
        {
            self.outputs
                .lock()
                .unwrap()
                .push((tool_name.clone(), output.clone()));
        }
    }
}

fn scripted_provider(model: &latent_ai::Model) -> Arc<dyn latent_ai::Provider> {
    use latent_ai::{ContentBlock, ScriptedProvider, ScriptedTurn};
    let tool_call = |id: &str, name: &str, args: serde_json::Value| ContentBlock::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    };
    Arc::new(ScriptedProvider::new(
        model,
        vec![
            // turn 1:危险 bash → mock 扩展 fail-closed 拦截
            ScriptedTurn::tool_calls(
                model,
                vec![tool_call(
                    "t1",
                    "bash",
                    serde_json::json!({"command": "echo dangerous-thing"}),
                )],
            ),
            // turn 2:扩展注册的工具(带扩展前缀),执行时经 elicitation 确认
            ScriptedTurn::tool_calls(
                model,
                vec![tool_call(
                    "t2",
                    &format!("{MOCK_EXTENSION_NAME}__echo"),
                    serde_json::json!({"text": "hello"}),
                )],
            ),
            // turn 3:收尾
            ScriptedTurn::text(model, "done"),
        ],
    ))
}

#[tokio::test]
async fn e2e_mock_extension_registers_tool_blocks_bash_and_confirms_via_elicitation() {
    let model = latent_ai::Model::minimal("mock-1", "mock", "mock");
    let collector = Arc::new(ToolResultCollector::default());
    let subscriber: SessionSharedSubscriber = collector.clone();

    let stop = run_session(SessionRequest {
        provider: scripted_provider(&model),
        model,
        prompt: "go".into(),
        extension_specs: vec![mock_spec()],
        extra_subscriber: Some(subscriber),
        session_store: latent::assembly::SessionStore::Memory,
        // Plan 模式默认收掉 MCP 扩展工具(13 文档 §5),此用例测扩展全链路
        // → 显式 FullAccess 保持旧行为
        settings: Default::default(),
        session_mode_override: Some(latent_core::SessionMode::FullAccess),
    })
    .await
    .expect("E2E session 应成功");
    assert_eq!(stop, latent_agent::RunStop::EndTurn);

    let outputs = collector.outputs.lock().unwrap().clone();
    assert_eq!(outputs.len(), 2, "两次工具调用各一个结果: {outputs:?}");

    // 1) 危险 bash 被 mock 扩展拦截(fail-closed 的 tool_call 事件)
    assert_eq!(outputs[0].0, "bash");
    assert!(
        outputs[0].1.contains("blocked by e2e extension"),
        "危险命令应被拦截: {}",
        outputs[0].1
    );

    // 2) 扩展注册的 echo 工具经 elicitation 确认后执行(NoopUi confirm=true)
    assert_eq!(outputs[1].0, format!("{MOCK_EXTENSION_NAME}__echo"));
    assert!(
        outputs[1].1.contains("echo: hello") && outputs[1].1.contains("confirmed=true"),
        "echo 应经 elicitation 确认执行: {}",
        outputs[1].1
    );
}
