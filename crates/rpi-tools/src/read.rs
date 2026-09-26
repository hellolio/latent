//! read 工具(05 文档 §3):文本读取 + offset/limit 切片 + 双限截断 + 续读提示。
//! 图片读取(05 文档的 ImageContent 路径)依赖模型多模态参数,M4 扩展时补齐。

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use rpi_agent::{
    Tool, ToolCall, ToolError, ToolOutput, ToolUpdater,
};

use crate::truncate::{truncate_head, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

pub struct ReadTool {
    cwd: std::path::PathBuf,
}

/// 工厂:cwd 解析相对路径(pi 的 resolveToCwd)。
pub fn create_read_tool(cwd: &Path) -> Arc<dyn Tool> {
    Arc::new(ReadTool { cwd: cwd.to_path_buf() })
}

fn parse_args(args: &serde_json::Value) -> Result<(String, Option<usize>, Option<usize>), String> {
    let obj = args
        .as_object()
        .ok_or("arguments must be an object")?;
    let path = obj
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("missing required argument `path`")?
        .to_string();
    let offset = match obj.get("offset") {
        Some(v) if !v.is_null() => Some(
            v.as_u64()
                .ok_or("`offset` must be a positive integer")? as usize,
        ),
        _ => None,
    };
    let limit = match obj.get("limit") {
        Some(v) if !v.is_null() => Some(
            v.as_u64()
                .ok_or("`limit` must be a positive integer")? as usize,
        ),
        _ => None,
    };
    Ok((path, offset, limit))
}

const IMAGE_EXTENSIONS: [&str; 5] = ["jpg", "jpeg", "png", "gif", "webp"];

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a text file from the local filesystem. Returns the file content, \
         truncated to 2000 lines / 50KB. Use offset/limit to page through large files."
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {"type": "string", "description": "File path (relative paths resolve against the working directory)"},
                "offset": {"type": "integer", "description": "1-based line number to start reading from"},
                "limit": {"type": "integer", "description": "Number of lines to read"}
            }
        })
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some("read(path, offset?, limit?): reads a file; large outputs are truncated with a continuation hint".into())
    }

    fn prompt_guidelines(&self) -> Vec<String> {
        vec!["When reading a large file, continue with the offset from the truncation hint instead of re-reading from the start.".into()]
    }

    async fn execute(
        &self,
        call: ToolCall,
        _cancel: CancellationToken,
        _updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        let (path, offset, limit) = parse_args(&call.args)
            .map_err(|message| ToolError::Failed { name: "read".into(), message })?;

        let resolved = if Path::new(&path).is_absolute() {
            std::path::PathBuf::from(&path)
        } else {
            self.cwd.join(&path)
        };

        let name = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        if IMAGE_EXTENSIONS.contains(&name.as_str()) {
            return Err(ToolError::Failed {
                name: "read".into(),
                message: format!(
                    "`{path}` is an image; image reading requires a vision-capable model (not supported in this build)"
                ),
            });
        }

        let bytes = tokio::fs::read(&resolved).await.map_err(|e| ToolError::Failed {
            name: "read".into(),
            message: format!("cannot read `{path}`: {e}"),
        })?;
        let content = String::from_utf8_lossy(&bytes);

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // offset 1 起始;越界报错(05 文档)
        let start = offset.unwrap_or(1);
        if start < 1 || (!lines.is_empty() && start > total_lines) || (lines.is_empty() && start > 1) {
            return Err(ToolError::Failed {
                name: "read".into(),
                message: format!(
                    "offset {start} is out of range: `{path}` has {total_lines} lines"
                ),
            });
        }
        let end = limit.map(|l| (start - 1 + l).min(total_lines)).unwrap_or(total_lines);
        let slice: String = if lines.is_empty() { String::new() } else { lines[start - 1..end].join("\n") };

        let truncation = truncate_head(&slice, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let mut output = truncation.content.clone();
        if truncation.truncation_by_bytes() {
            // 单行超字节限:offset 提示无意义,指引改用 bash(05 文档)
            let skip_bytes = output.len();
            output.push_str(&format!(
                "\n\n[Single line exceeds the byte limit; showing the first {skip_bytes} bytes. \
                 Use bash to read further: `tail -c +{next} {path} | head -c {MAX}`]",
                next = skip_bytes + 1,
                MAX = DEFAULT_MAX_BYTES
            ));
        } else if truncation.truncated {
            output.push_str(&format!(
                "\n\n[Showing lines {start}-{} of {total_lines}. Use offset={} to continue.]",
                start + truncation.output_lines - 1,
                start + truncation.output_lines
            ));
        } else if let Some(_limit) = limit {
            if end < total_lines {
                output.push_str(&format!(
                    "\n\n[{end} of {total_lines} lines shown; {remaining} more lines remain.]",
                    remaining = total_lines - end
                ));
            }
        }
        let _ = limit;
        Ok(ToolOutput {
            output,
            details: serde_json::to_value(&truncation).unwrap_or(serde_json::Value::Null),
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

    async fn exec(tool: &ReadTool, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        tool.execute(
            ToolCall { id: "t".into(), name: "read".into(), args },
            CancellationToken::new(),
            &Noop,
        )
        .await
    }

    #[tokio::test]
    async fn reads_file_with_offset_and_continuation_hint() {
        let dir = std::env::temp_dir().join(format!("rpi-read-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("sample.txt");
        let content = (1..=3000).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        tokio::fs::write(&path, &content).await.unwrap();

        let tool = ReadTool { cwd: dir.clone() };
        // 全量读:截断 + 续读提示
        let output = exec(&tool, serde_json::json!({"path": "sample.txt"})).await.unwrap();
        assert!(output.output.contains("Use offset="), "应带续读提示");
        // offset 翻页
        let output = exec(&tool, serde_json::json!({"path": "sample.txt", "offset": 2001})).await.unwrap();
        assert!(output.output.starts_with("line 2001"));
        // limit 提前结束 + 剩余行提示
        let output = exec(&tool, serde_json::json!({"path": "sample.txt", "offset": 1, "limit": 5})).await.unwrap();
        assert!(output.output.contains("5 of 3000 lines shown"));
        // offset 越界报错
        let err = exec(&tool, serde_json::json!({"path": "sample.txt", "offset": 99999})).await.unwrap_err();
        assert!(err.to_string().contains("out of range"));
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn missing_file_is_error() {
        let tool = ReadTool { cwd: std::env::temp_dir() };
        let err = exec(&tool, serde_json::json!({"path": "definitely-missing.txt"})).await.unwrap_err();
        assert!(err.to_string().contains("cannot read"));
    }
}
