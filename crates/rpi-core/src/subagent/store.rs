//! 子 agent 会话落盘面(14 文档扩展):与主会话同一套 rpi-session 机制。
//! `ChildStore` 由装配层经 `ChildStoreFactory` 按 tag 现场创建 —— sink 与
//! 主会话同一 `SessionSink` 面,stream_options 携带 contextSnapshot 的
//! on_payload(快照落子会话自己的 `.ctx/` 目录)。

use std::sync::Arc;

use rpi_ai::StreamOptions;

use crate::session::SessionSink;

#[derive(Clone)]
pub struct ChildStore {
    /// JSONL 落盘(消息/usage/ContextRef entry,格式与主会话一致)
    pub sink: Arc<dyn SessionSink>,
    /// contextSnapshot 开启时携带 on_payload;未开启为默认值
    pub stream_options: StreamOptions,
}

/// 按 tag(= run id 或 agent 名)现场创建子会话存储。
pub type ChildStoreFactory = Arc<dyn Fn(&str) -> Result<ChildStore, String> + Send + Sync>;
