//! latent-agent —— 循环 + Agent + 核心类型(依赖:仅 latent-ai)。
//!
//! 对外只暴露:工厂(`create_agent`)、trait(`LoopHooks`/`Tool`/`Subscriber`)、
//! 纯类型。循环按 03 文档 §3 伪代码逐行实现(双层 while + 工具执行四阶段 +
//! length 截断防御 + 护栏 `TurnLimits`)。工具 schema 不经转录声明 —— 每次请求
//! 经 `Context.tools` 动态下发。

pub mod agent;
pub mod event;
pub mod hooks;
pub mod loop_;
pub mod message;
pub mod tool;

pub use agent::{create_agent, Agent, AgentError, AgentState, AgentStateSnapshot, QueueMode};
pub use event::{AgentEvent, MessageDeltaPayload, SharedPartial, SharedSubscriber, Subscriber};
pub use hooks::{
    LoopHooks, PassthroughHooks, RequestUpdate, ToolBlock, ToolCallCtx, ToolPatch, ToolResultCtx,
    TurnCtx, TurnDecision, TurnUpdate,
};
pub use loop_::{
    create_injection_endpoints, run_agent_loop, tool_self_output_limit, trim_tool_result_output,
    validate_arguments, AgentContext, BudgetKind, InjectionDepth, InjectionReceiver,
    InjectionSender, LoopConfig, LoopOutput, Phase, RunStop, ToolOutcome, TurnLimits, Wake,
    DEFAULT_TOOL_RESULT_MAX_CHARS, TOOL_RESULT_MARGIN_CHARS,
};
pub use message::{now_ms, AgentMessage, CustomMessage};
// latent-ai 类型经 latent-agent 再导出:下游(latent-session/latent-tools)只依赖本 crate 的类型
pub use latent_ai::{AssistantMessage, ContentBlock, StopReason, Usage};
pub use tool::{Tool, ToolCall, ToolError, ToolExecution, ToolOutput, ToolUpdater};
