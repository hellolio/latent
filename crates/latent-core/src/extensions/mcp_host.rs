//! 07 §8.4 MCP 协议面:每个扩展 = 一个长驻 MCP server,latent 启动 spawn、退出回收。
//!
//! 通讯载体 rmcp(pin `=3.4.1`);协议映射:
//! - 能力注册:自定义 `latent/register`(宿主 → 扩展请求,响应即能力声明);
//! - 事件埋点:自定义 `latent/event`(request/notification 同名两用,
//!   决策类/观察类,params = `{event, payload}`);
//! - 工具注册与执行:`tools/list`、`tools/call`(桥成 `McpTool`);
//! - 工具进度:`notifications/progress` → `ToolUpdater`;
//! - 工具取消:`notifications/cancelled` ↔ `CancellationToken`;
//! - UI(select/input/confirm):`elicitation/create` → `ExtensionUi`;
//! - notify:`notifications/message`(logging)→ `ExtensionUi::notify`。
//!
//! 进程生命周期:崩溃/断连 → 连接失效(stale)→ 诊断 + 该扩展被跳过;
//! 等待中的决策请求随分发失败处理(fail-open/closed,07 §8.5)。

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{
    ClientNotification, ClientRequest, CustomNotification, CustomRequest, ElicitRequestParams,
    ElicitResult, ElicitationAction, PrimitiveSchemaDefinition, ProgressNotificationParam,
};
// logging 通知被 SEP-2577 弃用,但仍作为扩展 notify 的标准通道(on_logging_message)
#[allow(deprecated)]
use rmcp::model::LoggingMessageNotificationParam;
use rmcp::service::{RoleClient, RunningService, ServiceError, ServiceExt};
use rmcp::{transport::IntoTransport, ClientHandler};
use serde_json::{json, Value};

use super::event_bus::{record_diagnostic, DiagnosticsSink, ExtensionEvent, ExtensionRegistration};
use super::mcp_tool::{McpToolMeta, ProgressRoutes};
use super::ExtensionUi;

/// 自定义方法名(07 §8.4 协议映射)。
pub const REGISTER_METHOD: &str = "latent/register";
pub const EVENT_METHOD: &str = "latent/event";

/// 自定义方法响应信封:扩展以 `{"latentResult": <事件特定载荷>}` 应答。
/// 必须有信封 —— rmcp 的 ServerResult 是 untagged union,裸载荷形如
/// `{"isError":true}` 会被贪婪解析成 `CallToolResult` 而丢失。
pub const RESULT_ENVELOPE: &str = "latentResult";

pub(crate) fn unwrap_latent_result(result: Value) -> Value {
    match result.get(RESULT_ENVELOPE) {
        Some(payload) => payload.clone(),
        // 兼容无信封响应(逐字段都无冲突时仍可取)
        None => result,
    }
}

/// latent/register 握手超时;超时视为 init 失败(跳过 + 诊断,07 §8.5)。
const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);

/// cli settings 里的一个扩展进程声明(`mcpServers: [{command, args, env}]`)。
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerSpec {
    /// 扩展名;缺省取 command 文件名。用作诊断标识与工具名前缀。
    pub name: Option<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// 宿主侧 MCP client handler:server → client 的反向调用落点。
struct HostClientHandler {
    ui: Arc<dyn ExtensionUi>,
    progress_routes: ProgressRoutes,
}

/// elicitation → ExtensionUi 的桥(latent 约定:requestedSchema 单属性 "value",
/// boolean = confirm、enum = select、string = input;07 §8.4)。
/// 独立函数以便 no-op / 真实现两路测试。
pub async fn bridge_elicitation(
    ui: &dyn ExtensionUi,
    request: ElicitRequestParams,
) -> Result<ElicitResult, McpError> {
    let ElicitRequestParams::FormElicitationParams {
        message,
        requested_schema,
        meta: _,
    } = request
    else {
        // URL elicitation 是浏览器外跳流程,headless 宿主不支持
        return Ok(ElicitResult::new(ElicitationAction::Decline));
    };
    let Some((_, definition)) = requested_schema
        .properties
        .into_iter()
        .find(|(name, _)| name == "value")
    else {
        return Ok(ElicitResult::new(ElicitationAction::Decline));
    };
    let value = match definition {
        PrimitiveSchemaDefinition::Boolean(_) => {
            if !ui.confirm(&message).await {
                return Ok(ElicitResult::new(ElicitationAction::Decline));
            }
            json!(true)
        }
        PrimitiveSchemaDefinition::Enum(schema) => {
            let options = match schema {
                rmcp::model::EnumSchema::Single(rmcp::model::SingleSelectEnumSchema::Untitled(
                    untitled,
                )) => untitled.enum_,
                rmcp::model::EnumSchema::Single(rmcp::model::SingleSelectEnumSchema::Titled(
                    titled,
                )) => titled.one_of.into_iter().map(|item| item.const_).collect(),
                rmcp::model::EnumSchema::Legacy(legacy) => legacy.enum_,
                // 多选枚举没有对应的单选 UI 原语
                _ => return Ok(ElicitResult::new(ElicitationAction::Decline)),
            };
            let Some(index) = ui.select(&message, &options).await else {
                return Ok(ElicitResult::new(ElicitationAction::Decline));
            };
            match options.get(index) {
                Some(option) => json!(option),
                None => return Ok(ElicitResult::new(ElicitationAction::Decline)),
            }
        }
        PrimitiveSchemaDefinition::String(_) => {
            let Some(text) = ui.input(&message).await else {
                return Ok(ElicitResult::new(ElicitationAction::Decline));
            };
            json!(text)
        }
        // number/integer 等其余原语暂无对应 UI 原语
        _ => return Ok(ElicitResult::new(ElicitationAction::Decline)),
    };
    Ok(ElicitResult::new(ElicitationAction::Accept).with_content(json!({ "value": value })))
}

/// McpError = rmcp 的 ErrorData(handler 侧错误类型的别名)。
type McpError = rmcp::ErrorData;

impl ClientHandler for HostClientHandler {
    /// 声明 elicitation 能力:扩展才允许发起反向 UI 调用。
    fn get_info(&self) -> rmcp::model::ClientConfig {
        rmcp::model::ClientConfig::new(
            rmcp::model::ClientCapabilities::builder()
                .enable_elicitation()
                .build(),
            rmcp::model::Implementation::new("latent", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: rmcp::service::RequestContext<RoleClient>,
    ) -> Result<ElicitResult, McpError> {
        bridge_elicitation(&*self.ui, request).await
    }

    /// `notifications/message`(logging)→ `ExtensionUi::notify`(07 §8.4)。
    /// SEP-2577 弃用了 logging 通知,但它仍是扩展 notify 的标准通道,显式保留。
    #[allow(deprecated)]
    async fn on_logging_message(
        &self,
        notification: LoggingMessageNotificationParam,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) {
        #[allow(deprecated)]
        let message = match notification.data {
            Value::String(text) => text,
            other => other.to_string(),
        };
        self.ui.notify(&message).await;
    }

    /// `notifications/progress` → 在途工具调用的 `ToolUpdater`(07 §8.4)。
    async fn on_progress(
        &self,
        notification: ProgressNotificationParam,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) {
        let token = progress_token_key(&notification.progress_token);
        let route = self.progress_routes.lock().unwrap().get(&token).cloned();
        if let Some(updater) = route {
            let text = notification
                .message
                .unwrap_or_else(|| notification.progress.to_string());
            updater.update(text).await;
        }
    }

    /// server → client 自定义方法(sendMessage/appendEntry 等二期通道,07 §8.2
    /// 第三条):本期未开放,一律 method-not-found。
    async fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: rmcp::service::RequestContext<RoleClient>,
    ) -> Result<rmcp::model::CustomResult, McpError> {
        let CustomRequest { method, .. } = request;
        Err(McpError::new(
            rmcp::model::ErrorCode::METHOD_NOT_FOUND,
            std::borrow::Cow::Owned(method),
            None,
        ))
    }
}

fn progress_token_key(token: &rmcp::model::ProgressToken) -> String {
    match &token.0 {
        rmcp::model::NumberOrString::Number(number) => number.to_string(),
        rmcp::model::NumberOrString::String(text) => text.to_string(),
    }
}

/// 一条 MCP 扩展连接:握手 + 能力注册完成后的运行期句柄。
pub struct McpConnection {
    name: String,
    service: RunningService<RoleClient, HostClientHandler>,
    pub(crate) registration: ExtensionRegistration,
    /// tools/list 结果(连接期快照;tools/list_changed 增量随 M6)。
    pub(crate) tools: Vec<McpToolMeta>,
    stale: AtomicBool,
    progress_routes: ProgressRoutes,
    peer: rmcp::service::Peer<RoleClient>,
    diagnostics: DiagnosticsSink,
}

impl McpConnection {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn registration(&self) -> &ExtensionRegistration {
        &self.registration
    }

    /// tools/call 复用的 peer 句柄(Peer 是 Clone 的轻量通道句柄)。
    pub(crate) fn peer(&self) -> &rmcp::service::Peer<RoleClient> {
        &self.peer
    }

    pub fn is_stale(&self) -> bool {
        self.stale.load(Ordering::SeqCst)
    }

    /// 标记失效 + 记诊断(07 §8.4:连接断开 → 该扩展被跳过;宿主/测试亦可显式标记)。
    pub fn mark_stale(&self, message: impl Into<String>) {
        if !self.stale.swap(true, Ordering::SeqCst) {
            record_diagnostic(&self.diagnostics, &self.name, message.into());
        }
    }

    /// 观察类埋点:notification 发出即走。
    pub(crate) async fn send_event_notification(
        &self,
        event: ExtensionEvent,
        payload: &Value,
    ) -> Result<(), String> {
        let notification = event_notification(event, payload);
        match self.peer.send_notification(notification).await {
            Ok(()) => Ok(()),
            Err(error) => {
                // 通知发送失败 = 通道大概率已断:标记失效
                self.mark_stale(format!("transport closed: {error}"));
                Err(error.to_string())
            }
        }
    }

    /// 决策类埋点:request 按注册顺序串行 await(串行由 bus 的顺序 for-await 保证)。
    /// 超时不标记失效(连接仍健在);传输断开标记失效。
    pub(crate) async fn send_event_request(
        &self,
        event: ExtensionEvent,
        payload: &Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let request = ClientRequest::CustomRequest(CustomRequest::new(
            EVENT_METHOD,
            Some(json!({ "event": event.as_str(), "payload": payload })),
        ));
        let future = self.peer.send_request(request);
        match tokio::time::timeout(timeout, future).await {
            Ok(Ok(rmcp::model::ServerResult::CustomResult(result))) => {
                Ok(unwrap_latent_result(result.0))
            }
            Ok(Ok(other)) => Err(format!("unexpected response: {other:?}")),
            Ok(Err(error)) => {
                if !matches!(error, ServiceError::Timeout { .. }) {
                    self.mark_stale(format!("transport closed: {error}"));
                }
                Err(error.to_string())
            }
            Err(_) => Err(format!(
                "event `{}` timed out after {timeout:?}",
                event.as_str()
            )),
        }
    }

    /// 进度路由表(handler 收到通知时按 token 查表)。
    pub(crate) fn progress_routes(&self) -> &ProgressRoutes {
        &self.progress_routes
    }

    /// 桥接的 MCP 工具(名字加扩展前缀,避免多扩展冲突)。
    pub fn create_tools(self: &Arc<Self>) -> Vec<Arc<dyn latent_agent::Tool>> {
        self.tools
            .iter()
            .map(|meta| {
                Arc::new(super::McpTool::new(
                    self.clone(),
                    meta.name.clone(),
                    meta.description.clone(),
                    meta.schema.clone(),
                )) as Arc<dyn latent_agent::Tool>
            })
            .collect()
    }

    /// 进程回收(07 §8.4:退出回收)。
    pub fn terminate(&self) {
        self.service.cancellation_token().cancel();
    }
}

/// rmcp client 侧的实际装配:握手 → latent/register → tools/list。
async fn connect_after_serve(
    name: String,
    progress_routes: ProgressRoutes,
    service: RunningService<RoleClient, HostClientHandler>,
    diagnostics: DiagnosticsSink,
) -> Result<Arc<McpConnection>, String> {
    let peer = service.peer().clone();

    // 能力注册:扩展在响应里声明订阅事件表(07 §8.3)
    let request =
        ClientRequest::CustomRequest(CustomRequest::new(REGISTER_METHOD, Some(json!({}))));
    let response = tokio::time::timeout(REGISTER_TIMEOUT, peer.send_request(request))
        .await
        .map_err(|_| "extension registration timed out".to_string())?
        .map_err(|error| format!("extension registration failed: {error}"))?;
    let registration: ExtensionRegistration = match response {
        rmcp::model::ServerResult::CustomResult(result) => {
            let payload = unwrap_latent_result(result.0);
            serde_json::from_value::<ExtensionRegistration>(payload)
                .map_err(|error| format!("invalid latent/register response: {error}"))?
        }
        other => return Err(format!("unexpected latent/register response: {other:?}")),
    };

    // 工具清单(连接期快照)
    let listed = peer
        .list_all_tools()
        .await
        .map_err(|error| format!("tools/list failed: {error}"))?;
    let tools: Vec<McpToolMeta> = listed
        .into_iter()
        .map(|tool| McpToolMeta {
            name: tool.name.to_string(),
            description: tool
                .description
                .as_ref()
                .map(|description| description.to_string())
                .unwrap_or_default(),
            schema: serde_json::to_value(&*tool.input_schema).unwrap_or(Value::Null),
        })
        .collect();

    Ok(Arc::new(McpConnection {
        name,
        service,
        registration,
        tools,
        stale: AtomicBool::new(false),
        progress_routes,
        peer,
        diagnostics,
    }))
}

/// spawn 子进程扩展(cli 装配入口)。
pub async fn connect_stdio(
    spec: McpServerSpec,
    ui: Arc<dyn ExtensionUi>,
    diagnostics: DiagnosticsSink,
) -> Result<Arc<McpConnection>, String> {
    let name = spec.name.clone().unwrap_or_else(|| basename(&spec.command));
    let name = sanitize_extension_name(&name);
    let mut command = tokio::process::Command::new(&spec.command);
    command.args(&spec.args).envs(&spec.env);
    let transport = rmcp::transport::TokioChildProcess::new(command)
        .map_err(|error| format!("failed to spawn extension `{name}`: {error}"))?;
    connect_transport(name, transport, ui, diagnostics).await
}

/// 任意 rmcp client transport(in-memory transport 供测试锁行为,07 §8.7 风险条款)。
pub async fn connect_transport<T, E, A>(
    name: String,
    transport: T,
    ui: Arc<dyn ExtensionUi>,
    diagnostics: DiagnosticsSink,
) -> Result<Arc<McpConnection>, String>
where
    T: IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    // handler(收 progress 通知)与连接(发 tools/call)共享同一路由表
    let progress_routes: ProgressRoutes = Arc::new(Mutex::new(HashMap::new()));
    let handler = HostClientHandler {
        ui,
        progress_routes: progress_routes.clone(),
    };
    let service = handler
        .serve(transport)
        .await
        .map_err(|error| format!("MCP handshake with extension `{name}` failed: {error}"))?;
    connect_after_serve(name, progress_routes, service, diagnostics).await
}

/// 工具名前缀:`{extension}__{tool}`,多扩展同名工具不冲突。
pub(crate) fn tool_wire_name(extension: &str, tool: &str) -> String {
    format!("{}__{tool}", sanitize_extension_name(extension))
}

fn sanitize_extension_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "extension".into()
    } else {
        sanitized
    }
}

fn basename(command: &str) -> String {
    command
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(command)
        .to_string()
}

fn event_notification(event: ExtensionEvent, payload: &Value) -> ClientNotification {
    ClientNotification::CustomNotification(CustomNotification {
        method: EVENT_METHOD.to_string(),
        params: Some(json!({ "event": event.as_str(), "payload": payload })),
        extensions: Default::default(),
    })
}

/// 供 McpTool 取消在途 tools/call:`notifications/cancelled`(07 §8.4)。
pub(crate) async fn cancel_remote_request(
    peer: &rmcp::service::Peer<RoleClient>,
    request_id: rmcp::model::RequestId,
    reason: &str,
) {
    let notification = ClientNotification::CancelledNotification(
        rmcp::model::CancelledNotification::new(rmcp::model::CancelledNotificationParam::new(
            Some(request_id),
            Some(reason.to_string()),
        )),
    );
    let _ = peer.send_notification(notification).await;
}
