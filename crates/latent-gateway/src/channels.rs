//! 渠道工厂 + 管理器:gateway 不认识任何具体渠道 —— 具体类型只在工厂
//! `create_channel` 出现(feature 门控);其余全部经 `ChannelPlugin` trait
//! 交互。启动/状态记录/回复句柄/DM 策略注册都在这里。

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};

use latent_channel::error::ChannelError;
use latent_channel::plugin::{ChannelHandle, ChannelPlugin};
use latent_channel::types::{ChannelEvent, ChannelStatus};

use crate::config::{DmPolicy, GatewayConfig};

/// 带渠道标签的事件(渠道入站/状态 → 宿主;状态事件本名不含渠道,
/// 由每渠道转发任务补名)。
#[derive(Debug)]
pub enum TaggedChannelEvent {
    Inbound {
        channel: String,
        message: latent_channel::types::InboundMessage,
    },
    Status {
        channel: String,
        status: ChannelStatus,
    },
}

/// 已启动渠道的运行时状态。
struct ChannelEntry {
    handle: Option<ChannelHandle>,
    account_id: Option<String>,
    status: String,
    chunk_limit: usize,
}

/// 渠道管理器(daemon 全局一份)。
pub struct ChannelManager {
    entries: RwLock<HashMap<String, ChannelEntry>>,
}

impl ChannelManager {
    pub fn new() -> Self {
        ChannelManager {
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// 启动配置里 enabled 的渠道。凭据已由 daemon 预解析进 config;
    /// 返回 (渠道 id, 启动错误) 列表(单渠道失败不阻断 daemon)。
    pub async fn start_enabled(
        &self,
        config: &GatewayConfig,
        raw_channels: &serde_json::Value,
        events_tx: mpsc::Sender<TaggedChannelEvent>,
    ) -> Vec<(String, ChannelError)> {
        let mut failures = Vec::new();
        let mut enabled: Vec<(&'static str, usize, serde_json::Value)> = Vec::new();
        if let Some(qq) = &config.channels.qq {
            if qq.enabled {
                enabled.push((
                    "qq",
                    qq.text_chunk_limit,
                    serde_json::to_value(qq).unwrap_or_default(),
                ));
            }
        }
        if let Some(wecom) = &config.channels.wecom {
            if wecom.enabled {
                enabled.push((
                    "wecom",
                    wecom.text_chunk_limit,
                    serde_json::to_value(wecom).unwrap_or_default(),
                ));
            }
        }
        if let Some(telegram) = &config.channels.telegram {
            if telegram.enabled {
                enabled.push((
                    "telegram",
                    telegram.text_chunk_limit,
                    serde_json::to_value(telegram).unwrap_or_default(),
                ));
            }
        }
        if let Some(mock) = &config.channels.mock {
            if mock.enabled {
                // mock 渠道无 textChunkLimit 键,用默认;create_channel 在
                // mock feature 未编译时返回 None → 显式"拒绝启动"
                enabled.push((
                    "mock",
                    4000,
                    serde_json::to_value(mock).unwrap_or_default(),
                ));
            }
        }
        for (id, chunk_limit, mut typed) in enabled {
            // raw merged config 优先(保留未类型化键,原样转发给渠道)
            if let Some(raw) = raw_channels.get(id) {
                if let (Some(base), Some(raw)) = (typed.as_object_mut(), raw.as_object()) {
                    for (key, value) in raw {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
            let Some(plugin) = create_channel(id) else {
                failures.push((
                    id.to_string(),
                    ChannelError::Config(format!(
                        "渠道 {id} 未编译进本二进制(feature 未启用)"
                    )),
                ));
                continue;
            };
            if let Err(error) = plugin.apply_config(&typed) {
                failures.push((id.to_string(), error));
                continue;
            }
            match self.attach(id, plugin, chunk_limit, events_tx.clone()).await {
                Ok(_handle) => {
                    // 条目已由 attach 登记
                }
                Err(error) => {
                    // 仍登记条目(status 展示失败态;handle 缺失 = 不回复)
                    self.entries.write().await.insert(
                        id.to_string(),
                        ChannelEntry {
                            handle: None,
                            account_id: None,
                            status: format!("failed: {error}"),
                            chunk_limit,
                        },
                    );
                    failures.push((id.to_string(), error));
                }
            }
        }
        failures
    }

    /// 注册并启动一个插件(每渠道转发任务补渠道名;工厂启动与
    /// mock/测试注入共用同一通路)。
    pub async fn attach(
        &self,
        id: &str,
        plugin: Arc<dyn ChannelPlugin>,
        chunk_limit: usize,
        events_tx: mpsc::Sender<TaggedChannelEvent>,
    ) -> Result<ChannelHandle, ChannelError> {
        let (tag_tx, mut tag_rx) = mpsc::channel::<ChannelEvent>(128);
        let handle = plugin.start(tag_tx).await?;
        self.entries.write().await.insert(
            id.to_string(),
            ChannelEntry {
                handle: Some(handle.clone()),
                account_id: None,
                status: "connected".into(),
                chunk_limit,
            },
        );
        let forwarder_id = id.to_string();
        let forwarder_tx = events_tx;
        tokio::spawn(async move {
            while let Some(event) = tag_rx.recv().await {
                let tagged = match event {
                    ChannelEvent::Inbound(message) => TaggedChannelEvent::Inbound {
                        channel: forwarder_id.clone(),
                        message,
                    },
                    ChannelEvent::Status(status) => TaggedChannelEvent::Status {
                        channel: forwarder_id.clone(),
                        status,
                    },
                };
                if forwarder_tx.send(tagged).await.is_err() {
                    break;
                }
            }
        });
        Ok(handle)
    }

    pub async fn handle(&self, channel: &str) -> Option<ChannelHandle> {
        self.entries
            .read()
            .await
            .get(channel)
            .and_then(|entry| entry.handle.clone())
    }

    pub async fn chunk_limit(&self, channel: &str) -> usize {
        self.entries
            .read()
            .await
            .get(channel)
            .map(|entry| entry.chunk_limit)
            .unwrap_or(4000)
    }

    /// 状态记录(事件循环收到带标签的状态事件时调用)。
    pub async fn record_status(&self, channel: &str, status: ChannelStatus) -> Option<String> {
        let mut entries = self.entries.write().await;
        let entry = entries.get_mut(channel)?;
        match status {
            ChannelStatus::Connected { account_id } => {
                entry.status = "connected".into();
                entry.account_id = Some(account_id.clone());
                Some(account_id)
            }
            ChannelStatus::Disconnected { reason } => {
                entry.status = format!("disconnected: {reason}");
                None
            }
            ChannelStatus::Failed { reason } => {
                entry.status = format!("failed: {reason}");
                None
            }
        }
    }

    /// 渠道的机器人账号 id 列表(mention gating 的 self_ids;启动早期为空)。
    pub async fn account_ids(&self, channel: &str) -> Vec<String> {
        self.entries
            .read()
            .await
            .get(channel)
            .and_then(|entry| entry.account_id.clone())
            .into_iter()
            .collect()
    }

    /// 渠道状态快照(status/channels.status 用)。
    pub async fn status_snapshot(&self) -> Vec<(String, String, Option<String>)> {
        self.entries
            .read()
            .await
            .iter()
            .map(|(id, entry)| (id.clone(), entry.status.clone(), entry.account_id.clone()))
            .collect()
    }

    /// 优雅断开(退出处理)。
    pub async fn shutdown_all(&self) {
        for entry in self.entries.read().await.values() {
            if let Some(handle) = &entry.handle {
                handle.shutdown().await;
            }
        }
    }

    /// DM 策略注册表(pairing 判定用;按渠道配置展开)。
    pub fn dm_policies(config: &GatewayConfig) -> Vec<(&'static str, DmPolicy, Vec<String>)> {
        let mut out = Vec::new();
        let entries = [
            (
                "qq",
                config
                    .channels
                    .qq
                    .as_ref()
                    .map(|c| (c.dm_policy.as_str(), &c.allow_from)),
            ),
            (
                "wecom",
                config
                    .channels
                    .wecom
                    .as_ref()
                    .map(|c| (c.dm_policy.as_str(), &c.allow_from)),
            ),
            (
                "telegram",
                config
                    .channels
                    .telegram
                    .as_ref()
                    .map(|c| (c.dm_policy.as_str(), &c.allow_from)),
            ),
            (
                "mock",
                config
                    .channels
                    .mock
                    .as_ref()
                    .map(|c| (c.dm_policy.as_str(), &c.allow_from)),
            ),
        ];
        for (id, policy) in entries {
            if let Some((policy, allow_from)) = policy {
                if let Ok(parsed) = DmPolicy::parse(policy) {
                    out.push((id, parsed, allow_from.clone()));
                }
            }
        }
        out
    }
}

impl Default for ChannelManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 渠道工厂:具体渠道类型唯一出现点(feature 门控;`mock` 供测试注入)。
pub fn create_channel(id: &str) -> Option<Arc<dyn ChannelPlugin>> {
    match id {
        #[cfg(feature = "qq")]
        "qq" => Some(Arc::new(latent_channel::qq::QqChannel::new())),
        #[cfg(feature = "wecom")]
        "wecom" => Some(Arc::new(latent_channel::wecom::WecomChannel::new())),
        #[cfg(feature = "telegram")]
        "telegram" => Some(Arc::new(latent_channel::telegram::TelegramChannel::new())),
        #[cfg(feature = "mock")]
        "mock" => Some(latent_channel::mock::MockChannel::new("mock") as Arc<dyn ChannelPlugin>),
        _ => None,
    }
}
