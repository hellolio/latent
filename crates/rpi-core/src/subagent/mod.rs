//! 进程内 subagent 引擎(14 文档 §4):单一 `task` 工具 + 数据化 agent 定义
//! + 运行注册表 + supervisor 空闲唤醒。
//!
//! - 同步(`async:false`):execute 内嵌套 Agent 并 await 到完成,主 loop 阻塞
//!   在工具槽位;多 task 调用经 Parallel 批自动并发(上限 4,超出排队)。
//! - 异步(`async:true`):立即返回 run id,后台运行;结算后 supervisor 唤醒
//!   父会话新 turn;审批默认 Deny(fail-closed)。
//! - 递归防护 = 子工具面裁剪(单层嵌套);转录纯内存,不落主会话 JSONL。

pub mod defs;
pub mod factory;
pub mod store;
pub mod registry;
pub mod runner;
pub mod tool;

pub use defs::{discover_agent_defs, parse_agent_def, AgentDef};
pub use registry::{SubagentRegistry, MAX_ACTIVE_ASYNC, MAX_RUN_HISTORY};
pub use runner::{run_child, ChildOutcome, ChildSpec, RunGuard, RunStatus, MAX_OUTPUT_CHARS};
pub use factory::SubagentSessionFactory;
pub use store::{ChildStore, ChildStoreFactory};
pub use tool::{SubagentDeps, SubagentTool, DEFAULT_TIMEOUT_MS, MAX_SYNC_CONCURRENCY, TOOL_NAME};
