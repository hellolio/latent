//! 图片占位符替换的循环级集成测试(12 文档 L1):
//! `LoopConfig.block_images`(settings `blockImages` → Agent → LoopConfig)
//! 在每轮请求前把转录里的 Image 块替换为文本占位符;
//! 关闭且模型支持图片时原样保留。
//!
//! 捕获手段:`ScriptedProvider` 发送前会把 `messages` 序列化进 body 并回调
//! `StreamOptions.on_payload`(mock.rs `observe_mock_payload`),测试据此
//! 断言 latent 实际发出的请求内容。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use latent_agent::{
    AgentContext, AgentEvent, AgentMessage, LoopConfig, PassthroughHooks, SharedSubscriber,
    Subscriber,
};
use latent_ai::{ContentBlock, Model, ScriptedProvider, ScriptedTurn, StreamOptions};
use tokio_util::sync::CancellationToken;

/// 捕获 on_payload 观察到的请求体(mock body 含序列化后的 messages)。
#[derive(Clone, Default)]
struct CapturedBodies(Arc<Mutex<Vec<serde_json::Value>>>);

impl CapturedBodies {
    fn push(&self, body: &serde_json::Value) {
        self.0.lock().unwrap().push(body.clone());
    }

    fn joined(&self) -> String {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|body| body.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 最小事件订阅者(循环要求有 sink)。
#[derive(Default)]
struct Collector;

#[async_trait]
impl Subscriber for Collector {
    async fn on_event(&self, _event: &AgentEvent) {}
}

/// 支持图片输入的模型(隔离 block_images 开关本身的语义)。
fn vision_model() -> Model {
    let mut model = Model::minimal("mock-1", "mock", "mock");
    model.input = vec!["text".into(), "image".into()];
    model
}

fn image_block() -> ContentBlock {
    ContentBlock::Image {
        data: "aGVsbG8=".into(),
        mime_type: "image/png".into(),
    }
}

async fn run_with_block_images(block: bool) -> CapturedBodies {
    let model = vision_model();
    let captured = CapturedBodies::default();
    let mut stream_options = StreamOptions::default();
    let hook = captured.clone();
    stream_options.on_payload = Some(Arc::new(move |body| hook.push(body)));

    let provider = Arc::new(ScriptedProvider::new(
        &model,
        vec![ScriptedTurn::text(&model, "收到")],
    ));
    let context = AgentContext {
        system: None,
        messages: vec![
            AgentMessage::user("hi"),
            AgentMessage::ToolResult {
                tool_call_id: "t1".into(),
                tool_name: "read".into(),
                content: vec![ContentBlock::text("正文"), image_block()],
                details: None,
                usage: None,
                is_error: false,
                timestamp: 0,
            },
        ],
        tools: vec![],
    };
    let config = LoopConfig {
        block_images: block,
        stream_options,
        ..LoopConfig::new(model)
    };
    let sink: SharedSubscriber = Arc::new(Collector);
    let (sender, receiver) = latent_agent::create_injection_endpoints();
    let _ = sender;
    let _output = latent_agent::run_agent_loop(
        Vec::new(),
        context,
        Arc::new(PassthroughHooks),
        config,
        provider,
        sink,
        CancellationToken::new(),
        receiver,
    )
    .await;
    captured
}

/// block_images = true:发出的请求里 Image 块被替换为占位符文本,正文保留。
#[tokio::test]
async fn block_images_replaces_image_blocks_in_outgoing_request() {
    let bodies = run_with_block_images(true).await;
    let raw = bodies.joined();
    assert!(
        raw.contains("[Image omitted"),
        "应包含图片占位符: {raw}"
    );
    assert!(
        !raw.contains("\"type\":\"image\""),
        "不应再发出 image 块: {raw}"
    );
    assert!(raw.contains("正文"), "同块的文本内容应保留: {raw}");
}

/// block_images = false 且模型支持图片:Image 块原样发出。
#[tokio::test]
async fn images_pass_through_when_not_blocked_and_model_supports() {
    let bodies = run_with_block_images(false).await;
    let raw = bodies.joined();
    assert!(
        raw.contains("\"type\":\"image\""),
        "image 块应原样发出: {raw}"
    );
    assert!(!raw.contains("[Image omitted"), "不应出现占位符: {raw}");
    assert!(raw.contains("aGVsbG8="), "图片数据应保留: {raw}");
}
