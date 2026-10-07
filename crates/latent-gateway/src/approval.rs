//! ChatApprovalUi(§4.9):gateway **全局唯一实例** —— owner 聊天句柄 +
//! pending 表(id → oneshot),id 全局自增。模板是 rpc 模式的 RpcApprovalUi
//! (id → oneshot 路由 + close_all 全部落 Deny)。
//!
//! 硬规则:
//! - id 一律全局自增号,**禁止短哈希** —— 跨会话碰撞会把批准路由到错误请求;
//! - 审批消息**发送失败** → 立即 Deny + 诊断,不挂 120s(fail-closed);
//! - 超时 120s / 渠道断开 → `None` → core 的 ApprovalHooks 语义 = Deny;
//! - 绝不允许按会话各建实例(id 碰撞 → 批准误路由)—— `/approve` 在全局
//!   命令层查**同一张** pending 表完成跨会话路由。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use latent_runtime::facade::{ApprovalDecision, ApprovalRequest, ApprovalUi};
use tokio::sync::{oneshot, Mutex, RwLock};

use latent_channel::types::{ChatRef, OutboundMessage, Sender};
use latent_channel::plugin::ChannelHandle;

/// 审批请求等待上限。
pub const APPROVAL_TIMEOUT_SECS: u64 = 120;
/// 审批消息发送超时(P2-3):渠道命令队列卡死时按发送失败处理,不无限
/// 挂住 run(发送失败 = 立即 Deny,fail-closed)。
const APPROVAL_SEND_TIMEOUT_SECS: u64 = 10;

/// 审批消息投递目标(owner 的私聊)。
#[derive(Clone)]
pub struct ApprovalTransport {
    pub handle: ChannelHandle,
    pub chat: ChatRef,
}

pub struct ChatApprovalUi {
    next_id: AtomicU64,
    /// 未决审批(oneshot + 工具名,供裸词应答路由与待审摘要展示)
    pending: Mutex<HashMap<u64, PendingApproval>>,
    transport: RwLock<Option<ApprovalTransport>>,
}

struct PendingApproval {
    tx: oneshot::Sender<ApprovalDecision>,
    tool_name: String,
}

impl ChatApprovalUi {
    pub fn new() -> Arc<Self> {
        Arc::new(ChatApprovalUi {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            transport: RwLock::new(None),
        })
    }

    /// 注入 owner DM 投递目标(渠道连接就绪后;替换 = 重连场景)。
    pub async fn set_transport(&self, transport: ApprovalTransport) {
        *self.transport.write().await = Some(transport);
    }

    pub async fn clear_transport(&self) {
        *self.transport.write().await = None;
    }

    /// 审批请求消息文本(命令面见 auto_reply/commands.rs 的 /approve)。
    fn format_request(id: u64, request: &ApprovalRequest) -> String {
        format!(
            "⏳ 需要批准 [{}] {}\n/approve {id} allow-once|allow-always|deny\n\
             (或直接回复 allow-once / allow-always / deny 应答本请求)",
            request.tool_name, request.detail
        )
    }

    /// `/approve` 应答路由(全局命令层调用)。
    pub async fn resolve(&self, id: u64, decision: ApprovalDecision) -> bool {
        match self.pending.lock().await.remove(&id) {
            Some(pending) => {
                let _ = pending.tx.send(decision);
                true
            }
            None => false,
        }
    }

    /// 最新未决审批 id(id 全局自增,最大 = 用户刚看到的那条;裸 decision
    /// 应答路由用)。
    pub async fn latest_pending_id(&self) -> Option<u64> {
        self.pending.lock().await.keys().copied().max()
    }

    /// 未决审批摘要((id, 工具名) 升序;裸 /approve 与过期应答提示用)。
    pub async fn pending_summary(&self) -> Vec<(u64, String)> {
        let mut items: Vec<(u64, String)> = self
            .pending
            .lock()
            .await
            .iter()
            .map(|(id, pending)| (*id, pending.tool_name.clone()))
            .collect();
        items.sort_by_key(|(id, _)| *id);
        items
    }

    /// 渠道断开/重启:所有未决审批落 Deny(oneshot 关闭 = None = Deny)。
    pub async fn close_all(&self) {
        self.pending.lock().await.clear();
    }

    /// 当前未决 id 快照(status/诊断用)。
    pub async fn pending_ids(&self) -> Vec<u64> {
        self.pending_summary().await.into_iter().map(|(id, _)| id).collect()
    }
}

impl Default for ChatApprovalUi {
    fn default() -> Self {
        ChatApprovalUi {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            transport: RwLock::new(None),
        }
    }
}

#[async_trait]
impl ApprovalUi for ChatApprovalUi {
    async fn request_approval(&self, request: ApprovalRequest) -> Option<ApprovalDecision> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(
            id,
            PendingApproval {
                tx,
                tool_name: request.tool_name.clone(),
            },
        );

        let body = Self::format_request(id, &request);
        let transport = self.transport.read().await.clone();
        let send_result = match transport {
            Some(transport) => {
                match tokio::time::timeout(
                    Duration::from_secs(APPROVAL_SEND_TIMEOUT_SECS),
                    transport
                        .handle
                        .send(&transport.chat, OutboundMessage::text(body)),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(latent_channel::error::ChannelError::DeliveryFailed(
                        format!("审批消息发送超时({APPROVAL_SEND_TIMEOUT_SECS}s)"),
                    )),
                }
            }
            None => Err(latent_channel::error::ChannelError::Startup(
                "审批通道未就绪(无 owner DM 投递目标)".into(),
            )),
        };
        if let Err(error) = send_result {
            // 发送失败 → 立即 Deny + 诊断,不等 120s(fail-closed,§5.2)
            eprintln!(
                "[latent-gateway][approval] 审批消息发送失败,自动拒绝 [{id}] {}: {error}",
                request.tool_name
            );
            self.pending.lock().await.remove(&id);
            return None;
        }

        match tokio::time::timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS), rx).await {
            Ok(Ok(decision)) => Some(decision),
            _ => {
                // 超时/通道关闭 → None = Deny(core ApprovalUi 语义)
                self.pending.lock().await.remove(&id);
                None
            }
        }
    }
}

/// 展示名兜底(空昵称 → user_id)。
pub fn display_name_or_id(sender: &Sender) -> &str {
    let name = sender.display_name.trim();
    if name.is_empty() {
        &sender.user_id
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use latent_channel::mock::MockChannel;
    use latent_channel::types::Sender;
    use latent_runtime::facade::{ApprovalReason, ToolRiskClass};

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            tool_call_id: "t1".into(),
            tool_name: "bash".into(),
            args: serde_json::json!({"command": "make test"}),
            risk: ToolRiskClass::Shell,
            reason: ApprovalReason::ShellCommand,
            detail: "make test".into(),
        }
    }

    fn owner_chat() -> ChatRef {
        ChatRef::private("mock", "owner-1")
    }

    #[tokio::test]
    async fn approve_roundtrip_routes_to_requester() {
        let channel = MockChannel::new("mock");
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let handle = latent_channel::plugin::ChannelPlugin::start(&*channel, tx).await.unwrap();
        let approval = ChatApprovalUi::new();
        approval
            .set_transport(ApprovalTransport {
                handle,
                chat: owner_chat(),
            })
            .await;

        let waiter = {
            let approval = approval.clone();
            tokio::spawn(async move { approval.request_approval(request()).await })
        };
        // 等审批消息发出
        channel.wait_for_outbound(1).await;
        let sent = &channel.sent()[0].1;
        let text = match &sent.segments[0] {
            latent_channel::types::Segment::Text(text) => text.clone(),
            other => panic!("应为文本段: {other:?}"),
        };
        assert!(text.contains("/approve 1 allow-once"), "{text}");
        assert!(approval.pending_ids().await.contains(&1));

        // owner 应答 allow-always → ApproveForSession
        assert!(
            approval
                .resolve(1, ApprovalDecision::ApproveForSession)
                .await
        );
        assert_eq!(waiter.await.unwrap(), Some(ApprovalDecision::ApproveForSession));
    }

    #[tokio::test]
    async fn send_failure_denies_immediately() {
        let channel = MockChannel::new("mock");
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let handle = latent_channel::plugin::ChannelPlugin::start(&*channel, tx).await.unwrap();
        let approval = ChatApprovalUi::new();
        approval
            .set_transport(ApprovalTransport {
                handle,
                chat: owner_chat(),
            })
            .await;
        channel.set_send_error(Some(latent_channel::error::ChannelError::DeliveryFailed(
            "boom".into(),
        )));

        let started = std::time::Instant::now();
        let decision = approval.request_approval(request()).await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "发送失败应立即返回,不挂 120s"
        );
        assert_eq!(decision, None, "None = Deny");
        assert!(approval.pending_ids().await.is_empty());
    }

    #[tokio::test]
    async fn no_transport_denies_immediately() {
        let approval = ChatApprovalUi::new();
        let started = std::time::Instant::now();
        let decision = approval.request_approval(request()).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(decision, None);
    }

    #[tokio::test]
    async fn close_all_denies_everything() {
        let channel = MockChannel::new("mock");
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let handle = latent_channel::plugin::ChannelPlugin::start(&*channel, tx).await.unwrap();
        let approval = ChatApprovalUi::new();
        approval
            .set_transport(ApprovalTransport {
                handle,
                chat: owner_chat(),
            })
            .await;
        let waiter = {
            let approval = approval.clone();
            tokio::spawn(async move { approval.request_approval(request()).await })
        };
        channel.wait_for_outbound(1).await;
        approval.close_all().await;
        assert_eq!(waiter.await.unwrap(), None);
    }

    #[test]
    fn sender_display_falls_back_to_user_id() {
        let sender = Sender {
            user_id: "u1".into(),
            display_name: String::new(),
        };
        assert_eq!(display_name_or_id(&sender), "u1");
    }
}

