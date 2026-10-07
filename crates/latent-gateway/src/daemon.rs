//! daemon 事件循环与审批通道装配(自 bin main.rs 下沉,便于回归测试):
//! 渠道入站 → dispatch;状态变化 → 记录/审批通道刷新/广播。

use std::sync::Arc;

use tokio::sync::mpsc;

use latent_channel::types::ChannelStatus;

use crate::auto_reply::Gateway;
use crate::channels::TaggedChannelEvent;
use crate::control::events::GatewayEvent;

/// 主事件循环。**防御分支**:事件通道被全部关闭(recv 返回 None)时不再
/// 静默退出 —— 打警告后挂住,Ctrl-C 仍可优雅停机。回归背景:此前所有渠道
/// 启动失败时发送端全部 drop,主循环经此路径在"已就绪"后瞬间静默退出。
/// daemon 侧同时保留一个 keep-alive 发送端,让本分支在正常部署中不可达。
pub async fn run_event_loop(
    gateway: Arc<Gateway>,
    mut events_rx: mpsc::Receiver<TaggedChannelEvent>,
) {
    loop {
        tokio::select! {
            event = events_rx.recv() => match event {
                Some(TaggedChannelEvent::Inbound { message, .. }) => {
                    let gateway = gateway.clone();
                    tokio::spawn(async move {
                        gateway.dispatch_inbound(message).await;
                    });
                }
                Some(TaggedChannelEvent::Status { channel, status }) => {
                    let event_status = match &status {
                        ChannelStatus::Connected { .. } => "connected".to_string(),
                        ChannelStatus::Disconnected { reason } => {
                            format!("disconnected: {reason}")
                        }
                        ChannelStatus::Failed { reason } => format!("failed: {reason}"),
                    };
                    gateway.channels.record_status(&channel, status.clone()).await;
                    // 渠道断开/重启 → 未决审批全部落 Deny(fail-closed,§4.9)
                    if matches!(
                        status,
                        ChannelStatus::Disconnected { .. } | ChannelStatus::Failed { .. }
                    ) {
                        gateway.approval.close_all().await;
                    }
                    refresh_approval_transport(&gateway).await;
                    let _ = gateway.events.send(GatewayEvent::Channels {
                        channel,
                        status: event_status,
                    });
                }
                None => {
                    eprintln!(
                        "[latent-gateway][warn] 渠道事件通道已关闭(防御分支,不应发生):daemon 保持运行,仅控制面可用"
                    );
                    std::future::pending::<()>().await;
                }
            },
            _ = tokio::signal::ctrl_c() => {
                eprintln!("[latent-gateway] 收到退出信号,开始优雅停机…");
                break;
            }
        }
    }
}

/// 审批通道刷新:owner DM(第一个 ownerAllowFrom 中渠道**实际连接中**的项;
/// P2-4:attach 过的 handle 存在 ≠ 连接,断开的渠道会让审批钉死在死渠道)。
pub async fn refresh_approval_transport(gateway: &Gateway) {
    for owner in &gateway.config.commands.owner_allow_from {
        let Some((channel, user_id)) = owner.split_once(':') else {
            continue;
        };
        if let Some(handle) = gateway.channels.handle(channel).await {
            if !matches!(handle.status().await, ChannelStatus::Connected { .. }) {
                continue;
            }
            gateway
                .approval
                .set_transport(crate::approval::ApprovalTransport {
                    handle,
                    chat: latent_channel::types::ChatRef::private(
                        channel_static(channel),
                        user_id.to_string(),
                    ),
                })
                .await;
            eprintln!("[latent-gateway] 审批通道 → {owner} 的私聊");
            return;
        }
    }
    eprintln!(
        "[latent-gateway][warn] 无可用审批通道(检查 ownerAllowFrom 与渠道连接);审批请求将自动拒绝(fail-closed)"
    );
}

/// 渠道 id → 静态串(已知 id 直返;未知 id 泄漏进静态区 —— 配置错误路径,
/// 一次性代价可接受)。
fn channel_static(channel: &str) -> &'static str {
    match channel {
        "qq" => "qq",
        "wecom" => "wecom",
        "telegram" => "telegram",
        "mock" => "mock",
        _ => Box::leak(channel.to_string().into_boxed_str()),
    }
}
