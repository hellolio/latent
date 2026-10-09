//! skill developer 注入会话级集成测试(`skills::expand` 两条路径的闭环):
//! 1) 模型调用 load_skill 工具 → 下一轮请求:toolResult 换短确认 + developer(正文);
//! 2) 用户消息 `/skill <名称>` 前缀 → 请求:developer(正文) 在剥前缀的 user 之前,
//!    全程无工具调用参与。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use latent_ai::{
    assistant_message, AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream,
    ContentBlock, Message, Model, ScriptedTurn, StopReason, StreamOptions, TranscriptContext,
    UserContent,
};
use latent_agent::{PassthroughHooks, Tool};
use latent_core::{
    create_agent_session, AgentSessionConfig, ExtensionRegistry, LoadSkillDeps, LoadSkillTool,
    NoopUi, SkillDef,
};

fn model() -> Model {
    Model::minimal("mock-1", "mock", "mock")
}

fn skill_fixture(name: &str, body: &str) -> SkillDef {
    let dir = std::env::temp_dir().join(format!("latent-skill-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("SKILL.md");
    std::fs::write(&path, body).unwrap();
    SkillDef {
        name: name.into(),
        description: "test skill".into(),
        path,
    }
}

fn cleanup(def: &SkillDef) {
    let _ = std::fs::remove_dir_all(def.path.parent().unwrap());
}

fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

/// 记录型 provider:按请求记录角色序列(developer 可见),按序出脚本回复。
struct RecordingProvider {
    model: Model,
    turns: Mutex<VecDeque<AssistantMessage>>,
    captured: Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl latent_ai::Provider for RecordingProvider {
    async fn stream(
        &self,
        _model: &Model,
        ctx: TranscriptContext,
        _opts: StreamOptions,
    ) -> AssistantMessageEventStream {
        let mut roles = Vec::new();
        for message in &ctx.messages {
            let (role, text) = match message {
                Message::Developer { content, .. } => ("developer", content.clone()),
                Message::User {
                    content: UserContent::Text(text),
                    ..
                } => ("user", text.clone()),
                Message::ToolResult { content, is_error, .. } => (
                    "tool_result",
                    content
                        .iter()
                        .filter_map(|block| block.as_text())
                        .collect::<Vec<_>>()
                        .join("")
                        + if *is_error { "(error)" } else { "" },
                ),
                _ => ("other", String::new()),
            };
            roles.push(format!("{role}:{text}"));
        }
        self.captured.lock().unwrap().push(roles);
        let turn = self.turns.lock().unwrap().pop_front();
        let model = self.model.clone();
        Box::pin(async_stream::stream! {
            let message = turn.unwrap_or_else(|| {
                assistant_message(&model, vec![ContentBlock::text("ok")], StopReason::Stop)
            });
            yield AssistantMessageEvent::Done(Box::new(message));
        })
    }
}

async fn session_with(
    provider: Arc<RecordingProvider>,
    tools: Vec<Arc<dyn Tool>>,
    skills: Vec<SkillDef>,
) -> latent_core::AgentSession {
    create_agent_session(AgentSessionConfig {
        provider,
        model: model(),
        hooks: Arc::new(PassthroughHooks),
        ui: Arc::new(NoopUi),
        extensions: ExtensionRegistry::default(),
        tools,
        active_tool_names: None,
        system_prompt: Default::default(),
        limits: Default::default(),
        stream_options: Default::default(),
        session_sink: None,
        seed_messages: vec![],
        compactor: None,
        subscribers: None,
        permission: None,
        skills,
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn load_skill_tool_result_expands_to_confirmation_plus_developer() {
    let def = skill_fixture("review", "# Review\nSteps here.");
    let m = model();
    let provider = Arc::new(RecordingProvider {
        model: m.clone(),
        turns: Mutex::new(VecDeque::from(vec![
            ScriptedTurn::tool_calls(&m, vec![tool_call(
                "t1",
                "load_skill",
                serde_json::json!({"skill": "review"}),
            )])
            .message,
            ScriptedTurn::text(&m, "done").message,
        ])),
        captured: Mutex::new(vec![]),
    });
    let tool = Arc::new(LoadSkillTool::new(LoadSkillDeps {
        skills: vec![def.clone()],
    }));
    let session = session_with(provider.clone(), vec![tool], vec![def.clone()]).await;
    session.prompt("检查一下").await.unwrap();

    let captured = provider.captured.lock().unwrap().clone();
    assert_eq!(captured.len(), 2, "{captured:?}");
    let second = &captured[1];
    // 工具结果在模型视图里换成短确认,正文进紧随的 developer 消息
    assert!(
        second
            .iter()
            .any(|entry| entry.starts_with("tool_result:Skill loaded;")),
        "toolResult 应为短确认: {second:?}"
    );
    assert!(
        second
            .iter()
            .any(|entry| entry.starts_with("developer:<skill name=\"review\">")),
        "正文应以 developer 角色紧随其后: {second:?}"
    );
    assert!(
        !second.iter().any(|entry| entry.starts_with("tool_result:<skill")),
        "正文不应留在 toolResult 里(避免双份): {second:?}"
    );
    cleanup(&def);
}

#[tokio::test]
async fn skill_prefix_injects_developer_before_user_without_tool_call() {
    let def = skill_fixture("dev-loop", "Loop instructions body.");
    let m = model();
    let provider = Arc::new(RecordingProvider {
        model: m.clone(),
        turns: Mutex::new(VecDeque::from(vec![ScriptedTurn::text(&m, "好的").message])),
        captured: Mutex::new(vec![]),
    });
    // 无 load_skill 工具:前缀路径与工具调用完全无关
    let session = session_with(provider.clone(), vec![], vec![def.clone()]).await;
    session.prompt("/skill dev-loop 按流程来").await.unwrap();

    let captured = provider.captured.lock().unwrap().clone();
    assert_eq!(captured.len(), 1, "{captured:?}");
    let first = &captured[0];
    let developer_idx = first
        .iter()
        .position(|entry| entry.starts_with("developer:<skill name=\"dev-loop\">"))
        .expect("developer 消息应存在");
    let user_idx = first
        .iter()
        .position(|entry| *entry == "user:按流程来")
        .expect("剥前缀后的用户消息应存在");
    assert!(
        developer_idx < user_idx,
        "developer 应在用户消息之前: {first:?}"
    );
    assert!(
        !first.iter().any(|entry| entry.contains("/skill dev-loop")),
        "前缀不应进入模型上下文: {first:?}"
    );
    cleanup(&def);
}
