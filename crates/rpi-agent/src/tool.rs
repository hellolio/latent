//! 接缝 #3(循环 ↔ 工具):`Tool` trait(09 B2 映射)。
//!
//! 注册表存 `Arc<dyn Tool>`;`details` 边界统一 `serde_json::Value`,
//! 各工具内部用强类型中间表示(09 B5.4)。错误表达约定:**execute 返回 Err,
//! 不在 content 里编码错误**(01 文档 §3.1)。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolOutput {
    pub output: String,
    pub details: serde_json::Value,
    /// 工具声明的"任务完成信号":批内全部结果 terminate 时提前结束 agent
    /// (03 文档 §5.5);默认 false。
    #[serde(default)]
    pub terminate: bool,
}

impl ToolOutput {
    pub fn text(output: impl Into<String>) -> Self {
        ToolOutput {
            output: output.into(),
            details: serde_json::Value::Null,
            terminate: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("tool `{name}` failed: {message}")]
    Failed { name: String, message: String },
    #[error("tool `{name}` aborted")]
    Aborted { name: String },
}

/// 工具执行期进度上报(read 的行号流、bash 的输出落盘等,05 文档)。
#[async_trait]
pub trait ToolUpdater: Send + Sync {
    async fn update(&self, partial: String);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolExecution {
    Serial,
    /// 并行的双语义由循环保证(09 B2):事件按完成序,tool result 消息按源序
    Parallel,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    fn description(&self) -> &str {
        ""
    }

    /// JSON Schema(参数校验由循环执行;`strict:"prefer"` 即 schema 进工具定义)
    fn schema(&self) -> serde_json::Value;

    /// 逐工具覆盖批执行模式(03 文档 §5.1):None = 跟随 hooks 默认
    fn execution_mode(&self) -> Option<ToolExecution> {
        None
    }

    /// 一行片段,进系统提示词 tools 节(04 文档 §3.1 promptSnippet)
    fn prompt_snippet(&self) -> Option<String> {
        None
    }

    /// 条目,进系统提示词 rules 节(04 文档 §3.1 promptGuidelines)
    fn prompt_guidelines(&self) -> Vec<String> {
        Vec::new()
    }

    async fn execute(
        &self,
        call: ToolCall,
        cancel: CancellationToken,
        updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError>;
}
