//! ChatSessionRegistry(§4.4):每 session key 一个 `ChatSession`
//! (惰性创建 —— 第一条消息到达才 `build_session`)。
//!
//! **资源模型(写进用户文档)**:每会话 = 1 个 MCP 子进程 × spec 数 +
//! 1 个 PermissionEngine + 1 个 JSONL 文件。MVP 无会话回收(二期 LRU)。
//!
//! 事件泵(§4.5):每个 BuiltSession 挂一个 `SessionEventPump`
//! (`SessionSubscriber`),把 `AgentSessionEvent` 推进该会话的 sendChain
//! —— **订阅者只推 channel,绝不在回调里做慢 IO**(core 的事件分发是
//! 串行 await 的);通道有界,进度类满即丢,final 类宁可阻塞 pump 也不丢
//! (fail-closed)。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use latent_agent::{AgentEvent, MessageDeltaPayload};
use latent_core::{
    AgentSessionEvent, ApprovalUi, McpServerSpec, NoopUi, SessionSharedSubscriber,
    SessionSubscriber,
};
use latent_runtime::assembly::{
    build_session, BuiltSession, BuildOptions, SessionSettings, SessionStore,
};
use tokio::sync::Mutex as AsyncMutex;

use latent_channel::plugin::ChannelHandle;
use latent_channel::typing::{TypingConfig, TypingGuard};
use latent_channel::types::ChatRef;

use crate::auto_reply::queue::PendingQueue;
use crate::auto_reply::reply_dispatcher::{OutboundItem, ReplyDispatcher, ReplyTarget};
use crate::state::{now_ms, StateStore};

/// 进度通知节流间隔(默认 30s 一条「仍在工作…」)。
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// 会话工厂:全局解析一次共享(provider/settings/MCP specs),
/// PermissionEngine/MCP 子进程/会话文件每会话独立(`build_session` 既有语义)。
pub struct SessionFactory {
    pub provider: Arc<dyn latent_ai::Provider>,
    pub model: latent_ai::Model,
    pub settings: SessionSettings,
    /// 会话文件目录(`<数据目录>/sessions`)
    pub sessions_dir: std::path::PathBuf,
    /// MCP specs(全局解析一次)
    pub extension_specs: Vec<McpServerSpec>,
    /// 审批 UI(gateway 全局单例,clone 注入每个会话)
    pub approval_ui: Arc<dyn ApprovalUi>,
    /// state.json 句柄(session key → 文件路径持久化)
    pub state: Arc<StateStore>,
    /// typing 配置
    pub typing_config: TypingConfig,
    /// 控制面事件广播(operator 会话的 final 回复上事件线)
    pub events: Option<crate::auto_reply::reply_dispatcher::EventSink>,
    /// agents.defaults.thinkingLevel 默认覆盖(None = 不动)
    pub default_thinking_level: Option<latent_ai::ThinkingLevel>,
    /// messages.queue 全局队列设置(新会话的 PendingQueue 初始值;
    /// /queue 在会话内覆盖)
    pub queue_settings: crate::auto_reply::queue::QueueSettings,
}

impl SessionFactory {
    /// 构建一个聊天会话(含事件泵订阅与 sendChain)。
    pub async fn build(&self, session_key: &str) -> Result<Arc<ChatSession>, String> {
        // state.json 既有文件 → resume,否则新建(重启后会话恢复)
        let session_store = match self.state.session_file(session_key) {
            Some(file) if file.exists() => SessionStore::Resume { file },
            _ => SessionStore::New {
                dir: self.sessions_dir.clone(),
            },
        };
        let built = build_session(BuildOptions {
            provider: self.provider.clone(),
            model: self.model.clone(),
            ui: Arc::new(NoopUi),
            extension_specs: self.extension_specs.clone(),
            spawn_hook: None,
            session_store,
            context_snapshot: Some(self.settings.context_snapshot),
            active_tools: self.settings.active_tools.clone(),
            search_ignore: self.settings.search_ignore.clone(),
            tool_result_max_chars: self.settings.tool_result_max_chars,
            block_images: self.settings.block_images,
            compaction: self.settings.compaction.clone(),
            session_mode: None,
            default_session_mode: self.settings.session_mode,
            sandbox: self.settings.sandbox.clone(),
            approval: self.settings.approval.clone(),
            subagent_async_approval: self.settings.subagent_async_approval,
            approval_ui: Some(self.approval_ui.clone()),
        })
        .await?;

        // session key → 文件路径持久化(重启 resume 用)
        if let Some(manager) = &built.session_manager {
            if let Some(path) = manager.file_path() {
                self.state
                    .set_session_file(session_key, path, now_ms());
                if let Err(error) = self.state.save() {
                    eprintln!("[latent-gateway] state.json 落盘失败: {error}");
                }
            }
        }

        let dispatcher = ReplyDispatcher::spawn(
            Duration::from_millis(100),
            self.events.clone().map(|mut sink| {
                sink.session_key = session_key.to_string();
                sink
            }),
        );
        let typing: Arc<AsyncMutex<Option<TypingGuard>>> = Arc::new(AsyncMutex::new(None));
        let typing_ctx: Arc<AsyncMutex<Option<(ChannelHandle, ChatRef)>>> =
            Arc::new(AsyncMutex::new(None));
        let pump = SessionEventPump {
            dispatcher: dispatcher.clone(),
            final_text: Mutex::new(String::new()),
            last_progress: Mutex::new(None),
            typing: typing.clone(),
            typing_ctx: typing_ctx.clone(),
            typing_config: self.typing_config,
        };
        let subscriber: SessionSharedSubscriber = Arc::new(pump);
        built.session.subscribe(subscriber);
        // agents.defaults.thinkingLevel 覆盖(装配默认之后应用)
        if let Some(level) = self.default_thinking_level {
            built.session.agent().set_thinking_level(Some(level));
        }

        Ok(Arc::new(ChatSession {
            built,
            run_lock: AsyncMutex::new(()),
            dispatcher,
            typing,
            typing_ctx,
            pending: AsyncMutex::new(PendingQueue::new(self.queue_settings)),
        }))
    }
}

/// 每会话状态(registry 值)。
pub struct ChatSession {
    pub built: BuiltSession,
    /// 同会话串行;跨会话并行
    pub run_lock: tokio::sync::Mutex<()>,
    /// 出站 sendChain(进度/最终回复)
    pub dispatcher: Arc<ReplyDispatcher>,
    /// typing 指示器槽(run 任务/pump 共享)
    pub typing: Arc<AsyncMutex<Option<TypingGuard>>>,
    /// typing 启动上下文(run 任务写入:当前回复目标渠道; pump 据此在
    /// 首个可见回复活动时补开 typing)
    pub typing_ctx: Arc<AsyncMutex<Option<(ChannelHandle, ChatRef)>>>,
    /// collect 模式合并缓冲 + /queue 会话覆盖
    pub pending: AsyncMutex<PendingQueue>,
}

impl ChatSession {
    pub fn session(&self) -> &Arc<latent_core::AgentSession> {
        &self.built.session
    }
}

/// 会话注册表:key = session key。
pub struct ChatSessionRegistry {
    sessions: AsyncMutex<HashMap<String, Arc<ChatSession>>>,
    factory: Arc<SessionFactory>,
}

impl ChatSessionRegistry {
    pub fn new(factory: Arc<SessionFactory>) -> Self {
        ChatSessionRegistry {
            sessions: AsyncMutex::new(HashMap::new()),
            factory,
        }
    }

    /// 惰性创建:第一条消息到达该 session key 时才 build。
    pub async fn get_or_build(&self, key: &str) -> Result<Arc<ChatSession>, String> {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(key) {
            return Ok(session.clone());
        }
        let session = self.factory.build(key).await?;
        sessions.insert(key.to_string(), session.clone());
        Ok(session)
    }

    /// 已构建会话(不创建;/status 等只读命令用)。
    pub async fn get(&self, key: &str) -> Option<Arc<ChatSession>> {
        self.sessions.lock().await.get(key).cloned()
    }

    /// /new 后替换会话(旧 BuiltSession 弃用;subagent 由调用方 abort)。
    pub async fn replace(&self, key: &str, session: Arc<ChatSession>) {
        self.sessions.lock().await.insert(key.to_string(), session);
    }

    pub async fn keys(&self) -> Vec<String> {
        let sessions = self.sessions.lock().await;
        let mut keys: Vec<String> = sessions.keys().cloned().collect();
        keys.sort();
        keys
    }

    pub async fn len(&self) -> usize {
        self.sessions.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.sessions.lock().await.is_empty()
    }

    pub fn factory(&self) -> &Arc<SessionFactory> {
        &self.factory
    }
}

/// 事件泵:订阅 `AgentSessionEvent` → sendChain。
///
/// - 最终回复 = assistant `MessageEnd` 为权威定稿,`AgentSettled` 只发一次;
/// - 进度通知:`ToolExecutionStart` 节流(30s 一条);`AutoRetryStart` 可选;
/// - typing:首个可见回复活动(文本 delta)时按 typingMode 开启;
/// - 后台 subagent 的 supervisor 唤醒 turn 事件自然流入(常驻 gateway
///   不调 `wait_background_subagents`)。
pub struct SessionEventPump {
    dispatcher: Arc<ReplyDispatcher>,
    /// 最近一条 assistant 定稿文本(MessageEnd 覆盖)
    final_text: Mutex<String>,
    last_progress: Mutex<Option<std::time::Instant>>,
    typing: Arc<AsyncMutex<Option<TypingGuard>>>,
    typing_ctx: Arc<AsyncMutex<Option<(ChannelHandle, ChatRef)>>>,
    typing_config: TypingConfig,
}

impl SessionEventPump {
    fn try_progress_throttled(&self, text: &str) {
        let mut last = self.last_progress.lock().unwrap();
        let due = match *last {
            Some(at) => at.elapsed() >= PROGRESS_INTERVAL,
            None => true,
        };
        if due {
            *last = Some(std::time::Instant::now());
            self.dispatcher.try_progress(text.to_string());
        }
    }

    /// 首个可见回复活动补开 typing(mode=Message 且入站时未开 —— 未 @ 的
    /// 群聊;Never 恒不显示)。run 任务没开(如后台 subagent 唤醒)时跳过。
    async fn start_typing_for_activity(&self) {
        if self.typing_config.mode == latent_channel::typing::TypingMode::Never {
            return;
        }
        let mut slot = self.typing.lock().await;
        if slot.is_some() {
            return;
        }
        let ctx = self.typing_ctx.lock().await.clone();
        if let Some((handle, chat)) = ctx {
            *slot = Some(TypingGuard::start(&handle.sender(), chat, &self.typing_config));
        }
    }
}

#[async_trait]
impl SessionSubscriber for SessionEventPump {
    async fn on_session_event(&self, event: &AgentSessionEvent) {
        match event {
            AgentSessionEvent::Agent(agent_event) => match agent_event {
                // 首个可见回复活动:typing 开启(pump 侧只做去重判定;
                // 实际 guard 由 run 任务持有,这里不重复开)
                AgentEvent::MessageDelta {
                    delta: MessageDeltaPayload::Text { .. },
                } => {
                    self.start_typing_for_activity().await;
                }
                // assistant MessageEnd = 权威定稿(覆盖式)
                AgentEvent::MessageEnd { message } => {
                    if let latent_agent::AgentMessage::Assistant(assistant) = &**message {
                        let text = assistant.text_content();
                        if !text.trim().is_empty() {
                            *self.final_text.lock().unwrap() = text;
                        }
                    }
                }
                AgentEvent::ToolExecutionStart { tool_name, .. } => {
                    self.try_progress_throttled(&format!("仍在工作…(工具:{tool_name})"));
                }
                _ => {}
            },
            AgentSessionEvent::AutoRetryStart {
                attempt, reason, ..
            } => {
                self.dispatcher
                    .try_progress(format!("[重试 #{attempt}] {reason}"));
            }
            // run 结束:最终回复只发一次
            AgentSessionEvent::AgentSettled => {
                let text = std::mem::take(&mut *self.final_text.lock().unwrap());
                if !text.trim().is_empty() {
                    self.dispatcher.send_final(text).await;
                }
            }
            _ => {}
        }
    }
}

/// 回复目标便捷构造(pump/dispatcher 的装配辅助)。
pub fn make_reply_target(
    handle: ChannelHandle,
    chat: ChatRef,
    chunk_limit: usize,
) -> ReplyTarget {
    ReplyTarget {
        sink: crate::auto_reply::reply_dispatcher::ReplySink::Channel { handle, chat },
        chunk_limit,
    }
}

/// OutboundItem re-export(agent/mod 引用便利)。
pub type DispatcherItem = OutboundItem;
