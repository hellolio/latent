//! rpi-agent —— 循环 + Agent + 核心类型(依赖:仅 rpi-ai)。
//!
//! 对外只暴露:工厂(`create_agent`)、trait(`LoopHooks`/`Tool`/`Subscriber`)、
//! 纯类型。循环按 03 文档 §3 伪代码逐行实现(双层 while + 工具执行四阶段 +
//! length 截断防御 + declareToolChanges + 护栏 `TurnLimits`)。

pub mod agent;
pub mod declare;
pub mod event;
pub mod hooks;
pub mod loop_;
pub mod message;
pub mod tool;

pub use agent::{
    AgentError, AgentState, AgentStateSnapshot, QueueMode, create_agent, Agent,
};
pub use declare::{declare_tool_changes, declared_tools};
pub use event::{AgentEvent, SharedSubscriber, Subscriber};
pub use hooks::{
    LoopHooks, PassthroughHooks, RequestUpdate, ToolBlock, ToolCallCtx, ToolPatch, ToolResultCtx,
    TurnCtx, TurnDecision, TurnUpdate,
};
pub use loop_::{
    run_agent_loop, AgentContext, BudgetKind, LoopConfig, LoopOutput, Phase, RunStop, ToolOutcome,
    TurnLimits, validate_arguments,
};
pub use message::{now_ms, AgentMessage, CustomMessage};
// rpi-ai 类型经 rpi-agent 再导出:下游(rpi-session/rpi-tools)只依赖本 crate 的类型
pub use rpi_ai::{AssistantMessage, ContentBlock, StopReason, Usage};
pub use tool::{Tool, ToolCall, ToolError, ToolExecution, ToolOutput, ToolUpdater};
