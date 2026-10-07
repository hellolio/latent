//! 渠道插件接缝(收敛自 OpenClaw 的适配器集合形态:config/outbound/status/
//! mentions/typing 的 trait 面)。出站经 mpsc 命令通道串行消费 —— 串行化
//! 天然获得,保序由 gateway 侧 reply_dispatcher 保证。
//!
//! **panic 边界(硬要求)**:实现内部必须捕获自身任务 panic,降级为
//! `Status::Failed` 发给宿主,绝不允许 panic 击穿 gateway 进程 —— 用
//! [`spawn_guarded`] 包裹渠道内部任务。

use std::sync::Arc;

use async_trait::async_trait;
use futures::FutureExt;
use tokio::sync::{mpsc, oneshot, watch};

use crate::error::ChannelError;
use crate::types::{ChatRef, ChannelEvent, ChannelStatus, OutboundMessage};

/// 渠道内部命令(`ChannelSender` 推给渠道任务消费)。
#[derive(Debug)]
pub enum ChannelCommand {
    Send {
        chat: ChatRef,
        message: OutboundMessage,
        reply: oneshot::Sender<Result<(), ChannelError>>,
    },
    Typing {
        chat: ChatRef,
        on: bool,
    },
    Shutdown,
}

/// 出站发送句柄(clone 便宜;命令通道有界,发送方 await 背压)。
#[derive(Clone)]
pub struct ChannelSender {
    tx: mpsc::Sender<ChannelCommand>,
}

impl ChannelSender {
    pub fn new(tx: mpsc::Sender<ChannelCommand>) -> Self {
        ChannelSender { tx }
    }

    /// 出站消息;返回渠道侧投递结果(含平台限速/未在群等分类)。
    pub async fn send(
        &self,
        chat: &ChatRef,
        message: OutboundMessage,
    ) -> Result<(), ChannelError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ChannelCommand::Send {
                chat: chat.clone(),
                message,
                reply: reply_tx,
            })
            .await
            .map_err(|_| {
                ChannelError::DeliveryFailed("channel command task stopped".into())
            })?;
        reply_rx.await.map_err(|_| {
            ChannelError::DeliveryFailed("channel command task dropped reply".into())
        })?
    }

    /// typing 指示器(fire-and-forget;平台不支持时渠道侧 no-op,对齐
    /// OpenClaw:不支持渠道静默抑制)。
    pub async fn typing(&self, chat: &ChatRef, on: bool) {
        let _ = self
            .tx
            .send(ChannelCommand::Typing {
                chat: chat.clone(),
                on,
            })
            .await;
    }

    /// 请求渠道任务退出(优雅断开)。
    pub async fn shutdown(&self) {
        let _ = self.tx.send(ChannelCommand::Shutdown).await;
    }

    /// 渠道命令任务是否已停止(typing keepalive 据此计连续失败)。
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// 渠道句柄:`start` 的产物,gateway 持有后即可发送/typing/查状态。
#[derive(Clone)]
pub struct ChannelHandle {
    sender: ChannelSender,
    status: watch::Receiver<ChannelStatus>,
}

impl ChannelHandle {
    pub fn new(sender: ChannelSender, status: watch::Receiver<ChannelStatus>) -> Self {
        ChannelHandle { sender, status }
    }

    pub async fn send(
        &self,
        chat: &ChatRef,
        message: OutboundMessage,
    ) -> Result<(), ChannelError> {
        self.sender.send(chat, message).await
    }

    pub async fn typing(&self, chat: &ChatRef, on: bool) {
        self.sender.typing(chat, on).await;
    }

    pub async fn status(&self) -> ChannelStatus {
        self.status.borrow().clone()
    }

    pub fn sender(&self) -> ChannelSender {
        self.sender.clone()
    }

    pub async fn shutdown(&self) {
        self.sender.shutdown().await;
    }
}

/// 渠道插件trait。`start` 自持连接/重连/登录态,入站事件推 `tx`。
#[async_trait]
pub trait ChannelPlugin: Send + Sync {
    /// 渠道 id:"qq" | "wecom" | "telegram"(== config.rs channels 节的键名)
    fn id(&self) -> &'static str;

    /// 启动渠道:返回出站句柄;入站事件/状态变化推 `tx`。
    async fn start(&self, tx: mpsc::Sender<ChannelEvent>)
        -> Result<ChannelHandle, ChannelError>;

    /// 渠道配置注入(gateway 按渠道 id 分发原始 JSON,各渠道自己反序列化
    /// 自己的 Config struct —— gateway 不认识任何具体渠道)。错误一律类型化
    /// ChannelError;插件实现自持内部可变性(Arc<Inner> + RwLock<Config>),
    /// trait 方法走 &self。
    fn apply_config(&self, raw: &serde_json::Value) -> Result<(), ChannelError>;
}

/// 渠道内部任务的 panic 边界:任务 panic → 捕获 → `Status::Failed` 推给
/// 宿主(诊断),绝不击穿 daemon 进程。所有渠道 `start` 出来的长驻任务都
/// 应经本助手运行。
pub fn spawn_guarded<F>(label: &'static str, tx: mpsc::Sender<ChannelEvent>, fut: F)
where
    F: futures::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let result = std::panic::AssertUnwindSafe(fut).catch_unwind().await;
        if let Err(panic) = result {
            let reason = panic_message(&panic);
            eprintln!("[latent-channel:{label}] task panicked: {reason}");
            let _ = tx
                .send(ChannelEvent::Status(ChannelStatus::Failed {
                    reason: format!("task panicked: {reason}"),
                }))
                .await;
        }
    });
}

/// panic 载荷 → 可读文本(panic 消息是 `&str`/`String` 二选一)。
pub fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = panic.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = panic.downcast_ref::<String>() {
        text.clone()
    } else {
        "unknown panic payload".into()
    }
}

/// 渠道任务通用骨架:消费出站命令(`Send`/`Typing`/`Shutdown`),平台事件
/// 由实现自行 select。供各渠道复用,保证出站命令串行。
pub async fn run_command_loop<S, F, Fut>(
    mut rx: mpsc::Receiver<ChannelCommand>,
    mut on_send: S,
    mut on_typing: F,
) where
    S: FnMut(ChatRef, OutboundMessage, oneshot::Sender<Result<(), ChannelError>>) -> Fut,
    F: FnMut(ChatRef, bool) + Send,
    Fut: std::future::Future<Output = ()> + Send,
{
    while let Some(command) = rx.recv().await {
        match command {
            ChannelCommand::Send {
                chat,
                message,
                reply,
            } => on_send(chat, message, reply).await,
            ChannelCommand::Typing { chat, on } => on_typing(chat, on),
            ChannelCommand::Shutdown => break,
        }
    }
}

/// watch 状态通道的初始值(启动中 = Disconnected,重连由渠道自行翻转)。
pub fn initial_status() -> (watch::Sender<ChannelStatus>, watch::Receiver<ChannelStatus>) {
    watch::channel(ChannelStatus::Disconnected {
        reason: "starting".into(),
    })
}

/// 供测试/网关断言的 Arc 别名(channel 内部多处共享 inner)。
pub type Shared<T> = Arc<T>;
