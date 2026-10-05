//! json 模式(08 文档 §1):AgentSession 事件序列化为 JSONL stdout。
//! 事件形状见 `modes::session_event_to_json`(剥离流式 partial,
//! toolcall_start 附 id/toolName);prompt 后逐事件落一行,结束输出总结。

use std::io::Write;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use latent_agent::RunStop;
use latent_core::{
    AgentSession, AgentSessionEvent, ExtensionUi, SessionSharedSubscriber, SessionSubscriber,
};

use crate::assembly::BuiltSession;
use crate::modes::session_event_to_json;

pub type SharedWriter = Arc<Mutex<dyn Write + Send>>;

/// json 模式入口:订阅 session 事件 → JSONL stdout,同时跑 prompt。
pub async fn run_json_mode(
    built: BuiltSession,
    prompt: String,
    out: SharedWriter,
) -> Result<RunStop, String> {
    let subscriber: SessionSharedSubscriber = Arc::new(JsonEventSubscriber { out });
    built.session.subscribe(subscriber);

    let outcome = built
        .session
        .prompt(prompt)
        .await
        .map_err(|e| e.to_string())?;
    built.session.wait_idle().await;
    // 后台 subagent 的结算通知经 supervisor 唤醒新 turn、事件照常上 JSONL;
    // 全部安静后才退出,不再有后台结果随进程静默丢失
    crate::assembly::wait_background_subagents(&built).await;
    Ok(outcome.stop())
}

/// json 模式纯函数面:装配好的 session + 单条 prompt → JSONL(测试用;
/// writer 注入,不依赖 stdout)。
pub async fn run_json_mode_with_session(
    session: &Arc<AgentSession>,
    prompt: String,
    out: SharedWriter,
) -> Result<RunStop, String> {
    let subscriber: SessionSharedSubscriber = Arc::new(JsonEventSubscriber { out });
    session.subscribe(subscriber);
    let outcome = session.prompt(prompt).await.map_err(|e| e.to_string())?;
    session.wait_idle().await;
    Ok(outcome.stop())
}

struct JsonEventSubscriber {
    out: SharedWriter,
}

fn write_line(out: &SharedWriter, value: serde_json::Value) {
    let mut writer = out.lock().unwrap();
    let _ = writeln!(writer, "{value}");
    let _ = writer.flush();
}

#[async_trait]
impl SessionSubscriber for JsonEventSubscriber {
    async fn on_session_event(&self, event: &AgentSessionEvent) {
        if let Some(json) = session_event_to_json(event) {
            write_line(&self.out, json);
        }
    }
}

/// json 模式下的 headless `ExtensionUi`(pi:print/json 模式的 UI 是 no-op;
/// notify 经事件线上报为 extension_notify,交互类返回默认值)。
pub struct JsonUi {
    pub out: SharedWriter,
}

#[async_trait]
impl ExtensionUi for JsonUi {
    async fn notify(&self, message: &str) {
        write_line(
            &self.out,
            serde_json::json!({ "type": "extension_notify", "message": message }),
        );
    }
}
