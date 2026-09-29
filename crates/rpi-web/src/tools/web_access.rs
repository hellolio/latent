//! web_access 工具(tool-activation.ts:53-58 的移植):
//! 懒加载激活器 —— 把四个 web 工具加入激活集(经注入的 ToolSetActivator),
//! 下一次模型请求生效。本身无搜索/抓取行为。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use rpi_agent::{Tool, ToolCall, ToolError, ToolOutput, ToolUpdater};

use crate::prompts;
use crate::tools::{names, WebContext};

pub struct WebEnableTool {
    context: Arc<WebContext>,
}

impl WebEnableTool {
    pub fn new(context: Arc<WebContext>) -> Self {
        WebEnableTool { context }
    }
}

#[async_trait]
impl Tool for WebEnableTool {
    fn name(&self) -> &str {
        names::WEB_ACCESS
    }

    fn description(&self) -> &str {
        prompts::WEB_ACCESS_DESCRIPTION
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(prompts::web_access_prompt_snippet())
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        call: ToolCall,
        _cancel: CancellationToken,
        _updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        let activator = self.context.activator.as_ref().ok_or(ToolError::Failed {
            name: call.name.clone(),
            message: "Tool activation is not available in this session".to_string(),
        })?;
        activator
            .activate(&names::activatable_strings())
            .await
            .map_err(|message| ToolError::Failed {
                name: call.name.clone(),
                message,
            })?;
        // 工具集在 run 起点快照(agent.rs AgentContext):本 run 的后续请求
        // 不可见,经 follow_up 唤醒新 run(subagent supervisor 同款机制)后生效
        if let Some(notifier) = &self.context.notifier {
            notifier
                .notify(format!(
                    "web_access completed: {} are now active. Continue with the user's original request using these tools.",
                    names::ACTIVATABLE.join(", ")
                ))
                .await;
        }
        Ok(ToolOutput {
            output: format!(
                "Web access enabled: {}. The tools take effect in the next turn — end this turn; a follow-up turn is scheduled automatically where you can call them.",
                names::ACTIVATABLE.join(", ")
            ),
            details: json!({
                "enabled": names::ACTIVATABLE,
            }),
            terminate: false,
        })
    }
}
