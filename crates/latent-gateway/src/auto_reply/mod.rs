//! dispatch 管线(§4.5,阶段序对齐上游 dispatch.ts):
//!
//! 1. 去重 claim(message_id,**先于 ACK** —— 上游语义 "ingress reserves
//!    before ACK",否则客户端重试造成双跑);
//! 2. 授权检查:私聊 dmPolicy(pairing/allowlist/open/disabled);群聊
//!    groupPolicy(allowlist 默认)+ requireMention(未 @ 且
//!    unmentionedInbound=room_event → 仅作上下文存储,不触发 run —— MVP
//!    直接丢弃并记日志);
//! 3. 命令拦截(未识别 /xxx 本地警告不发给模型);
//! 4. 防抖合并窗口(debounce.rs;命令/At 消息立即冲刷);
//! 5. session key → 注册表取/建会话;
//! 6. 队列处置(四模式);
//! 7. 执行 run:envelope 包装 → session.prompt() → 事件泵收集 →
//!    reply_dispatcher 投递。

pub mod commands;
pub mod envelope;
pub mod queue;
pub mod reply_dispatcher;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use latent_channel::debounce::{DebounceConfig, InboundDebouncer};
use latent_channel::mention_gating::{resolve_mention, MentionConfig, MentionDecision};
use latent_channel::plugin::ChannelHandle;
use latent_channel::types::{ChatType, InboundMessage, OutboundMessage};
use latent_core::SessionMode;
use latent_runtime::assembly::switch_new_session;

use crate::agents::{make_reply_target, ChatSession, ChatSessionRegistry, SessionFactory};
use crate::approval::ChatApprovalUi;
use crate::config::{DropPolicy, GatewayConfig, QueueMode};
use crate::pairing::{DmDecision, PairingStore};
use crate::routing::session_key::{build_session_key, DmScope, GroupScope};
use crate::state::now_ms;
use crate::state::StateStore;

use commands::ChatCommand;
use queue::{OverflowAction, QueueSettings};

/// 去重表过期时间(进程内 Map + 过期;重启后重放风险与上游一致,接受)。
const DEDUPE_TTL_MS: u64 = 10 * 60 * 1000;
/// 去重表容量上限(防泄漏)。
const DEDUPE_MAX_ENTRIES: usize = 4096;

/// 聊天网关引擎(daemon 全局一份)。
pub struct Gateway {
    pub config: GatewayConfig,
    pub channels: Arc<crate::channels::ChannelManager>,
    pub registry: Arc<ChatSessionRegistry>,
    pub approval: Arc<ChatApprovalUi>,
    pub pairing: Arc<PairingStore>,
    pub state: Arc<StateStore>,
    debouncer: Mutex<InboundDebouncer>,
    debounce_notify: tokio::sync::Notify,
    dedupe: Mutex<HashMap<String, u64>>,
    /// 副作用幂等键(chat.send;进程内 Map + 过期)
    idempotency: Mutex<HashMap<String, u64>>,
    /// 控制面事件广播(容量 256;无订阅者时发送即丢)
    pub events: tokio::sync::broadcast::Sender<crate::control::events::GatewayEvent>,
    queue_settings: QueueSettings,
    typing_config: latent_channel::typing::TypingConfig,
    dm_scope: DmScope,
    group_scope: GroupScope,
    /// /activation 按群覆盖(session key → require_mention)
    activation_overrides: Mutex<HashMap<String, bool>>,
    pub started_at: std::time::Instant,
}

impl Gateway {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: GatewayConfig,
        channels: Arc<crate::channels::ChannelManager>,
        factory: Arc<SessionFactory>,
        approval: Arc<ChatApprovalUi>,
        pairing: PairingStore,
        state: Arc<StateStore>,
        events: tokio::sync::broadcast::Sender<crate::control::events::GatewayEvent>,
    ) -> Arc<Self> {
        let queue = &config.messages.queue;
        let typing_mode =
            latent_channel::typing::TypingMode::parse(Some(&config.agents.defaults.typing_mode));
        let typing_config = latent_channel::typing::TypingConfig {
            mode: typing_mode,
            // typingIntervalSeconds × 500ms:默认 6 → 3000ms(上游 keepalive)
            interval: Duration::from_millis(config.agents.defaults.typing_interval_seconds * 500),
            ..latent_channel::typing::TypingConfig::default()
        };
        let queue_settings = QueueSettings {
            mode: QueueMode::parse(&queue.mode).unwrap_or_default(),
            debounce_ms: queue.debounce_ms,
            cap: queue.cap,
            drop_policy: match queue.drop.as_str() {
                "old" => DropPolicy::Old,
                "new" => DropPolicy::New,
                _ => DropPolicy::Summarize,
            },
        };
        Arc::new(Gateway {
            dm_scope: DmScope::parse(Some(&config.session.dm_scope)),
            group_scope: GroupScope::parse(Some(&config.session.group_scope)),
            config,
            channels,
            registry: Arc::new(ChatSessionRegistry::new(factory)),
            approval,
            pairing: Arc::new(pairing),
            state,
            debouncer: Mutex::new(InboundDebouncer::new(DebounceConfig::default())),
            debounce_notify: tokio::sync::Notify::new(),
            dedupe: Mutex::new(HashMap::new()),
            idempotency: Mutex::new(HashMap::new()),
            events,
            queue_settings,
            typing_config,
            activation_overrides: Mutex::new(HashMap::new()),
            started_at: std::time::Instant::now(),
        })
    }

    pub fn queue_settings(&self) -> QueueSettings {
        self.queue_settings
    }

    pub fn typing_config(&self) -> latent_channel::typing::TypingConfig {
        self.typing_config
    }

    fn emit(&self, event: crate::control::events::GatewayEvent) {
        let _ = self.events.send(event);
    }

    // ------------------------------------------------------------------
    // 去重
    // ------------------------------------------------------------------

    /// 去重 claim:**先于 ACK**(返回 false = 重复消息,丢弃)。
    fn claim_dedupe(&self, msg: &InboundMessage, now: u64) -> bool {
        let key = format!("{}:{}", msg.platform, msg.message_id);
        let mut dedupe = self.dedupe.lock().unwrap();
        // 过期清理 + 容量上限
        dedupe.retain(|_, at| now.saturating_sub(*at) < DEDUPE_TTL_MS);
        while dedupe.len() >= DEDUPE_MAX_ENTRIES {
            let oldest = dedupe.iter().min_by_key(|(_, at)| **at).map(|(k, _)| k.clone());
            match oldest {
                Some(key) => {
                    dedupe.remove(&key);
                }
                None => break,
            }
        }
        dedupe.insert(key, now).is_none()
    }

    // ------------------------------------------------------------------
    // dispatch 管线
    // ------------------------------------------------------------------

    /// 入站消息主入口(事件循环 per-message spawn 调用)。
    pub async fn dispatch_inbound(self: &Arc<Self>, msg: InboundMessage) {
        let now = now_ms();
        // 1. 去重 claim(先于 ACK)
        if !self.claim_dedupe(&msg, now) {
            return;
        }
        // 回复通道就绪检查
        let Some((handle, chunk_limit)) = self.reply_channel(&msg).await else {
            eprintln!(
                "[latent-gateway] 渠道 {} 未连接,消息丢弃: {}",
                msg.platform, msg.message_id
            );
            return;
        };

        // 2. 授权检查
        match msg.chat.chat_type {
            ChatType::Group => {
                if !self.group_allowed(&msg) {
                    eprintln!(
                        "[latent-gateway] 群消息被 groupPolicy 拒绝: {}:{}, group={}",
                        msg.platform,
                        msg.sender.user_id,
                        msg.chat.conversation_id
                    );
                    return;
                }
                if !self.mentioned(&msg).await {
                    // 未 @ 且 unmentionedInbound=room_event → 仅作上下文存储,
                    // 不触发 run(MVP:丢弃并记日志)
                    return;
                }
            }
            ChatType::Private => match self.pairing.decide(&msg, now) {
                DmDecision::Allow => {}
                DmDecision::PairingCode(text) => {
                    let _ = handle
                        .send(&msg.chat, OutboundMessage::text(text))
                        .await;
                    return;
                }
                DmDecision::Reject(text) => {
                    let _ = handle
                        .send(&msg.chat, OutboundMessage::text(text))
                        .await;
                    return;
                }
            },
        }
        self.emit(crate::control::events::GatewayEvent::Chat {
            platform: msg.platform.to_string(),
            chat_type: envelope::chat_type_label(msg.chat.chat_type).to_string(),
            conversation_id: msg.chat.conversation_id.clone(),
            sender: msg.sender.user_id.clone(),
            text: msg.text.clone(),
        });

        // 3. 命令拦截(未识别 /xxx → 本地警告不发给模型)
        match commands::parse(&msg.text) {
            Some(command) => {
                if let Some(reply) = self.handle_command(&msg, command).await {
                    let _ = handle
                        .send(&msg.chat, OutboundMessage::text(reply))
                        .await;
                    return;
                }
                // parse 命中但执行层不识别(理论不可达):本地警告兜底
                let _ = handle
                    .send(
                        &msg.chat,
                        OutboundMessage::text(format!(
                            "未知命令: {}(用 /help 查看命令清单)",
                            msg.text.split_whitespace().next().unwrap_or("")
                        )),
                    )
                    .await;
                return;
            }
            None if msg.text.trim_start().starts_with('/') => {
                // 形如命令但不在命令表:本地警告
                let _ = handle
                    .send(
                        &msg.chat,
                        OutboundMessage::text(format!(
                            "未知命令: {}(用 /help 查看命令清单)",
                            msg.text.split_whitespace().next().unwrap_or("")
                        )),
                    )
                    .await;
                return;
            }
            None => {}
        }

        // 4. 防抖合并窗口
        let window = self.debounce_window_for(msg.platform);
        let immediate = {
            let mut debouncer = self.debouncer.lock().unwrap();
            let out = debouncer.enqueue(msg, now, window);
            if debouncer.next_deadline().is_some() {
                self.debounce_notify.notify_one();
            }
            out
        };
        for message in immediate {
            Box::pin(self.run_message(message, &handle, chunk_limit)).await;
        }
    }

    /// 防冲刷循环(daemon 启动时 spawn):到期冲刷合并消息并重新入管线
    /// (跳过命令解析 —— 合并产物是纯文本)。
    pub async fn run_debounce_loop(self: Arc<Self>) {
        loop {
            let deadline = self.debouncer.lock().unwrap().next_deadline();
            match deadline {
                None => self.debounce_notify.notified().await,
                Some(at) => {
                    let now = now_ms();
                    if at <= now {
                        let flushed: Vec<InboundMessage> =
                            self.debouncer.lock().unwrap().flush_due(now_ms());
                        for message in flushed {
                            let Some((handle, chunk_limit)) = self.reply_channel(&message).await
                            else {
                                continue;
                            };
                            self.run_message(message, &handle, chunk_limit).await;
                        }
                    } else {
                        let _ = tokio::time::timeout(
                            Duration::from_millis(at - now),
                            self.debounce_notify.notified(),
                        )
                        .await;
                    }
                }
            }
        }
    }

    /// 防抖窗口:per-channel 覆盖 > 全局。
    fn debounce_window_for(&self, platform: &str) -> u64 {
        self.config
            .messages
            .queue
            .debounce_ms_by_channel
            .get(platform)
            .copied()
            .unwrap_or(self.config.messages.queue.debounce_ms)
    }

    /// 单条就绪消息 → session key → 注册表 → run。
    async fn run_message(self: &Arc<Self>, msg: InboundMessage, handle: &ChannelHandle, chunk_limit: usize) {
        // 5. session key
        let key = build_session_key(
            "main",
            &msg.chat,
            "",
            self.dm_scope,
            self.group_scope,
        );
        // 6/7. 队列处置 + 执行 run
        self.run_turn(&key, msg, handle.clone(), chunk_limit).await;
    }

    /// 队列处置(§4.6):空闲直接 run;忙则按四模式。
    async fn run_turn(
        self: &Arc<Self>,
        session_key: &str,
        msg: InboundMessage,
        handle: ChannelHandle,
        chunk_limit: usize,
    ) {
        let session = match self.registry.get_or_build(session_key).await {
            Ok(session) => session,
            Err(error) => {
                eprintln!("[latent-gateway] 会话构建失败({session_key}): {error}");
                let _ = handle
                    .send(
                        &msg.chat,
                        OutboundMessage::text(format!("会话启动失败: {error}")),
                    )
                    .await;
                return;
            }
        };
        let text = envelope::format_inbound_envelope(&msg, false);
        let lock_result = session.run_lock.try_lock();
        match lock_result {
            Ok(_guard) => {
                self.execute_turn(&session, &msg, text, handle, chunk_limit)
                    .await;
            }
            Err(_) => {
                self.queue_disposition(&session, &msg, text, &handle).await;
            }
        }
    }

    /// 执行 run(run_lock 已持有):reply target → typing → prompt → 收尾。
    async fn execute_turn(
        self: &Arc<Self>,
        session: &Arc<ChatSession>,
        msg: &InboundMessage,
        text: String,
        handle: ChannelHandle,
        chunk_limit: usize,
    ) {
        session
            .dispatcher
            .set_target(make_reply_target(handle.clone(), msg.chat.clone(), chunk_limit))
            .await;
        // typing:入站接受即发(DM 与被 @ 的群聊)
        {
            let mut slot = session.typing.lock().await;
            if let Some(previous) = slot.take() {
                previous.stop().await;
            }
            if self
                .typing_config
                .mode
                .should_show(msg.chat.chat_type, msg.to_me, latent_channel::typing::TypingTrigger::InboundAccepted)
            {
                *slot = Some(latent_channel::typing::TypingGuard::start(
                    &handle.sender(),
                    msg.chat.clone(),
                    &self.typing_config,
                ));
            }
        }
        // typing 上下文供 pump 补开(未 @ 群聊首个回复活动)
        *session.typing_ctx.lock().await = Some((handle.clone(), msg.chat.clone()));

        let _ = session.built.session.prompt(text).await;
        session.built.session.wait_idle().await;

        // collect 缓存冲刷:合并文本作为新 run(仍在 run_lock 内)
        loop {
            let merged = session.pending.lock().await.drain_merged();
            let Some(merged) = merged else { break };
            let notice = session.pending.lock().await.summarize_notice();
            let text = match notice {
                Some(notice) => format!("{notice}\n\n{merged}"),
                None => merged,
            };
            let _ = session.built.session.prompt(text).await;
            session.built.session.wait_idle().await;
        }
        session.pending.lock().await.reset_drop_counter();

        // typing 停止 + 上下文清理
        if let Some(guard) = session.typing.lock().await.take() {
            guard.stop().await;
        }
        session.typing_ctx.lock().await.take();
    }

    /// 忙时处置:steer/followup/collect/interrupt(§4.6)。
    async fn queue_disposition(
        self: &Arc<Self>,
        session: &Arc<ChatSession>,
        msg: &InboundMessage,
        text: String,
        handle: &ChannelHandle,
    ) {
        let settings = session.pending.lock().await.settings();
        match settings.mode {
            QueueMode::Steer => {
                let (steering, follow_up) = session.built.session.queue_depths();
                if steering + follow_up >= settings.cap {
                    self.handle_queue_overflow(session, msg, handle, &text).await;
                } else {
                    session.built.session.steer(text).await;
                }
            }
            QueueMode::Followup => {
                let (steering, follow_up) = session.built.session.queue_depths();
                if steering + follow_up >= settings.cap {
                    self.handle_queue_overflow(session, msg, handle, &text).await;
                } else {
                    session.built.session.follow_up(text).await;
                }
            }
            QueueMode::Collect => {
                // 网关自管缓冲:cap 溢出可真丢最旧(偏离 3 近似)
                let mut pending = session.pending.lock().await;
                match pending.push(text, now_ms()) {
                    OverflowAction::Enqueued => {}
                    OverflowAction::Rejected(receipt) => {
                        drop(pending);
                        let _ = handle
                            .send(&msg.chat, OutboundMessage::text(receipt))
                            .await;
                    }
                }
            }
            QueueMode::Interrupt => {
                // abort 后重新 prompt(spawn:等当前 run 结算)
                let gateway = self.clone();
                let session = session.clone();
                let msg = msg.clone();
                let handle = handle.clone();
                tokio::spawn(async move {
                    session.built.session.abort();
                    session.built.session.wait_idle().await;
                    let chunk_limit = gateway.chunk_limit_for(msg.platform).await;
                    let lock_result = session.run_lock.try_lock();
                    match lock_result {
                        Ok(_guard) => {
                            gateway
                                .execute_turn(&session, &msg, text, handle, chunk_limit)
                                .await;
                        }
                        // 又被占:退化为 steer
                        Err(_) => session.built.session.steer(text).await,
                    }
                });
            }
        }
    }

    /// steer/followup 的 cap 溢出:latent 队列是会话内部态,无法回退删除
    /// —— 近似处置(偏离 3 记录):new → 拒新并回执;old → 丢新;
    /// summarize → 丢新 + 合成提示回执。
    async fn handle_queue_overflow(
        self: &Arc<Self>,
        session: &Arc<ChatSession>,
        msg: &InboundMessage,
        handle: &ChannelHandle,
        text: &str,
    ) {
        let _ = session;
        let settings = self.queue_settings;
        match settings.drop_policy {
            DropPolicy::New => {
                let _ = handle
                    .send(
                        &msg.chat,
                        OutboundMessage::text("队列已满(cap),本条被丢弃(drop=new)".to_string()),
                    )
                    .await;
            }
            DropPolicy::Old => {
                eprintln!(
                    "[latent-gateway] 队列已满(drop=old),丢弃新消息(会话内部队列不可回退删除)"
                );
            }
            DropPolicy::Summarize => {
                let _ = handle
                    .send(
                        &msg.chat,
                        OutboundMessage::text(format!(
                            "队列已满(cap {}),本条被丢弃",
                            settings.cap
                        )),
                    )
                    .await;
            }
        }
        let _ = text;
    }

    // ------------------------------------------------------------------
    // 授权辅助
    // ------------------------------------------------------------------

    /// 群策略:allowlist(默认 fail-closed)/ open / disabled。
    fn group_allowed(&self, msg: &InboundMessage) -> bool {
        let group_chat = &self.config.messages.group_chat;
        let key = format!("{}:{}", msg.platform, msg.chat.conversation_id);
        match group_chat.group_policy.as_str() {
            "open" => true,
            "disabled" => false,
            // allowlist:命中才放行
            _ => group_chat.group_allow_from.iter().any(|entry| entry == &key),
        }
    }

    /// @判定(requireMention + activation 覆盖 + mentionPatterns)。
    async fn mentioned(&self, msg: &InboundMessage) -> bool {
        let group_chat = &self.config.messages.group_chat;
        let key = build_session_key("main", &msg.chat, "", self.dm_scope, self.group_scope);
        let require_mention = *self
            .activation_overrides
            .lock()
            .unwrap()
            .get(&key)
            .unwrap_or(&group_chat.require_mention);
        let cfg = MentionConfig {
            require_mention,
            mention_patterns: group_chat.mention_patterns.clone(),
            self_ids: self.channels.account_ids(msg.platform).await,
            identity_name: None,
        };
        resolve_mention(msg, &cfg) == MentionDecision::ToMe
    }

    /// 渠道回复通道(连接中的 handle + chunk 上限)。
    async fn reply_channel(&self, msg: &InboundMessage) -> Option<(ChannelHandle, usize)> {
        let handle = self.channels.handle(msg.platform).await?;
        let chunk_limit = self.chunk_limit_for(msg.platform).await;
        Some((handle, chunk_limit))
    }

    async fn chunk_limit_for(&self, platform: &str) -> usize {
        self.channels.chunk_limit(platform).await
    }

    // ------------------------------------------------------------------
    // 命令执行(§4.7)
    // ------------------------------------------------------------------

    /// 命令拦截:返回回执文本(Some)或 None(未识别)。
    async fn handle_command(
        self: &Arc<Self>,
        msg: &InboundMessage,
        command: ChatCommand,
    ) -> Option<String> {
        let owner =
            commands::is_owner(msg.platform, &msg.sender.user_id, &self.config.commands.owner_allow_from);
        let is_group = msg.chat.chat_type == ChatType::Group;
        // 权限天花板(§5.1):full-access 切换恒 owner;群聊中会话级命令
        // 一律 owner(§8 偏离 11,群会话全群共享)
        let full_access_requested =
            matches!(&command, ChatCommand::Mode { arg: Some(arg) } if arg.trim() == "full-access");
        if ((is_group && command.owner_only_in_group()) || full_access_requested) && !owner {
            return Some(commands::PERMISSION_DENIED_TEXT.to_string());
        }
        let session_key =
            build_session_key("main", &msg.chat, "", self.dm_scope, self.group_scope);
        match command {
            ChatCommand::Help => Some(commands::help_text()),
            ChatCommand::Status => {
                let Some(session) = self.registry.get(&session_key).await else {
                    return Some("会话尚未创建(先发一条消息)".into());
                };
                Some(self.status_text(&session, &session_key))
            }
            ChatCommand::Approve { id, decision } => {
                if !owner {
                    return Some(commands::PERMISSION_DENIED_TEXT.to_string());
                }
                if self.approval.resolve(id, decision).await {
                    Some(format!("已应用审批 #{id}"))
                } else {
                    Some(format!("没有待审的审批 #{id}"))
                }
            }
            command if command.needs_session() => {
                let Some(session) = self.registry.get(&session_key).await else {
                    return Some("会话尚未创建(先发一条消息)".into());
                };
                self.execute_session_command(&session, &session_key, command, msg)
                    .await
            }
            ChatCommand::Activation { arg } => {
                // 仅 owner(上方权限门已拦群聊;私聊无意义但无害)
                let want_mention = match arg.as_deref() {
                    Some("mention") => Some(true),
                    Some("always") => Some(false),
                    _ => None,
                };
                match want_mention {
                    Some(require_mention) => {
                        self.activation_overrides
                            .lock()
                            .unwrap()
                            .insert(session_key, require_mention);
                        Some(format!(
                            "本会话 @ 门已切换为 {}",
                            if require_mention { "mention(需要 @)" } else { "always(无需 @)" }
                        ))
                    }
                    None => Some("用法: /activation <mention|always>".into()),
                }
            }
            _ => None,
        }
    }

    /// 需要会话的命令执行。
    async fn execute_session_command(
        self: &Arc<Self>,
        session: &Arc<ChatSession>,
        session_key: &str,
        command: ChatCommand,
        msg: &InboundMessage,
    ) -> Option<String> {
        let agent_session = session.built.session.clone();
        match command {
            ChatCommand::New | ChatCommand::Reset => {
                // 后台 subagent 全部中止(会话废弃)
                if let Some(registry) = &session.built.subagent_registry {
                    registry.abort_all();
                }
                match switch_new_session(&agent_session, &session.built.manager_holder, None).await
                {
                    Ok(path) => {
                        // 新文件持久化进 state.json(重启 resume 跟随)
                        if let Some(path) = path {
                            self.state.set_session_file(session_key, &path, now_ms());
                            if let Err(error) = self.state.save() {
                                eprintln!("[latent-gateway] state.json 落盘失败: {error}");
                            }
                        }
                        // 换新 BuiltSession:重建 ChatSession(泵/dispatcher 重新挂)
                        match self.rebuild_session(session_key).await {
                            Ok(_) => Some("已新建会话".into()),
                            Err(error) => Some(format!("新建会话失败: {error}")),
                        }
                    }
                    Err(error) => Some(format!("新建会话失败: {error}")),
                }
            }
            ChatCommand::Compact { .. } => match agent_session.compact().await {
                Ok(tokens) => Some(format!("已压缩上下文(保留约 {tokens} tokens)")),
                Err(error) => Some(format!("压缩失败: {error}")),
            },
            ChatCommand::Stop => {
                agent_session.abort();
                Some("已中止当前任务".into())
            }
            ChatCommand::Model { arg } => match arg {
                None => {
                    let snapshot = agent_session.agent().state_snapshot();
                    Some(match snapshot.model {
                        Some(model) => format!("当前模型: {}/{}", model.provider, model.id),
                        None => "当前模型: (未知)".into(),
                    })
                }
                Some(spec) => match self.resolve_model(&spec) {
                    Ok(model) => {
                        agent_session.set_model(model.clone()).await;
                        Some(format!("模型已切换: {}/{}", model.provider, model.id))
                    }
                    Err(error) => Some(format!("模型切换失败: {error}")),
                },
            },
            ChatCommand::Thinking { arg } => match arg {
                None => {
                    let snapshot = agent_session.agent().state_snapshot();
                    Some(match snapshot.thinking_level {
                        Some(level) => format!(
                            "当前思考级别: {}",
                            latent_runtime::assembly::parse_thinking_level_name(level)
                        ),
                        None => "当前思考级别: off".into(),
                    })
                }
                Some(level) => {
                    let parsed = if level == "off" {
                        Some(None)
                    } else {
                        latent_runtime::assembly::parse_thinking_level(&level).map(Some)
                    };
                    match parsed {
                        Some(value) => {
                            agent_session.agent().set_thinking_level(value);
                            Some(format!("思考级别已设置: {level}"))
                        }
                        None => Some(format!(
                            "未知思考级别: {level}(off|minimal|low|medium|high|xhigh|max)"
                        )),
                    }
                }
            },
            ChatCommand::Mode { arg } => {
                let Some(arg) = arg else {
                    return Some(format!("当前模式: {:?}", agent_session.mode()));
                };
                let Some(mode) = SessionMode::parse(&arg) else {
                    return Some("用法: /mode <plan|confirm|full-access>".into());
                };
                match agent_session.set_mode(mode).await {
                    Ok(()) => Some(format!("会话模式已切换: {arg}")),
                    Err(error) => Some(format!("模式切换失败: {error}")),
                }
            }
            ChatCommand::Queue { arg } => {
                // 仅 owner(群聊由权限门拦);私聊 owner 才可调
                if !commands::is_owner(
                    msg.platform,
                    &msg.sender.user_id,
                    &self.config.commands.owner_allow_from,
                ) {
                    return Some(commands::PERMISSION_DENIED_TEXT.to_string());
                }
                let Some(arg) = arg else {
                    let settings = session.pending.lock().await.settings();
                    return Some(format!(
                        "当前队列: {:?}(cap {})",
                        settings.mode, settings.cap
                    ));
                };
                let mut tokens = arg.split_whitespace();
                let Some(mode_token) = tokens.next() else {
                    return Some("用法: /queue <steer|followup|collect|interrupt> [cap N]".into());
                };
                let Ok(mode) = QueueMode::parse(mode_token) else {
                    return Some(format!("未知队列模式: {mode_token}"));
                };
                let mut settings = session.pending.lock().await.settings();
                if let Some(rest) = tokens.next() {
                    if rest == "cap" {
                        if let Some(value) = tokens.next().and_then(|v| v.parse::<usize>().ok()) {
                            settings.cap = value;
                        }
                    }
                }
                settings.mode = mode;
                session.pending.lock().await.set_settings(settings);
                Some(format!("队列模式已切换: {mode_token}(cap {})", settings.cap))
            }
            _ => None,
        }
    }

    /// 控制面 chat.send 注入(§4.12):**sender 身份固定 operator**
    /// (params 无 sender 字段;不匹配 ownerAllowFrom、不受 dmPolicy 放行)。
    /// sessionKey 直指目标会话;带幂等键(进程内 Map + 过期)。
    /// 会话空闲 → 新 run(回复上控制面事件线);忙 → steer。
    pub async fn inject_operator_message(
        self: &Arc<Self>,
        session_key: String,
        text: String,
        idempotency_key: Option<String>,
    ) -> Result<String, String> {
        if let Some(key) = &idempotency_key {
            let now = now_ms();
            let mut idempotency = self.idempotency.lock().unwrap();
            idempotency.retain(|_, at| now.saturating_sub(*at) < DEDUPE_TTL_MS);
            if idempotency.insert(key.clone(), now).is_some() {
                return Err("重复的幂等键".into());
            }
        }
        let session = self.registry.get_or_build(&session_key).await?;
        let lock_result = session.run_lock.try_lock();
        let outcome = match lock_result {
            Ok(_guard) => {
                session.dispatcher.set_target_control_plane().await;
                let _ = session.built.session.prompt(text).await;
                session.built.session.wait_idle().await;
                "started".to_string()
            }
            Err(_) => {
                session.built.session.steer(text).await;
                "steered".to_string()
            }
        };
        Ok(outcome)
    }

    /// /new 后重建 ChatSession(泵/dispatcher 重新挂到新 BuiltSession)。
    pub async fn rebuild_session(&self, session_key: &str) -> Result<(), String> {
        let factory = self.registry.factory().clone();
        let session = factory.build(session_key).await?;
        self.registry.replace(session_key, session).await;
        Ok(())
    }

    /// /status 文本:模型/模式/队列深度/会话文件。
    fn status_text(&self, session: &Arc<ChatSession>, session_key: &str) -> String {
        let snapshot = session.built.session.agent().state_snapshot();
        let (steering, follow_up) = session.built.session.queue_depths();
        let file = session
            .built
            .session_manager
            .as_ref()
            .and_then(|manager| manager.file_path())
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(内存)".into());
        let model = snapshot
            .model
            .map(|m| format!("{}/{}", m.provider, m.id))
            .unwrap_or_else(|| "(未知)".into());
        format!(
            "session: {session_key}\nmodel: {model}\nmessages: {}\nqueue: steering={steering} follow_up={follow_up}\nfile: {file}",
            snapshot.message_count
        )
    }

    fn resolve_model(&self, spec: &str) -> Result<latent_ai::Model, String> {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let dir = latent_runtime::bootstrap::dirs_home()
            .and_then(|home| latent_core::latent_dir(Some(&home)));
        let resolver =
            latent_core::create_model_resolver_from_config(Some(&cwd), dir.as_deref());
        resolver.resolve(spec)
    }
}
