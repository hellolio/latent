//! rpc 模式(08 文档 §2):stdio 上的 JSONL RPC,编辑器集成的正式接口。
//!
//! - **stdin**:每行一个 `RpcCommand`(serde tagged `type`,camelCase 字段,
//!   与 pi `rpc-types.ts` 同构;M6 实现业务核已支持的子集,未支持命令返回
//!   明确错误);
//! - **stdout**:`RpcResponse` + `AgentSessionEvent` 事件流(与 json 模式
//!   共用同一 JSON 映射);
//! - **扩展 UI 反向通道**:扩展要弹选择框时,`RpcUi` 把 `ExtensionUiContext`
//!   调用编码为 `extension_ui_request` 发给客户端,等待 `extension_ui_response`
//!   —— 扩展代码完全无感(接缝 #5 的 rpc 实现)。
//!
//! 传输泛型化(`AsyncBufRead`/`AsyncWrite`),测试用内存缓冲注入。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, Mutex};

use rpi_agent::RunStop;
use rpi_core::{AgentSession, ExtensionUi, SessionSharedSubscriber, SessionSubscriber};

use crate::assembly::BuiltSession;
use crate::modes::session_event_to_json;

/// stdin 命令(08 文档 `RpcCommand` 联合的 M6 子集 + 反向通道应答)。
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcCommand {
    Prompt { message: String },
    Steer { message: String },
    FollowUp { message: String },
    Abort,
    GetState,
    SetModel { model: String },
    SetThinkingLevel { level: Option<String> },
    GetMessages,
    GetEntries,
    GetTree,
    GetCommands,
    Bash { command: String },
    ExtensionUiResponse { id: u64, value: Value },
}

/// stdout 应答:`{"type":"response","id":N,"ok":true,"result":…}`。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcResponse {
    #[serde(rename = "type")]
    kind: &'static str,
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RpcResponse {
    fn ok(id: u64, result: Value) -> Self {
        Self {
            kind: "response",
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    fn err(id: u64, error: impl Into<String>) -> Self {
        Self {
            kind: "response",
            id,
            ok: false,
            result: None,
            error: Some(error.into()),
        }
    }
}

/// rpc 模式的 `ExtensionUi`:UI 调用 → `extension_ui_request` 上行,
/// 等 `extension_ui_response`(按 id 路由回 oneshot)。
pub struct RpcUi {
    writer: SharedRpcWriter,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
}

pub type SharedRpcWriter = Arc<Mutex<dyn RpcWrite>>;

/// `AsyncWriteExt` 的对象安全薄封装(测试注入内存缓冲)。
#[async_trait]
pub trait RpcWrite: Send {
    async fn write_line(&mut self, line: &str);
    async fn flush(&mut self);
}

#[async_trait]
impl<W: AsyncWrite + Send + Unpin> RpcWrite for W {
    async fn write_line(&mut self, line: &str) {
        let _ = AsyncWriteExt::write_all(self, format!("{line}\n").as_bytes()).await;
    }
    async fn flush(&mut self) {
        let _ = AsyncWriteExt::flush(self).await;
    }
}

impl RpcUi {
    pub fn new(writer: SharedRpcWriter) -> Self {
        Self {
            writer,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
        }
    }

    async fn emit(&self, value: Value) {
        let mut writer = self.writer.lock().await;
        writer.write_line(&value.to_string()).await;
        writer.flush().await;
    }

    /// 发请求并等客户端应答(扩展调用方视角是一次阻塞的 UI 调用)。
    async fn request(&self, payload: Value) -> Option<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let request = json!({
            "type": "extension_ui_request",
            "id": id,
            "request": payload,
        });
        self.emit(request).await;
        match rx.await {
            Ok(value) => Some(value),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                None
            }
        }
    }

    /// 客户端应答路由(命令循环里调用)。
    pub async fn resolve(&self, id: u64, value: Value) -> bool {
        match self.pending.lock().await.remove(&id) {
            Some(tx) => {
                let _ = tx.send(value);
                true
            }
            None => false,
        }
    }

    /// 客户端断开(stdin EOF):清掉全部未决请求,让等待中的扩展 UI 调用
    /// 走 oneshot Err 分支落默认值,而不是挂死进程。
    pub async fn close_all(&self) {
        self.pending.lock().await.clear();
    }
}

#[async_trait]
impl ExtensionUi for RpcUi {
    async fn notify(&self, message: &str) {
        // notify 无需应答:fire-and-forget
        self.emit(json!({
            "type": "extension_ui_request",
            "request": { "kind": "notify", "message": message },
        }))
        .await;
    }

    async fn confirm(&self, message: &str) -> bool {
        self.request(json!({ "kind": "confirm", "message": message }))
            .await
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    async fn select(&self, message: &str, options: &[String]) -> Option<usize> {
        self.request(json!({ "kind": "select", "message": message, "options": options }))
            .await
            .and_then(|v| v.as_u64().map(|i| i as usize))
    }

    async fn input(&self, message: &str) -> Option<String> {
        self.request(json!({ "kind": "input", "message": message }))
            .await
            .and_then(|v| v.as_str().map(str::to_string))
    }
}

/// rpc 模式入口:订阅事件流 → 读 stdin 命令循环。返回时进程退出码语义
/// 交由 main 处理。
pub async fn run_rpc_mode<R: tokio::io::AsyncRead + Unpin>(
    built: BuiltSession,
    reader: R,
    writer: SharedRpcWriter,
) -> Result<(), String> {
    let BuiltSession {
        session,
        session_manager,
    } = built;

    let subscriber: SessionSharedSubscriber = Arc::new(RpcEventSubscriber {
        writer: writer.clone(),
    });
    session.subscribe(subscriber);

    let ui = Arc::new(RpcUi::new(writer.clone()));
    let mut lines = BufReader::new(reader).lines();
    let mut command_id = 0u64;
    let mut in_flight: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| format!("stdin 读取失败: {e}"))?
    {
        if line.trim().is_empty() {
            continue; // 空行不消耗命令 id
        }
        command_id += 1;
        let command: RpcCommand = match serde_json::from_str(&line) {
            Ok(command) => command,
            Err(error) => {
                write_response(
                    &writer,
                    RpcResponse::err(command_id, format!("命令解析失败: {error}")),
                )
                .await;
                continue;
            }
        };
        if let Some(task) = dispatch(
            &session,
            session_manager.as_deref(),
            &ui,
            &writer,
            command_id,
            command,
        )
        .await
        {
            in_flight.push(task);
        }
    }
    // stdin 关闭:先释放全部未决的 extension_ui 请求(编辑器可能已断连,
    // 不释放则扩展 UI 调用永不返回,wait_idle 挂死),再等在途 run 结算
    ui.close_all().await;
    for task in in_flight {
        let _ = task.await;
    }
    session.wait_idle().await;
    Ok(())
}

async fn write_response(writer: &SharedRpcWriter, response: RpcResponse) {
    let mut writer = writer.lock().await;
    writer
        .write_line(&serde_json::to_string(&response).unwrap_or_default())
        .await;
    writer.flush().await;
}

/// 命令分派(08 文档 rpc-mode.ts 的命令循环)。长命令 spawn 以保持 stdin
/// 可响应(abort 等控制命令才能在 run 期间生效),返回其 JoinHandle 供
/// 循环退出时 join。
async fn dispatch(
    session: &Arc<AgentSession>,
    session_manager: Option<&rpi_session::SessionManager>,
    ui: &Arc<RpcUi>,
    writer: &SharedRpcWriter,
    id: u64,
    command: RpcCommand,
) -> Option<tokio::task::JoinHandle<()>> {
    // prompt 是长命令:spawn 出去保持 stdin 可响应(abort 等才能在 run 期间生效)
    if let RpcCommand::Prompt { message } = command {
        let session = session.clone();
        let writer = writer.clone();
        return Some(tokio::spawn(async move {
            // T8:PromptOutcome 区分"新启动的 run"与"已入队(steering)";
            // 旧实现入队时谎报 end_turn,现如实上报 enqueued
            let response = match session.prompt(message).await {
                Ok(rpi_core::PromptOutcome::Started(stop)) => {
                    RpcResponse::ok(id, json!({ "stopReason": stop_reason(&stop) }))
                }
                Ok(rpi_core::PromptOutcome::Enqueued) => {
                    RpcResponse::ok(id, json!({ "stopReason": "enqueued" }))
                }
                Err(error) => RpcResponse::err(id, error.to_string()),
            };
            write_response(&writer, response).await;
        }));
    }
    match command {
        RpcCommand::Prompt { .. } => unreachable!("prompt 已在上面 spawn 分支处理"),
        RpcCommand::Steer { message } => {
            session.steer(message).await;
            write_response(writer, RpcResponse::ok(id, json!({ "steered": true }))).await;
        }
        RpcCommand::FollowUp { message } => {
            session.follow_up(message).await;
            write_response(writer, RpcResponse::ok(id, json!({ "followed_up": true }))).await;
        }
        RpcCommand::Abort => {
            session.abort();
            write_response(writer, RpcResponse::ok(id, json!({ "aborted": true }))).await;
        }
        RpcCommand::GetState => {
            let state = session.agent().state_snapshot();
            let (steering, follow_up) = session.queue_depths();
            write_response(
                writer,
                RpcResponse::ok(
                    id,
                    json!({
                        "model": state.model.as_ref().map(|m| m.id.clone()),
                        "thinkingLevel": state.thinking_level.map(level_name),
                        "messageCount": state.message_count,
                        "toolCount": state.tool_count,
                        "pendingToolCalls": state.pending_tool_calls,
                        "isStreaming": state.is_streaming,
                        "errorMessage": state.error_message,
                        "queue": { "steering": steering, "follow_up": follow_up },
                    }),
                ),
            )
            .await;
        }
        RpcCommand::SetModel { model } => match config_model_resolver().resolve(&model) {
            Ok(resolved) => {
                session.set_model(resolved.clone()).await;
                write_response(writer, RpcResponse::ok(id, json!({ "model": resolved.id }))).await;
            }
            Err(error) => write_response(writer, RpcResponse::err(id, error.to_string())).await,
        },
        RpcCommand::SetThinkingLevel { level } => match parse_level(level.as_deref()) {
            Ok(level) => {
                session.agent().set_thinking_level(level);
                write_response(
                    writer,
                    RpcResponse::ok(id, json!({ "thinkingLevel": level.map(level_name) })),
                )
                .await;
            }
            Err(error) => write_response(writer, RpcResponse::err(id, error)).await,
        },
        RpcCommand::GetMessages => {
            let messages = session.agent().messages();
            write_response(writer, RpcResponse::ok(id, json!({ "messages": messages }))).await;
        }
        RpcCommand::GetEntries => match session_manager {
            Some(manager) => {
                write_response(
                    writer,
                    RpcResponse::ok(id, json!({ "entries": manager.branch_entries() })),
                )
                .await;
            }
            None => write_response(writer, RpcResponse::err(id, "会话持久化未装配")).await,
        },
        RpcCommand::GetTree => match session_manager {
            Some(manager) => {
                write_response(
                    writer,
                    RpcResponse::ok(id, json!({ "tree": manager.get_tree() })),
                )
                .await;
            }
            None => write_response(writer, RpcResponse::err(id, "会话持久化未装配")).await,
        },
        RpcCommand::GetCommands => {
            // core 的 slash 命令注册表尚未落地(M6 子集);如实返回空集
            write_response(writer, RpcResponse::ok(id, json!({ "commands": [] }))).await;
        }
        RpcCommand::Bash { command } => {
            // 长命令同样 spawn:bash 阻塞在内联等待时 abort/get_state/prompt
            // 全部无响应,且无超时保护,一条 sleep 600 即可让命令循环挂死
            let writer = writer.clone();
            return Some(tokio::spawn(async move {
                let output = tokio::process::Command::new("bash")
                    .arg("-c")
                    .arg(&command)
                    .output()
                    .await;
                let response = match output {
                    Ok(output) => RpcResponse::ok(
                        id,
                        json!({
                            "exitCode": output.status.code().unwrap_or(-1),
                            "stdout": String::from_utf8_lossy(&output.stdout),
                            "stderr": String::from_utf8_lossy(&output.stderr),
                        }),
                    ),
                    Err(error) => RpcResponse::err(id, error.to_string()),
                };
                write_response(&writer, response).await;
            }));
        }
        RpcCommand::ExtensionUiResponse {
            id: request_id,
            value,
        } => {
            // 反向通道应答是单向消息:路由即可,不回 response 帧(pi 语义)
            ui.resolve(request_id, value).await;
        }
    }
    // 其余命令都是同步分派(应答已在各分支写出)
    None
}

fn stop_reason(stop: &RunStop) -> &'static str {
    match stop {
        RunStop::EndTurn => "end_turn",
        RunStop::Aborted => "aborted",
        RunStop::Error(_) => "error",
        RunStop::BudgetExhausted(_) => "budget_exhausted",
    }
}

/// SetModel 用带 models.json 配置的 resolver(与启动路径一致)。
fn config_model_resolver() -> rpi_core::ModelResolver {
    let cwd = std::env::current_dir().ok();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    rpi_core::create_model_resolver_from_config(cwd.as_deref(), home.as_deref())
}

fn level_name(level: rpi_ai::ThinkingLevel) -> &'static str {
    match level {
        rpi_ai::ThinkingLevel::Minimal => "minimal",
        rpi_ai::ThinkingLevel::Low => "low",
        rpi_ai::ThinkingLevel::Medium => "medium",
        rpi_ai::ThinkingLevel::High => "high",
        rpi_ai::ThinkingLevel::Xhigh => "xhigh",
        rpi_ai::ThinkingLevel::Max => "max",
    }
}

fn parse_level(level: Option<&str>) -> Result<Option<rpi_ai::ThinkingLevel>, String> {
    let Some(level) = level else { return Ok(None) };
    match level {
        "minimal" => Ok(Some(rpi_ai::ThinkingLevel::Minimal)),
        "low" => Ok(Some(rpi_ai::ThinkingLevel::Low)),
        "medium" => Ok(Some(rpi_ai::ThinkingLevel::Medium)),
        "high" => Ok(Some(rpi_ai::ThinkingLevel::High)),
        "xhigh" => Ok(Some(rpi_ai::ThinkingLevel::Xhigh)),
        "max" => Ok(Some(rpi_ai::ThinkingLevel::Max)),
        other => Err(format!("未知 thinking level: {other}")),
    }
}

struct RpcEventSubscriber {
    writer: SharedRpcWriter,
}

#[async_trait]
impl SessionSubscriber for RpcEventSubscriber {
    async fn on_session_event(&self, event: &rpi_core::AgentSessionEvent) {
        if let Some(json) = session_event_to_json(event) {
            let mut writer = self.writer.lock().await;
            writer.write_line(&json.to_string()).await;
            writer.flush().await;
        }
    }
}
