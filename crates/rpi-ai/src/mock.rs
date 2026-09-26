//! MockProvider:canned 流式回复;ScriptedProvider:按脚本逐 turn 出流(测试多轮循环)。

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::provider::{AssistantMessageEventStream, Provider};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Model, StopReason, StreamOptions,
    TranscriptContext, Usage,
};

const CHUNK_CHARS: usize = 12;

pub struct MockProvider {
    reply: String,
}

impl MockProvider {
    pub fn new(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
        }
    }

    fn final_message(&self, model: &Model) -> AssistantMessage {
        let mut message = AssistantMessage::pending(model);
        message.content = vec![ContentBlock::text(self.reply.clone())];
        message.stop_reason = StopReason::Stop;
        message.usage = Usage::zero();
        message
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn stream(
        &self,
        model: &Model,
        _ctx: TranscriptContext,
        _opts: StreamOptions,
    ) -> AssistantMessageEventStream {
        let model = model.clone();
        let reply = self.reply.clone();
        Box::pin(async_stream::stream! {
            yield AssistantMessageEvent::Start;
            yield AssistantMessageEvent::TextStart { content_index: 0 };
            let chars: Vec<char> = reply.chars().collect();
            for chunk in chars.chunks(CHUNK_CHARS) {
                yield AssistantMessageEvent::TextDelta { content_index: 0, delta: chunk.iter().collect() };
            }
            yield AssistantMessageEvent::TextEnd { content_index: 0, content: reply.clone() };
            let provider = MockProvider { reply };
            yield AssistantMessageEvent::Done(Box::new(provider.final_message(&model)));
        })
    }
}

// ---------------------------------------------------------------------------
// ScriptedProvider:每次 stream() 按顺序消费一个脚本 turn(多轮循环测试用)
// ---------------------------------------------------------------------------

/// 脚本的一个 turn:流式前的延迟 + 最终 assistant 消息。
pub struct ScriptedTurn {
    pub delay_ms: u64,
    pub message: AssistantMessage,
}

impl ScriptedTurn {
    pub fn new(message: AssistantMessage) -> Self {
        ScriptedTurn {
            delay_ms: 0,
            message,
        }
    }

    pub fn with_delay(mut self, delay_ms: u64) -> Self {
        self.delay_ms = delay_ms;
        self
    }

    /// 纯文本回复(stop)。
    pub fn text(model: &Model, text: impl Into<String>) -> Self {
        ScriptedTurn::new(assistant_message(
            model,
            vec![ContentBlock::text(text)],
            StopReason::Stop,
        ))
    }

    /// 工具调用回复(toolUse)。
    pub fn tool_calls(model: &Model, calls: Vec<ContentBlock>) -> Self {
        ScriptedTurn::new(assistant_message(model, calls, StopReason::ToolUse))
    }

    /// length 截断回复(验证截断防御)。
    pub fn truncated(model: &Model, calls: Vec<ContentBlock>) -> Self {
        ScriptedTurn::new(assistant_message(model, calls, StopReason::Length))
    }

    /// provider 错误(失败编码进流的终态)。
    pub fn error(model: &Model, message: impl Into<String>) -> Self {
        ScriptedTurn::new(AssistantMessage::error(model, message, false))
    }
}

/// 测试助手:构造最终态 assistant 消息。
pub fn assistant_message(
    model: &Model,
    content: Vec<ContentBlock>,
    stop_reason: StopReason,
) -> AssistantMessage {
    let mut message = AssistantMessage::pending(model);
    message.content = content;
    message.stop_reason = stop_reason;
    message
}

/// 按脚本逐 turn 出流的 provider:第 n 次 stream() 消费第 n 个脚本项;
/// 脚本耗尽后返回错误终态(测试可据此断言"循环不该再请求")。
pub struct ScriptedProvider {
    model: Model,
    turns: Mutex<VecDeque<ScriptedTurn>>,
}

impl ScriptedProvider {
    pub fn new(model: &Model, turns: Vec<ScriptedTurn>) -> Self {
        ScriptedProvider {
            model: model.clone(),
            turns: Mutex::new(turns.into()),
        }
    }

    pub fn remaining(&self) -> usize {
        self.turns.lock().unwrap().len()
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn stream(
        &self,
        _model: &Model,
        _ctx: TranscriptContext,
        opts: StreamOptions,
    ) -> AssistantMessageEventStream {
        let turn = self.turns.lock().unwrap().pop_front();
        let model = self.model.clone();
        let cancel = opts.cancel;
        Box::pin(async_stream::stream! {
            // 取消感知:退避/延迟中 abort → aborted 终态收尾(与真实适配器契约一致)
            let turn = match &cancel {
                Some(token) => {
                    let cancelled = turn.is_none() && token.is_cancelled();
                    if cancelled { None } else { turn }
                }
                None => turn,
            };
            let Some(turn) = turn else {
                if cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false) {
                    yield AssistantMessageEvent::Error(Box::new(AssistantMessage::error(
                        &model,
                        "aborted",
                        true,
                    )));
                    return;
                }
                yield AssistantMessageEvent::Error(Box::new(AssistantMessage::error(
                    &model,
                    "scripted provider exhausted",
                    false,
                )));
                return;
            };
            if turn.delay_ms > 0 {
                let sleep = tokio::time::sleep(std::time::Duration::from_millis(turn.delay_ms));
                if let Some(token) = &cancel {
                    tokio::select! {
                        _ = sleep => {},
                        _ = token.cancelled() => {
                            yield AssistantMessageEvent::Error(Box::new(AssistantMessage::error(
                                &model, "aborted", true,
                            )));
                            return;
                        }
                    }
                } else {
                    sleep.await;
                }
            }
            let message = turn.message;
            let terminal = if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                AssistantMessageEvent::Error(Box::new(message.clone()))
            } else {
                AssistantMessageEvent::Done(Box::new(message.clone()))
            };
            yield AssistantMessageEvent::Start;
            for (index, block) in message.content.iter().enumerate() {
                match block {
                    ContentBlock::Thinking { thinking, .. } => {
                        yield AssistantMessageEvent::ThinkingStart { content_index: index };
                        yield AssistantMessageEvent::ThinkingDelta { content_index: index, delta: thinking.clone() };
                        yield AssistantMessageEvent::ThinkingEnd { content_index: index, content: thinking.clone() };
                    }
                    ContentBlock::Text { text, .. } => {
                        yield AssistantMessageEvent::TextStart { content_index: index };
                        yield AssistantMessageEvent::TextDelta { content_index: index, delta: text.clone() };
                        yield AssistantMessageEvent::TextEnd { content_index: index, content: text.clone() };
                    }
                    ContentBlock::ToolCall { id, name, arguments } => {
                        let _ = id;
                        yield AssistantMessageEvent::ToolCallStart { content_index: index };
                        let partial = serde_json::to_string(arguments).unwrap_or_default();
                        yield AssistantMessageEvent::ToolCallDelta { content_index: index, delta: partial };
                        yield AssistantMessageEvent::ToolCallEnd {
                            content_index: index,
                            tool_call: ContentBlock::ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                arguments: arguments.clone(),
                            },
                        };
                    }
                    ContentBlock::Image { .. } => {}
                }
            }
            yield terminal;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test]
    async fn mock_stream_ends_with_done_and_matches_content() {
        let provider = MockProvider::new("你好,世界");
        let model = Model::minimal("mock-1", "mock", "mock");
        let mut stream = provider
            .stream(
                &model,
                TranscriptContext { messages: vec![] },
                StreamOptions::default(),
            )
            .await;
        let mut text = String::new();
        let mut terminal = None;
        while let Some(event) = stream.next().await {
            match event {
                AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(&delta),
                AssistantMessageEvent::Done(msg) => terminal = Some(*msg),
                AssistantMessageEvent::Error(err) => {
                    panic!("mock 不应失败: {}", err.error_message.unwrap_or_default())
                }
                _ => {}
            }
        }
        assert_eq!(text, "你好,世界");
        assert_eq!(terminal.unwrap().text_content(), "你好,世界");
    }
}
