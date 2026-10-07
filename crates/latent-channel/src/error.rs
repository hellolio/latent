//! 渠道错误面(对齐 OpenClaw ReplyMediaFailure 的语义面):出站失败分类 +
//! 生命周期/配置错误共用同一类型化错误面(§9:thiserror 向上传播,禁止
//! `Result<_, String>`)。

use std::time::Duration;

use thiserror::Error;

#[derive(Debug, Error, Clone)]
pub enum ChannelError {
    #[error("chat not found")]
    ChatNotFound,
    #[error("not in group")]
    NotInGroup,
    #[error("rate limited (retry after {retry_after_ms}ms)")]
    RateLimited { retry_after_ms: u64 },
    #[error("unsupported operation on this channel")]
    Unsupported,
    #[error("delivery failed: {0}")]
    DeliveryFailed(String),
    /// 配置缺失/非法(含凭据解析失败)→ 渠道拒绝启动
    #[error("channel config error: {0}")]
    Config(String),
    /// 连接/握手失败 → 宿主据此决定重试或 Status::Failed
    #[error("channel startup failed: {0}")]
    Startup(String),
}

impl ChannelError {
    /// 平台限速给出的重试建议(reply_dispatcher 据此延迟重试)。
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            ChannelError::RateLimited { retry_after_ms } => {
                Some(Duration::from_millis(*retry_after_ms))
            }
            _ => None,
        }
    }

    /// 出站可重试类(限速/瞬时投递失败);配置与生命周期错误不重试。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            ChannelError::RateLimited { .. } | ChannelError::DeliveryFailed(_)
        )
    }
}
