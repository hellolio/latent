//! 审批流(13 文档 §4):`ApprovalHooks` 是 `LoopHooks::before_tool_call`
//! 的装饰器实现,插在 hooks 洋葱最外层(Approval → Extension → Passthrough):
//! 审批先问(便宜、人审),批准后才轮到扩展埋点;审批拒绝时扩展不感知。
//!
//! 暂停即 await:before_tool_call 是 async hook,prepare_call await 它;
//! hook 内 await 人工应答,不需要改循环状态机。拒绝走 `ToolBlock{block:true}`
//! → 错误 tool result,拒绝原因自动进转录,模型可继续换路径。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use rpi_agent::{AgentMessage, LoopHooks, ToolBlock, ToolCallCtx};

use crate::permission::engine::PermissionEngine;
use crate::permission::types::{classify_tool, ApprovalDecision, ApprovalRequest, Verdict};
use crate::session::{SessionSharedSubscriber, AgentSessionEvent};

/// 审批 UI 接缝(接缝 #5 的姊妹面):mode 提供实现,core 的 ApprovalHooks 调用。
/// 返回 None = UI 通道关闭(rpc 客户端断连/print 模式),按 Deny 处理。
#[async_trait]
pub trait ApprovalUi: Send + Sync {
    async fn request_approval(&self, request: ApprovalRequest) -> Option<ApprovalDecision>;
}

/// headless(print/json)审批策略(settings `headlessApproval`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeadlessApproval {
    /// 遇审批请求直接拒绝(默认;零阻塞)
    #[default]
    Deny,
    /// 自动批准(CI 明知风险显式开启)
    AutoApprove,
}

/// print/json 的无交互审批 UI:按策略直接返回,不弹任何东西。
#[derive(Debug, Clone, Copy, Default)]
pub struct HeadlessApprovalUi {
    pub policy: HeadlessApproval,
}

#[async_trait]
impl ApprovalUi for HeadlessApprovalUi {
    async fn request_approval(&self, _request: ApprovalRequest) -> Option<ApprovalDecision> {
        match self.policy {
            HeadlessApproval::Deny => Some(ApprovalDecision::Deny),
            HeadlessApproval::AutoApprove => Some(ApprovalDecision::Approve),
        }
    }
}

/// 测试用固定决策审批 UI。
#[allow(dead_code)]
pub(crate) struct FixedApprovalUi(pub ApprovalDecision);

#[async_trait]
impl ApprovalUi for FixedApprovalUi {
    async fn request_approval(&self, _request: ApprovalRequest) -> Option<ApprovalDecision> {
        Some(self.0)
    }
}

pub struct ApprovalHooks {
    inner: Arc<dyn LoopHooks>,
    engine: Arc<PermissionEngine>,
    ui: Arc<dyn ApprovalUi>,
    subscribers: Arc<Mutex<Vec<SessionSharedSubscriber>>>,
}

/// 模式节 hooks(13 文档 §8.2 的迁移形态):把可切换的模式提示词作为请求级
/// `Message::Developer` 追加在**每请求消息数组末尾**——系统提示词与工具数组
/// 随模式恒定(保 KV 缓存前缀命中),模式切换只影响尾部一条小消息。
/// `mode_text` cell 由 `AgentSession::apply_mode` 运行期写(None = 不追加);
/// 子 agent 的 hooks 不包本层,避免父模式提示词误导子会话。
pub struct ModeHooks {
    inner: Arc<dyn LoopHooks>,
    mode_text: Arc<Mutex<Option<String>>>,
}

impl ModeHooks {
    pub fn new(inner: Arc<dyn LoopHooks>, mode_text: Arc<Mutex<Option<String>>>) -> Self {
        ModeHooks { inner, mode_text }
    }
}

#[async_trait]
impl LoopHooks for ModeHooks {
    fn convert_to_llm(&self, msgs: &[AgentMessage]) -> Vec<rpi_ai::Message> {
        let mut out = self.inner.convert_to_llm(msgs);
        if let Some(text) = self.mode_text.lock().unwrap().clone() {
            // 插在最后一条用户输入之前(倒数第二):模型最后看到的是用户的
            // 最新请求(保持专注);工具轮次(末尾是 toolResult)不受影响,
            // 不破坏 tool_use→tool_result 相邻约束。位置随最新用户消息移动,
            // 每轮缓存命中覆盖到上一轮交换之前 —— 已知情接受的取舍
            let pos = out
                .iter()
                .rposition(|message| matches!(message, rpi_ai::Message::User { .. }))
                .unwrap_or(out.len());
            out.insert(pos, rpi_ai::Message::developer(text));
        }
        out
    }

    async fn transform_context(&self, msgs: Vec<AgentMessage>) -> Vec<AgentMessage> {
        self.inner.transform_context(msgs).await
    }

    async fn get_api_key(&self, provider: &str) -> Option<String> {
        self.inner.get_api_key(provider).await
    }

    async fn prepare_request(
        &self,
        model: &rpi_ai::Model,
        thinking: Option<rpi_ai::ThinkingLevel>,
    ) -> Option<rpi_agent::RequestUpdate> {
        self.inner.prepare_request(model, thinking).await
    }

    async fn prepare_next_turn(
        &self,
        ctx: rpi_agent::TurnCtx,
    ) -> Option<rpi_agent::TurnUpdate> {
        self.inner.prepare_next_turn(ctx).await
    }

    async fn finish_turn(&self, ctx: rpi_agent::TurnCtx) -> Option<rpi_agent::TurnDecision> {
        self.inner.finish_turn(ctx).await
    }

    async fn before_tool_call(
        &self,
        ctx: rpi_agent::ToolCallCtx,
    ) -> Option<rpi_agent::ToolBlock> {
        self.inner.before_tool_call(ctx).await
    }

    async fn after_tool_call(
        &self,
        ctx: rpi_agent::ToolResultCtx,
    ) -> Option<rpi_agent::ToolPatch> {
        self.inner.after_tool_call(ctx).await
    }

    fn tool_execution(&self) -> rpi_agent::ToolExecution {
        self.inner.tool_execution()
    }
}

impl ApprovalHooks {
    pub fn new(
        inner: Arc<dyn LoopHooks>,
        engine: Arc<PermissionEngine>,
        ui: Arc<dyn ApprovalUi>,
        subscribers: Arc<Mutex<Vec<SessionSharedSubscriber>>>,
    ) -> Self {
        ApprovalHooks {
            inner,
            engine,
            ui,
            subscribers,
        }
    }

    async fn broadcast(&self, event: AgentSessionEvent) {
        let subscribers = self.subscribers.lock().unwrap().clone();
        for subscriber in subscribers {
            subscriber.on_session_event(&event).await;
        }
    }

    async fn inner_before(&self, ctx: ToolCallCtx) -> Option<ToolBlock> {
        self.inner.before_tool_call(ctx).await
    }
}

#[async_trait]
impl LoopHooks for ApprovalHooks {
    fn convert_to_llm(&self, msgs: &[AgentMessage]) -> Vec<rpi_ai::Message> {
        self.inner.convert_to_llm(msgs)
    }

    async fn before_tool_call(&self, ctx: ToolCallCtx) -> Option<ToolBlock> {
        let risk = classify_tool(&ctx.name);
        match self.engine.evaluate(&ctx, risk) {
            Verdict::Allow => self.inner_before(ctx).await,
            Verdict::Deny(reason) => Some(ToolBlock {
                block: true,
                reason,
                ..Default::default()
            }),
            Verdict::Ask(request) => {
                // 缓存命中已在引擎判定层短路,这里必是真实人审
                self.broadcast(AgentSessionEvent::ApprovalRequested {
                    request: request.clone(),
                })
                .await;
                let decision = self
                    .ui
                    .request_approval(request.clone())
                    .await
                    .unwrap_or(ApprovalDecision::Deny); // UI 通道关闭 = Deny
                self.broadcast(AgentSessionEvent::ApprovalResolved {
                    tool_call_id: request.tool_call_id.clone(),
                    decision,
                })
                .await;
                match decision {
                    ApprovalDecision::Approve => self.inner_before(ctx).await,
                    ApprovalDecision::ApproveForSession => {
                        self.engine.approve_for_session(&request);
                        self.inner_before(ctx).await
                    }
                    ApprovalDecision::Deny => Some(ToolBlock {
                        block: true,
                        reason: format!("用户拒绝了该操作:{}", request.detail),
                        ..Default::default()
                    }),
                    ApprovalDecision::Abort => Some(ToolBlock {
                        block: true,
                        reason: "用户中止".into(),
                        terminate: Some(true),
                        ..Default::default()
                    }),
                }
            }
        }
    }
}

// 未使用告警规避:FixedApprovalUi 供集成测试用(crate 内测试通过 pub(crate) 访问)
#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::engine::{ApprovalRules, PermissionEngine};
    use crate::permission::types::{ApprovalDecision, SandboxConfig, SessionMode};
    use crate::session::SessionSubscriber;
    use rpi_agent::PassthroughHooks;
    use std::path::PathBuf;

    struct Collecting(Mutex<Vec<AgentSessionEvent>>);

    #[async_trait]
    impl SessionSubscriber for Collecting {
        async fn on_session_event(&self, event: &AgentSessionEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    fn ctx(name: &str, args: serde_json::Value) -> ToolCallCtx {
        ToolCallCtx {
            tool_call_id: "t1".into(),
            name: name.into(),
            args,
        }
    }

    fn setup(mode: SessionMode, decision: ApprovalDecision) -> (ApprovalHooks, Arc<Collecting>) {
        let engine = Arc::new(PermissionEngine::new(
            mode,
            SandboxConfig::default(),
            ApprovalRules::default(),
            PathBuf::from("/tmp"),
            true,
        ));
        let collecting = Arc::new(Collecting(Mutex::new(Vec::new())));
        let hooks = ApprovalHooks::new(
            Arc::new(PassthroughHooks),
            engine,
            Arc::new(FixedApprovalUi(decision)),
            Arc::new(Mutex::new(vec![collecting.clone() as SessionSharedSubscriber])),
        );
        (hooks, collecting)
    }

    #[tokio::test]
    async fn allow_passes_through_without_events() {
        let (hooks, collecting) = setup(SessionMode::Confirm, ApprovalDecision::Approve);
        let block = hooks.before_tool_call(ctx("read", serde_json::json!({"path": "x"}))).await;
        assert!(block.is_none());
        assert!(collecting.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn deny_produces_block_without_ui_call() {
        let (hooks, collecting) = setup(SessionMode::Plan, ApprovalDecision::Approve);
        let block = hooks
            .before_tool_call(ctx("write", serde_json::json!({"path": "x"})))
            .await
            .expect("期望 block");
        assert!(block.block);
        assert!(block.reason.contains("Plan"));
        assert!(collecting.0.lock().unwrap().is_empty(), "Deny 不产生审批事件");
    }

    #[tokio::test]
    async fn approve_for_session_writes_cache_and_second_call_skips_ui() {
        let (hooks, collecting) = setup(SessionMode::Confirm, ApprovalDecision::ApproveForSession);
        let first = hooks
            .before_tool_call(ctx("bash", serde_json::json!({"command": "make test"})))
            .await;
        assert!(first.is_none());
        let second = hooks
            .before_tool_call(ctx("bash", serde_json::json!({"command": "make test"})))
            .await;
        assert!(second.is_none(), "缓存命中不再 Ask");
        let events = collecting.0.lock().unwrap();
        assert_eq!(events.len(), 2, "两次调用各发一对 Requested/Resolved");
    }

    #[tokio::test]
    async fn ui_channel_closed_falls_back_to_deny() {
        struct Closed;
        #[async_trait]
        impl ApprovalUi for Closed {
            async fn request_approval(&self, _request: ApprovalRequest) -> Option<ApprovalDecision> {
                None
            }
        }
        let engine = Arc::new(PermissionEngine::new(
            SessionMode::Confirm,
            SandboxConfig::default(),
            ApprovalRules::default(),
            PathBuf::from("/tmp"),
            true,
        ));
        let hooks = ApprovalHooks::new(
            Arc::new(PassthroughHooks),
            engine,
            Arc::new(Closed),
            Arc::new(Mutex::new(vec![])),
        );
        let block = hooks
            .before_tool_call(ctx("bash", serde_json::json!({"command": "make test"})))
            .await
            .expect("期望 block");
        assert!(block.block);
        assert!(block.reason.contains("用户拒绝"));
    }

    #[tokio::test]
    async fn abort_sets_terminate() {
        let (hooks, _collecting) = setup(SessionMode::Confirm, ApprovalDecision::Abort);
        let block = hooks
            .before_tool_call(ctx("bash", serde_json::json!({"command": "make test"})))
            .await
            .expect("期望 block");
        assert_eq!(block.terminate, Some(true));
        assert!(block.reason.contains("中止"));
    }
}
