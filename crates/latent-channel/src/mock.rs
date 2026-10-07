//! 内存版假渠道(`mock` feature):`start` 返回可编程 handle —— 测试注入
//! InboundMessage、断言 Outbound、可编程发送失败/状态翻转。gateway 集成
//! 测试的基础设施(不经网络,全离线)。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::ChannelError;
use crate::plugin::{
    initial_status, run_command_loop, spawn_guarded, ChannelCommand, ChannelHandle,
    ChannelPlugin, ChannelSender,
};
use crate::types::{ChatRef, ChannelEvent, ChannelStatus, InboundMessage, OutboundMessage};

struct Inner {
    config: Option<serde_json::Value>,
    events_tx: Option<mpsc::Sender<ChannelEvent>>,
    sent: Vec<(ChatRef, OutboundMessage)>,
    typing: Vec<(ChatRef, bool)>,
    status: ChannelStatus,
    send_error: Option<ChannelError>,
}

impl Default for Inner {
    fn default() -> Self {
        Inner {
            config: None,
            events_tx: None,
            sent: Vec::new(),
            typing: Vec::new(),
            status: ChannelStatus::Disconnected {
                reason: "not started".into(),
            },
            send_error: None,
        }
    }
}

/// 可编程内存渠道。测试流程:`MockChannel::new("mock")` → `apply_config`
/// (可选)→ `start(tx)` → `push_inbound(...)` 注入 → `wait_for_outbound`
/// 后断言 `sent()`。
pub struct MockChannel {
    id: &'static str,
    inner: Arc<Mutex<Inner>>,
}

impl MockChannel {
    pub fn new(id: &'static str) -> Arc<Self> {
        Arc::new(MockChannel {
            id,
            inner: Arc::new(Mutex::new(Inner::default())),
        })
    }

    /// 注入一条入站消息(走与真实渠道相同的 ChannelEvent 通路)。
    pub async fn push_inbound(&self, message: InboundMessage) {
        let tx = self
            .inner
            .lock()
            .unwrap()
            .events_tx
            .clone()
            .expect("push_inbound before start(): 先调用 ChannelPlugin::start");
        let _ = tx.send(ChannelEvent::Inbound(message)).await;
    }

    /// 注入一条状态事件(连接断开/失败模拟)。
    pub async fn push_status(&self, status: ChannelStatus) {
        let tx = self
            .inner
            .lock()
            .unwrap()
            .events_tx
            .clone()
            .expect("push_status before start(): 先调用 ChannelPlugin::start");
        let _ = tx.send(ChannelEvent::Status(status)).await;
    }

    /// 可编程出站失败(下一条 send 返回该错误)。
    pub fn set_send_error(&self, error: Option<ChannelError>) {
        self.inner.lock().unwrap().send_error = error;
    }

    pub fn sent(&self) -> Vec<(ChatRef, OutboundMessage)> {
        self.inner.lock().unwrap().sent.clone()
    }

    pub fn typing_events(&self) -> Vec<(ChatRef, bool)> {
        self.inner.lock().unwrap().typing.clone()
    }

    pub fn status(&self) -> ChannelStatus {
        self.inner.lock().unwrap().status.clone()
    }

    pub fn config(&self) -> Option<serde_json::Value> {
        self.inner.lock().unwrap().config.clone()
    }

    /// 等待 typing 事件累计达到 `min` 条(测试编排;超时 panic)。
    pub async fn wait_for_typing(&self, min: usize) {
        for _ in 0..500 {
            if self.typing_events().len() >= min {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "等待 typing 事件超时:期望 >= {min},实际 {}",
            self.typing_events().len()
        );
    }

    /// 等待累计出站消息达到 `min` 条(测试编排;超时 panic 给出当前计数)。
    pub async fn wait_for_outbound(&self, min: usize) {
        for _ in 0..500 {
            if self.sent().len() >= min {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "等待出站消息超时:期望 >= {min},实际 {}",
            self.sent().len()
        );
    }
}

#[async_trait]
impl ChannelPlugin for MockChannel {
    fn id(&self) -> &'static str {
        self.id
    }

    fn apply_config(&self, raw: &serde_json::Value) -> Result<(), ChannelError> {
        self.inner.lock().unwrap().config = Some(raw.clone());
        Ok(())
    }

    async fn start(
        &self,
        tx: mpsc::Sender<ChannelEvent>,
    ) -> Result<ChannelHandle, ChannelError> {
        let (command_tx, command_rx) = mpsc::channel::<ChannelCommand>(64);
        let (status_tx, status_rx) = initial_status();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.events_tx = Some(tx.clone());
            inner.status = ChannelStatus::Connected {
                account_id: "mock-account".into(),
            };
        }
        let _ = status_tx.send(ChannelStatus::Connected {
            account_id: "mock-account".into(),
        });

        let send_inner = self.inner.clone();
        let typing_inner = self.inner.clone();
        spawn_guarded(
            "mock",
            tx,
            run_command_loop(
                command_rx,
                move |chat, message, reply| {
                    let inner = send_inner.clone();
                    async move {
                        let error = inner.lock().unwrap().send_error.clone();
                        let result = match error {
                            Some(error) => Err(error),
                            None => Ok(()),
                        };
                        inner.lock().unwrap().sent.push((chat, message));
                        let _ = reply.send(result);
                    }
                },
                move |chat, on| {
                    typing_inner.lock().unwrap().typing.push((chat, on));
                },
            ),
        );
        Ok(ChannelHandle::new(ChannelSender::new(command_tx), status_rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatType, Sender};

    fn dm(text: &str) -> InboundMessage {
        InboundMessage::plain_text(
            "mock",
            ChatRef::new(ChatType::Private, "u1"),
            Sender {
                user_id: "u1".into(),
                display_name: "u".into(),
            },
            "m-1",
            text,
            true,
        )
    }

    #[tokio::test]
    async fn roundtrip_inbound_outbound_and_typing() {
        let channel = MockChannel::new("mock");
        let (tx, mut rx) = mpsc::channel(16);
        let handle = ChannelPlugin::start(&*channel, tx).await.unwrap();

        channel.push_inbound(dm("你好")).await;
        match rx.recv().await.unwrap() {
            ChannelEvent::Inbound(msg) => assert_eq!(msg.text, "你好"),
            other => panic!("应为 Inbound 事件: {other:?}"),
        }

        let chat = ChatRef::new(ChatType::Private, "u1");
        handle.send(&chat, OutboundMessage::text("回复")).await.unwrap();
        handle.typing(&chat, true).await;
        channel.wait_for_outbound(1).await;
        assert_eq!(channel.sent()[0].1.segments[0], crate::types::Segment::text("回复"));
        channel.wait_for_typing(1).await;
        assert_eq!(channel.typing_events(), vec![(chat.clone(), true)]);

        // 状态查询
        assert_eq!(
            handle.status().await,
            ChannelStatus::Connected {
                account_id: "mock-account".into()
            }
        );
    }

    #[tokio::test]
    async fn send_error_is_programmable() {
        let channel = MockChannel::new("mock");
        let (tx, _rx) = mpsc::channel(16);
        let handle = ChannelPlugin::start(&*channel, tx).await.unwrap();
        channel.set_send_error(Some(ChannelError::NotInGroup));
        let error = handle
            .send(&ChatRef::new(ChatType::Group, "g1"), OutboundMessage::text("x"))
            .await
            .unwrap_err();
        assert!(matches!(error, ChannelError::NotInGroup));
    }
}
