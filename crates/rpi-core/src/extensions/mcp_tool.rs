//! MCP tool 桥(07 §8.4):`tools/list` 快照 → `Arc<dyn Tool>`;
//! `tools/call` 执行,`notifications/progress` → `ToolUpdater`,
//! `CancellationToken` → `notifications/cancelled`。
//!
//! 对模型暴露的名字带扩展前缀 `{extension}__{tool}`(多扩展同名工具不冲突);
//! 事件埋点(tool_call 拦截/改参)作用于**原始名**,与前缀无关。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{CallToolRequest, CallToolRequestParams, ClientRequest, JsonObject, ServerResult};
use rmcp::service::PeerRequestOptions;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use rpi_agent::{Tool, ToolCall, ToolError, ToolOutput, ToolUpdater};

use super::mcp_host::{cancel_remote_request, McpConnection};

/// 连接期 tools/list 快照的单个工具元数据。
#[derive(Debug, Clone)]
pub struct McpToolMeta {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// 进度路由表:progress token → 转发器(跨任务可达,handler 收到
/// `notifications/progress` 时按 token 查表)。
pub(crate) type ProgressRoutes =
    Arc<Mutex<HashMap<String, Arc<dyn ToolUpdater>>>>;

/// 远端 MCP 工具的宿主侧执行体。
pub struct McpTool {
    connection: Arc<McpConnection>,
    /// tools/list 里的远端名。
    remote_name: String,
    /// 模型可见名(带扩展前缀)。
    wire_name: String,
    description: String,
    schema: Value,
}

/// tools/call 的兜底挂起防护:远端既不响应也不发进度时按失败结算
/// (07 §8.4:挂起防护 = 进程边界 + 兜底超时)。
const TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(600);

impl McpTool {
    pub(crate) fn new(
        connection: Arc<McpConnection>,
        remote_name: String,
        description: String,
        schema: Value,
    ) -> Self {
        let wire_name = super::mcp_host::tool_wire_name(connection.name(), &remote_name);
        McpTool { connection, remote_name, wire_name, description, schema }
    }
}

/// 进度转发器:跨任务路由表里存它(有 'static),把进度通知转入本地 channel,
/// 由 execute 的转发任务再交给借用态的 `&dyn ToolUpdater`(不改接缝 #3 签名)。
struct ChannelUpdater {
    tx: tokio::sync::mpsc::UnboundedSender<String>,
}

#[async_trait::async_trait]
impl ToolUpdater for ChannelUpdater {
    async fn update(&self, partial: String) {
        let _ = self.tx.send(partial);
    }
}

/// 路由表清理守卫:请求结算后移除 token 路由。
struct RouteGuard {
    routes: ProgressRoutes,
    token: String,
}

impl Drop for RouteGuard {
    fn drop(&mut self) {
        self.routes.lock().unwrap().remove(&self.token);
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.wire_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(
        &self,
        call: ToolCall,
        cancel: CancellationToken,
        updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        if self.connection.is_stale() {
            return Err(ToolError::Failed {
                name: self.wire_name.clone(),
                message: format!("extension `{}` is disconnected", self.connection.name()),
            });
        }

        // 非 object 参数是模型侧协议错误,显式报错而不是无声吞参
        let Some(arguments) = call.args.as_object() else {
            return Err(ToolError::Failed {
                name: self.wire_name.clone(),
                message: "tool arguments must be a JSON object".into(),
            });
        };
        let params = CallToolRequestParams::new(self.remote_name.clone())
            .with_arguments(arguments.clone().into_iter().collect::<JsonObject>());
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));

        // 可取消请求:rmcp 自动分配 progress token,进度通知按它回路由表
        // 入队阶段同样可被挂起的扩展卡住(select 覆盖发送与等待全程)
        let mut dispatch = std::pin::pin!(
            self.connection
                .peer()
                .send_cancellable_request(request, PeerRequestOptions::no_options())
        );
        let handle = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(ToolError::Aborted { name: self.wire_name.clone() });
            }
            dispatched = &mut dispatch => dispatched.map_err(|error| {
                self.connection
                    .mark_stale(format!("tools/call dispatch failed: {error}"));
                ToolError::Failed {
                    name: self.wire_name.clone(),
                    message: format!("tools/call dispatch failed: {error}"),
                }
            })?,
            _ = tokio::time::sleep(TOOL_CALL_TIMEOUT) => {
                return Err(ToolError::Failed {
                    name: self.wire_name.clone(),
                    message: "tools/call timed out".into(),
                });
            }
        };
        let token_key = progress_token_key(&handle.progress_token);
        let request_id = handle.id.clone();

        let routes = self.connection.progress_routes().clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        routes.lock().unwrap().insert(token_key.clone(), Arc::new(ChannelUpdater { tx }));
        let guard = RouteGuard { routes: routes.clone(), token: token_key.clone() };

        // 本地转发:channel → 借用态 updater(在本次 execute 的任务内)
        let forward = async {
            while let Some(text) = rx.recv().await {
                updater.update(text).await;
            }
        };

        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                drop(guard);
                cancel_remote_request(self.connection.peer(), request_id.clone(), "cancelled by rpi host").await;
                return Err(ToolError::Aborted { name: self.wire_name.clone() });
            }
            response = handle.rx => response,
            _ = tokio::time::sleep(TOOL_CALL_TIMEOUT) => {
                drop(guard);
                cancel_remote_request(self.connection.peer(), request_id.clone(), "tools/call timed out").await;
                return Err(ToolError::Failed {
                    name: self.wire_name.clone(),
                    message: "tools/call timed out".into(),
                });
            }
        };
        drop(guard);
        // 排空在途进度通知:guard 已释放,channel 关闭后 forward 立即结束;
        // 进度通知先于响应入站,响应结算时可能仍在 channel 里
        let _ = tokio::time::timeout(Duration::from_millis(500), forward).await;

        let result = result.map_err(|error| {
            // 响应通道关闭 = 连接断开:标记失效 + 诊断(与事件路径语义一致)
            self.connection
                .mark_stale(format!("tools/call response channel closed: {error}"));
            ToolError::Failed {
                name: self.wire_name.clone(),
                message: "tools/call response channel closed".into(),
            }
        })?;
        match result {
            Ok(ServerResult::CallToolResult(result)) => convert_tool_result(self, result),
            Ok(other) => Err(ToolError::Failed {
                name: self.wire_name.clone(),
                message: format!("unexpected tools/call response: {other:?}"),
            }),
            Err(error) => Err(ToolError::Failed {
                name: self.wire_name.clone(),
                message: error.to_string(),
            }),
        }
    }
}

/// CallToolResult → ToolOutput:文本拼接为 output,structured content 为 details;
/// is_error 走 Err(错误表达约定:execute 返回 Err,不在 content 里编码错误)。
fn convert_tool_result(
    tool: &McpTool,
    result: rmcp::model::CallToolResult,
) -> Result<ToolOutput, ToolError> {
    let mut text = String::new();
    for block in &result.content {
        if let Some(item) = block.as_text() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&item.text);
        } else if !text.ends_with("[non-text content omitted]") {
            // 非文本块(图片等)显式标注,不无声丢弃
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str("[non-text content omitted]");
        }
    }
    if result.is_error.unwrap_or(false) {
        return Err(ToolError::Failed {
            name: tool.wire_name.clone(),
            message: if text.is_empty() { "tool reported error".into() } else { text },
        });
    }
    Ok(ToolOutput {
        output: text,
        details: result.structured_content.clone().unwrap_or(Value::Null),
        terminate: false,
    })
}

fn progress_token_key(token: &rmcp::model::ProgressToken) -> String {
    match &token.0 {
        rmcp::model::NumberOrString::Number(number) => number.to_string(),
        rmcp::model::NumberOrString::String(text) => text.to_string(),
    }
}
