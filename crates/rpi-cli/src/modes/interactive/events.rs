//! UI 事件通道:session 事件 + 扩展 UI 调用 + 后台任务回写,共用同一
//! `mpsc` 通道,由事件循环统一消费渲染(接缝 #5 的 interactive 侧)。

use async_trait::async_trait;
use rpi_core::{AgentSessionEvent, ExtensionUi, SessionSubscriber};
use tokio::sync::{mpsc, oneshot};

pub enum UiEvent {
    Session(AgentSessionEvent),
    Notify(String),
    /// 后台任务回写状态行(compact 等不再内联 await 的命令)
    Status(String),
    /// /compact 完成(结果经后台任务回流;成功后 ctx% 估计重置)
    CompactDone(Result<usize, String>),
    /// `!` bash 执行完成(后台任务回流;inject = 是否进上下文)
    BashDone {
        command: String,
        output: String,
        exit_code: Option<i32>,
        is_error: bool,
        inject: bool,
    },
    Confirm {
        message: String,
        responder: oneshot::Sender<bool>,
    },
    Select {
        message: String,
        options: Vec<String>,
        responder: oneshot::Sender<Option<usize>>,
    },
}

/// interactive 模式的 `ExtensionUi` 真实现:把 UI 调用发进主循环渲染。
/// 通道在装配期创建(扩展 init 可能就会调 UI),主循环消费。
#[derive(Clone)]
pub struct TuiUi {
    pub(crate) tx: mpsc::UnboundedSender<UiEvent>,
}

/// 创建 interactive 模式的 UI 通道:装配期把 `TuiUi` 传给 build_session,
/// 运行期把 receiver 交给 `run_interactive_mode`。
pub fn create_tui_ui() -> (TuiUi, mpsc::UnboundedReceiver<UiEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (TuiUi { tx }, rx)
}

#[async_trait]
impl ExtensionUi for TuiUi {
    async fn notify(&self, message: &str) {
        let _ = self.tx.send(UiEvent::Notify(message.to_string()));
    }

    async fn confirm(&self, message: &str) -> bool {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(UiEvent::Confirm {
            message: message.to_string(),
            responder: tx,
        });
        // 应答者被取消(客户端断开/请求被丢弃)= 未获确认 → 默认拒绝,
        // 不 fail-open
        rx.await.unwrap_or(false)
    }

    async fn select(&self, message: &str, options: &[String]) -> Option<usize> {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(UiEvent::Select {
            message: message.to_string(),
            options: options.to_vec(),
            responder: tx,
        });
        rx.await.unwrap_or(None)
    }

    async fn input(&self, message: &str) -> Option<String> {
        // 自由文本输入以状态行提示(简化;选择类 UI 已完整)
        let _ = self.tx.send(UiEvent::Notify(format!("[input] {message}")));
        None
    }
}

pub(crate) struct SessionToUiSubscriber {
    pub tx: mpsc::UnboundedSender<UiEvent>,
}

#[async_trait]
impl SessionSubscriber for SessionToUiSubscriber {
    async fn on_session_event(&self, event: &AgentSessionEvent) {
        let _ = self.tx.send(UiEvent::Session(event.clone()));
    }
}
