//! write 工具(05 文档 §6):整文件写入(新建或完整重写)。

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use rpi_agent::{Tool, ToolCall, ToolError, ToolOutput, ToolUpdater};

pub struct WriteTool {
    cwd: std::path::PathBuf,
}

/// 工厂。
pub fn create_write_tool(cwd: &Path) -> Arc<dyn Tool> {
    Arc::new(WriteTool { cwd: cwd.to_path_buf() })
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write a file to the local filesystem. Use write only for new files or complete rewrites; \
         use edit for partial changes."
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["path", "content"],
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            }
        })
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some("write(path, content): creates a new file or completely rewrites an existing one".into())
    }

    fn prompt_guidelines(&self) -> Vec<String> {
        vec!["Use write only for new files or complete rewrites; prefer edit for partial changes.".into()]
    }

    async fn execute(
        &self,
        call: ToolCall,
        _cancel: CancellationToken,
        _updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        let obj = call.args.as_object().ok_or(ToolError::Failed {
            name: "write".into(),
            message: "arguments must be an object".into(),
        })?;
        let path = obj
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::Failed { name: "write".into(), message: "missing required argument `path`".into() })?
            .to_string();
        let content = obj
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::Failed {
                name: "write".into(),
                message: "missing required argument `content`".into(),
            })?
            .to_string();

        let resolved = if Path::new(&path).is_absolute() {
            std::path::PathBuf::from(&path)
        } else {
            self.cwd.join(&path)
        };
        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| ToolError::Failed {
                name: "write".into(),
                message: format!("cannot create directory for `{path}`: {e}"),
            })?;
        }
        tokio::fs::write(&resolved, &content).await.map_err(|e| ToolError::Failed {
            name: "write".into(),
            message: format!("cannot write `{path}`: {e}"),
        })?;
        let bytes = content.len();
        let lines = content.lines().count();
        Ok(ToolOutput {
            output: format!("Wrote `{path}` ({bytes} bytes, {lines} lines)."),
            details: json!({"bytes": bytes, "lines": lines}),
            terminate: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    

    struct Noop;
    #[async_trait]
    impl ToolUpdater for Noop {
        async fn update(&self, _partial: String) {}
    }

    #[tokio::test]
    async fn writes_new_file_and_creates_parents() {
        let dir = std::env::temp_dir().join(format!("rpi-write-{}", uuid::Uuid::now_v7()));
        let tool = WriteTool { cwd: dir.clone() };
        let output = tool
            .execute(
                ToolCall {
                    id: "t".into(),
                    name: "write".into(),
                    args: serde_json::json!({"path": "nested/dir/file.txt", "content": "hello\nworld"}),
                },
                CancellationToken::new(),
                &Noop,
            )
            .await
            .unwrap();
        assert!(output.output.contains("Wrote"));
        let content = tokio::fs::read_to_string(dir.join("nested/dir/file.txt")).await.unwrap();
        assert_eq!(content, "hello\nworld");
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
