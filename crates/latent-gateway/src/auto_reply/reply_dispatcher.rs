//! 出站保序(上游 `src/auto-reply/reply/reply-dispatcher.ts` —— 语义逐条
//! 对照):
//!
//! - 所有出站(工具进度/最终回复)串在同一条 **sendChain**(本模块的
//!   mpsc 消费任务)上,保持 tool → block → final 顺序;
//! - **最终回复只发一次**(AgentSettled 定稿时一次性产出,缓冲即取即清);
//! - 长文经 `latent_channel::chunk` 分段后顺序发送(段间可配延时;首段不延迟);
//! - 平台发送失败:按 `ChannelError` 分类诊断/限速重试(对齐上游
//!   `ReplyMediaFailure` 的 code 面)。

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use latent_channel::chunk::{chunk_text, ChunkMode, ChunkOptions};
use latent_channel::error::ChannelError;
use latent_channel::plugin::ChannelHandle;
use latent_channel::types::{ChatRef, OutboundMessage};

use crate::control::events::GatewayEvent;

/// sendChain 上的出站项。
#[derive(Debug, Clone)]
pub enum OutboundItem {
    /// 工具进度/重试通知(不 chunk)
    Progress(String),
    /// 最终回复(chunk 分段后顺序发送)
    Final(String),
}

/// 单段发送兜底超时(P2-16):渠道发送悬挂(qq/wecom WS 对端黑洞)时,
/// sendChain 消费任务被永久卡住 → send_final 阻塞 → pump/会话串行广播
/// 停摆 → run_lock 永久被持,会话后续消息全部入队直至 cap 丢弃。fail-closed
/// 是计划取舍,但"永久卡死"必须有界 —— 超时记诊断后放行下一段。
const SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// 回复出口:渠道私聊/群聊,或控制面事件线(chat.send 的 operator 会话)。
#[derive(Clone)]
pub enum ReplySink {
    Channel { handle: ChannelHandle, chat: ChatRef },
    /// 最终回复经 GatewayEvent::Agent 上控制面事件线
    ControlPlane,
}

/// 回复目标(每条 run 由触发消息决定;同会话后到覆盖先到)。
#[derive(Clone)]
pub struct ReplyTarget {
    pub sink: ReplySink,
    /// 渠道 textChunkLimit(QQ 2000 / 企微 2048 / TG 4000)
    pub chunk_limit: usize,
}

/// 每会话一个 dispatcher:handle + sendChain 消费任务。
pub struct ReplyDispatcher {
    tx: mpsc::Sender<OutboundItem>,
    target: std::sync::Arc<tokio::sync::RwLock<Option<ReplyTarget>>>,
}

/// 控制面事件出口(每会话一份:session_key + 广播句柄)。
#[derive(Clone)]
pub struct EventSink {
    pub events: tokio::sync::broadcast::Sender<GatewayEvent>,
    pub session_key: String,
}

impl ReplyDispatcher {
    /// 创建并启动 sendChain 消费任务(每会话一个,随 ChatSession 生存)。
    pub fn spawn(segment_delay: Duration, event_sink: Option<EventSink>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<OutboundItem>(256);
        let target: Arc<tokio::sync::RwLock<Option<ReplyTarget>>> =
            Arc::new(tokio::sync::RwLock::new(None));
        let consumer_target = target.clone();
        let shared_sink = Arc::new(std::sync::RwLock::new(event_sink));
        tokio::spawn(async move {
            let mut rx = rx;
            let mut first_in_batch = true;
            while let Some(item) = rx.recv().await {
                let target = consumer_target.read().await.clone();
                let Some(target) = target else {
                    eprintln!(
                        "[latent-gateway][reply] 出站丢弃(无回复目标): {item:?}"
                    );
                    continue;
                };
                let is_final = matches!(item, OutboundItem::Final(_));
                let items: Vec<String> = match item {
                    OutboundItem::Progress(text) => vec![text],
                    OutboundItem::Final(text) => chunk_text(
                        &text,
                        ChunkOptions {
                            limit: target.chunk_limit.max(1),
                            mode: ChunkMode::Length,
                            markdown: true,
                        },
                    ),
                };
                for (index, piece) in items.into_iter().enumerate() {
                    if piece.trim().is_empty() {
                        continue;
                    }
                    // human delay:首块不延迟,续块按配置间隔(上游仅 block)
                    if !first_in_batch || index > 0 {
                        tokio::time::sleep(segment_delay).await;
                    }
                    match &target.sink {
                        ReplySink::Channel { handle, chat } => {
                            // P2-16:单段发送加兜底超时,悬挂不放行整条链
                            let send = handle
                                .send(chat, OutboundMessage::text(piece.clone()));
                            match tokio::time::timeout(SEND_TIMEOUT, send).await {
                                Ok(Ok(())) => {}
                                Ok(Err(error)) => {
                                    report_send_failure(&error);
                                    // 限速:按 retry_after 延迟后重试一次
                                    if let Some(delay) = error.retry_after() {
                                        tokio::time::sleep(delay).await;
                                        let _ = handle
                                            .send(chat, OutboundMessage::text(piece.clone()))
                                            .await;
                                    }
                                }
                                Err(_) => {
                                    eprintln!(
                                        "[latent-gateway][reply] 发送悬挂超时({}s),放弃本段放行下一段",
                                        SEND_TIMEOUT.as_secs()
                                    );
                                }
                            }
                        }
                        ReplySink::ControlPlane => {
                            // 上控制面事件线(无订阅者即丢,broadcast 语义)
                            if let Some(event) = shared_sink.read().unwrap().as_ref() {
                                let _ = event.events.send(GatewayEvent::Agent {
                                    session_key: event.session_key.clone(),
                                    kind: if is_final {
                                        "final".to_string()
                                    } else {
                                        "progress".to_string()
                                    },
                                    text: piece,
                                });
                            }
                        }
                    }
                    first_in_batch = false;
                }
            }
        });
        Arc::new(ReplyDispatcher { tx, target })
    }

    /// 设置当前回复目标(每条 run 开始时由触发消息覆盖)。
    pub async fn set_target(&self, target: ReplyTarget) {
        *self.target.write().await = Some(target);
    }

    /// operator 会话(chat.send):回复上控制面事件线。
    pub async fn set_target_control_plane(&self) {
        *self.target.write().await = Some(ReplyTarget {
            sink: ReplySink::ControlPlane,
            chunk_limit: 30_000,
        });
    }

    pub async fn clear_target(&self) {
        *self.target.write().await = None;
    }

    /// 进度类:try_send(通道满即丢,fail-open —— 丢进度可接受)。
    pub fn try_progress(&self, text: String) {
        let _ = self.tx.try_send(OutboundItem::Progress(text));
    }

    /// 最终/回执类:await(阻塞 pump 也不丢,fail-closed)。
    pub async fn send_final(&self, text: String) {
        let _ = self.tx.send(OutboundItem::Final(text)).await;
    }
}

/// 出站失败分类诊断(对齐上游 ReplyMediaFailure 的 code 面)。
fn report_send_failure(error: &ChannelError) {
    let code = match error {
        ChannelError::ChatNotFound => "chat-not-found",
        ChannelError::NotInGroup => "not-in-group",
        ChannelError::RateLimited { .. } => "rate-limited",
        ChannelError::Unsupported => "unsupported-format",
        ChannelError::DeliveryFailed(_) => "delivery-failed",
        ChannelError::Config(_) | ChannelError::Startup(_) => "channel-unavailable",
    };
    eprintln!("[latent-gateway][reply] 发送失败({code}): {error}");
}
