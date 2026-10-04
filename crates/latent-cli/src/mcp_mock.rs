//! mock MCP 扩展(07 §8.7 步骤 5 端到端验收):`latent --mcp-mock-server` 以
//! MCP stdio server 形态运行,E2E 用它走通"注册工具 + 拦截危险 bash +
//! elicitation 确认"全链路。
//!
//! 能力声明:订阅 `tool_call`(fail-closed,含 `dangerous` 的 bash 命令拦截);
//! 注册工具 `echo`(执行时经 elicitation 请求宿主确认)。

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, CustomRequest,
    ElicitRequestParams, ElicitResult, ElicitationAction, ElicitationSchema, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt};
use serde_json::{json, Value};

use rmcp::model::CustomResult;
use latent_core::extensions::{EventPolicy, ExtensionRegistration};

/// mock 扩展名(E2E 里作为连接名,工具桥接为 `e2e__echo`)。
pub const MOCK_EXTENSION_NAME: &str = "e2e";

/// 能力声明:订阅 tool_call(fail-closed)。
pub fn mock_registration() -> ExtensionRegistration {
    let mut events = std::collections::HashMap::new();
    events.insert(
        "tool_call".to_string(),
        EventPolicy {
            timeout_ms: 3_000,
            fail_closed: true,
        },
    );
    ExtensionRegistration {
        events,
        high_frequency: Vec::new(),
    }
}

/// mock 扩展的 ServerHandler:行为固定,专供端到端验收。
#[derive(Default)]
pub struct MockExtension;

fn tool_call_payload(payload: &Value) -> Option<(&str, &str, &Value)> {
    let object = payload.as_object()?;
    let name = object.get("name")?.as_str()?;
    let args = object.get("args")?;
    let id = object
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or("");
    Some((id, name, args))
}

impl ServerHandler for MockExtension {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(MOCK_EXTENSION_NAME, "0.1.0"))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let schema: JsonObject = serde_json::from_value(json!({
            "type": "object",
            "required": ["text"],
            "properties": {"text": {"type": "string"}}
        }))
        .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        Ok(ListToolsResult {
            tools: vec![Tool::new(
                "echo",
                "原样回显 text 参数(执行前向宿主请求确认)",
                schema,
            )],
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if request.name.as_ref() != "echo" {
            return Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                ContentBlock::text(format!("unknown tool: {}", request.name)),
            ])));
        }
        let text = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        // elicitation:请求宿主确认(NoopUi 默认 confirm=true → accept;
        // 真实 UI 随 M5 接入)
        let elicitation = ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: format!("确认执行 echo({text})?"),
            requested_schema: ElicitationSchema::builder()
                .required_bool("value")
                .build()
                .expect("mock schema is valid"),
        };
        let ElicitResult {
            action, content, ..
        } = context
            .peer
            .create_elicitation(elicitation)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        if action != ElicitationAction::Accept {
            return Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                ContentBlock::text(format!("echo declined by user ({action:?})")),
            ])));
        }
        let confirmed = content
            .as_ref()
            .and_then(|content| content.get("value"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(CallToolResponse::Complete(CallToolResult::success(vec![
            ContentBlock::text(format!("echo: {text} (confirmed={confirmed})")),
        ])))
    }

    async fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, McpError> {
        match request.method.as_str() {
            // 能力注册:返回订阅事件表(07 §8.3)
            "latent/register" => {
                let registration = serde_json::to_value(mock_registration())
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                Ok(CustomResult::new(json!({ "latentResult": registration })))
            }
            // 决策埋点:拦截含 dangerous 的 bash 命令(fail-closed 扩展)
            "latent/event" => {
                let payload = request
                    .params
                    .unwrap_or(Value::Null)
                    .get("payload")
                    .cloned()
                    .unwrap_or(Value::Null);
                let Some((_, name, args)) = tool_call_payload(&payload) else {
                    return Ok(CustomResult::new(json!({})));
                };
                if name == "bash" {
                    let command = args.get("command").and_then(Value::as_str).unwrap_or("");
                    if command.contains("dangerous") {
                        return Ok(CustomResult::new(json!({
                            "latentResult": {
                                "block": true,
                                "reason": "blocked by e2e extension: dangerous command",
                            }
                        })));
                    }
                }
                Ok(CustomResult::new(json!({ "latentResult": {} })))
            }
            other => Err(McpError::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                other.to_string(),
                None,
            )),
        }
    }
}

/// `latent --mcp-mock-server`:以 MCP stdio server 形态运行 mock 扩展。
pub async fn run_mock_server() -> Result<(), String> {
    use tokio::io::{stdin, stdout};
    let service = MockExtension
        .serve((stdin(), stdout()))
        .await
        .map_err(|error| error.to_string())?;
    service.waiting().await.map_err(|error| error.to_string())?;
    Ok(())
}
