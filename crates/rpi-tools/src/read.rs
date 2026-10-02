//! read 工具(05 文档 §3):文本读取 + offset/limit 切片 + 双限截断 + 续读提示。
//! 图片读取(05 文档的 ImageContent 路径)依赖模型多模态参数,M4 扩展时补齐。

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use rpi_agent::{Tool, ToolCall, ToolError, ToolOutput, ToolUpdater};

use crate::truncate::{truncate_head, OutputLimits};

pub struct ReadTool {
    cwd: std::path::PathBuf,
    limits: OutputLimits,
    description: String,
}

/// 工厂:cwd 解析相对路径(pi 的 resolveToCwd)。
pub fn create_read_tool(cwd: &Path) -> Arc<dyn Tool> {
    create_read_tool_with_limits(cwd, OutputLimits::default())
}

/// 工厂 + 输出上限注入(装配层统一派生值,truncate 模块文档)。
pub fn create_read_tool_with_limits(cwd: &Path, limits: OutputLimits) -> Arc<dyn Tool> {
    let description = format!(
        "Read a text file from the local filesystem. Returns the file content, \
         truncated to {} lines / {} bytes. Use offset/limit to page through large files.",
        limits.max_lines,
        limits.effective_max_bytes()
    );
    Arc::new(ReadTool {
        cwd: cwd.to_path_buf(),
        limits,
        description,
    })
}

fn parse_args(args: &serde_json::Value) -> Result<(String, Option<usize>, Option<usize>), String> {
    let obj = args.as_object().ok_or("arguments must be an object")?;
    let path = obj
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("missing required argument `path`")?
        .to_string();
    let offset = match obj.get("offset") {
        Some(v) if !v.is_null() => {
            Some(v.as_u64().ok_or("`offset` must be a positive integer")? as usize)
        }
        _ => None,
    };
    let limit = match obj.get("limit") {
        Some(v) if !v.is_null() => {
            let l = v.as_u64().ok_or("`limit` must be a positive integer")? as usize;
            if l == 0 {
                return Err("`limit` must be a positive integer".into());
            }
            Some(l)
        }
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
        &self.description
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


    async fn execute(
        &self,
        call: ToolCall,
        _cancel: CancellationToken,
        _updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        let (path, offset, limit) =
            parse_args(&call.args).map_err(|message| ToolError::Failed {
                name: "read".into(),
                message,
            })?;

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

        let bytes = tokio::fs::read(&resolved)
            .await
            .map_err(|e| ToolError::Failed {
                name: "read".into(),
                message: format!("cannot read `{path}`: {e}"),
            })?;
        // 二进制文件(NUL 字节,grep 同判据):from_utf8_lossy 会产出大段 U+FFFD 垃圾
        if bytes.contains(&0) {
            return Err(ToolError::Failed {
                name: "read".into(),
                message: format!("`{path}` appears to be a binary file; use bash to inspect it"),
            });
        }
        let content = String::from_utf8_lossy(&bytes);

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // offset 1 起始;越界报错(05 文档)
        let start = offset.unwrap_or(1);
        if start < 1
            || (!lines.is_empty() && start > total_lines)
            || (lines.is_empty() && start > 1)
        {
            return Err(ToolError::Failed {
                name: "read".into(),
                message: format!(
                    "offset {start} is out of range: `{path}` has {total_lines} lines"
                ),
            });
        }
        let end = limit
            .map(|l| (start - 1 + l).min(total_lines))
            .unwrap_or(total_lines);
        let slice: String = if lines.is_empty() {
            String::new()
        } else {
            lines[start - 1..end].join("\n")
        };

        let truncation = truncate_head(
            &slice,
            self.limits.max_lines,
            self.limits.effective_max_bytes(),
        );
        let mut output = truncation.content.clone();
        // slice 之前被 offset 跳过部分占用的字节量(单行续读提示的 tail -c 偏移需计入)
        let skipped_bytes: usize = lines[..start - 1].iter().map(|l| l.len() + 1).sum();
        // output_lines == 1 的字节截断才是"单行超字节限";多行命中字节限时
        // 仍可按行翻页,报单行文案会把模型引向错误的数据
        if truncation.truncated_by == "bytes" && truncation.output_lines == 1 {
            // 单行超字节限:offset 提示无意义,指引改用 bash(05 文档)
            let shown = output.len();
            let next = skipped_bytes + shown + 1;
            output.push_str(&format!(
                "\n\n[Single line exceeds the byte limit; showing the first {shown} bytes. \
                 Use bash to read further: `tail -c +{next} {path} | head -c {MAX}`]",
                MAX = self.limits.effective_max_bytes()
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
            ToolCall {
                id: "t".into(),
                name: "read".into(),
                args,
            },
            CancellationToken::new(),
            &Noop,
        )
        .await
    }

    fn read_tool(cwd: std::path::PathBuf) -> ReadTool {
        ReadTool {
            cwd,
            limits: OutputLimits::default(),
            description: String::new(),
        }
    }

    #[tokio::test]
    async fn reads_file_with_offset_and_continuation_hint() {
        let dir = std::env::temp_dir().join(format!("rpi-read-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("sample.txt");
        let content = (1..=3000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        tokio::fs::write(&path, &content).await.unwrap();

        let tool = read_tool(dir.clone());
        // 全量读:截断 + 续读提示
        let output = exec(&tool, serde_json::json!({"path": "sample.txt"}))
            .await
            .unwrap();
        assert!(output.output.contains("Use offset="), "应带续读提示");
        // offset 翻页
        let output = exec(
            &tool,
            serde_json::json!({"path": "sample.txt", "offset": 2001}),
        )
        .await
        .unwrap();
        assert!(output.output.starts_with("line 2001"));
        // limit 提前结束 + 剩余行提示
        let output = exec(
            &tool,
            serde_json::json!({"path": "sample.txt", "offset": 1, "limit": 5}),
        )
        .await
        .unwrap();
        assert!(output.output.contains("5 of 3000 lines shown"));
        // offset 越界报错
        let err = exec(
            &tool,
            serde_json::json!({"path": "sample.txt", "offset": 99999}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("out of range"));
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn missing_file_is_error() {
        let tool = read_tool(std::env::temp_dir());
        let err = exec(&tool, serde_json::json!({"path": "definitely-missing.txt"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot read"));
    }

    #[tokio::test]
    async fn multiline_byte_truncation_hints_offset_not_bash() {
        let dir = std::env::temp_dir().join(format!("rpi-read-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        // 多行、总字节超限但每行很短:应走 offset 翻页提示,而非"单行超限"文案
        let content = (1..=3000)
            .map(|i| format!("line-{i}-padding-xxxxxx"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(content.len() > 18_000);
        tokio::fs::write(dir.join("multi.txt"), &content).await.unwrap();

        let tool = read_tool(dir.clone());
        let output = exec(&tool, serde_json::json!({"path": "multi.txt"}))
            .await
            .unwrap();
        assert!(
            !output.output.contains("Single line exceeds"),
            "多行字节截断不应报单行文案: {}",
            &output.output[..200]
        );
        assert!(output.output.contains("Use offset="));

        // 单行超限:保留 bash 续读提示,且偏移计入 offset 跳过的字节
        let huge = "z".repeat(90_000);
        tokio::fs::write(dir.join("single.txt"), format!("prefix\n{huge}")).await.unwrap();
        let output = exec(
            &tool,
            serde_json::json!({"path": "single.txt", "offset": 2}),
        )
        .await
        .unwrap();
        let next: usize = output
            .output
            .rsplit("tail -c +")
            .next()
            .and_then(|rest| rest.split(' ').next())
            .and_then(|n| n.parse().ok())
            .expect("应带 tail -c 续读提示");
        // offset=1 跳过 "prefix\n"(7 字节)后再截断,提示偏移 = 7 + 已显示字节 + 1
        assert_eq!(
            next,
            "prefix\n".len() + output.output.split("\n\n").next().unwrap().len() + 1
        );
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn zero_limit_and_binary_file_are_rejected() {
        let dir = std::env::temp_dir().join(format!("rpi-read-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("bin.dat"), [b'a', 0, b'b']).await.unwrap();

        let tool = read_tool(dir.clone());
        let err = exec(&tool, serde_json::json!({"path": "bin.dat", "limit": 0}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("positive integer"));
        let err = exec(&tool, serde_json::json!({"path": "bin.dat"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("binary file"));
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
