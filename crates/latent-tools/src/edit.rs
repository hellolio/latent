//! edit 工具(05 文档 §5):多点精确替换 —— 每个 oldText 在**原始文件**中唯一
//! 且互不重叠;写回时恢复原 BOM 与行尾(pi 的 normalizeToLF + 恢复)。

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use latent_agent::{Tool, ToolCall, ToolError, ToolOutput, ToolUpdater};

pub struct EditTool {
    cwd: std::path::PathBuf,
}

/// 工厂。
pub fn create_edit_tool(cwd: &Path) -> Arc<dyn Tool> {
    Arc::new(EditTool {
        cwd: cwd.to_path_buf(),
    })
}

struct Edit {
    old_text: String,
    new_text: String,
}

fn parse_edits(args: &serde_json::Value) -> Result<Vec<Edit>, String> {
    let obj = args.as_object().ok_or("arguments must be an object")?;
    let edits_value = obj
        .get("edits")
        .ok_or("missing required argument `edits`")?;
    let list = edits_value.as_array().ok_or("`edits` must be an array")?;
    if list.is_empty() {
        return Err("`edits` must not be empty".into());
    }
    let mut edits = Vec::with_capacity(list.len());
    for edit in list {
        let edit = edit.as_object().ok_or("each edit must be an object")?;
        let old_text = edit
            .get("oldText")
            .and_then(|v| v.as_str())
            .ok_or("each edit requires string `oldText`")?;
        if old_text.is_empty() {
            return Err("oldText must not be empty".into());
        }
        let new_text = edit
            .get("newText")
            .and_then(|v| v.as_str())
            .ok_or("each edit requires string `newText`")?;
        edits.push(Edit {
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        });
    }
    Ok(edits)
}

fn detect_line_ending(content: &str) -> &'static str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// 应用多点编辑(全部针对原始内容匹配,互不重叠;05 文档 applyEditsToNormalizedContent)。
fn apply_edits(content: &str, edits: &[Edit]) -> Result<(String, usize), String> {
    let mut ranges: Vec<(usize, usize, &Edit)> = Vec::new();
    for edit in edits {
        let mut matches: Vec<usize> = Vec::new();
        let mut from = 0usize;
        while let Some(pos) = content[from..].find(edit.old_text.as_str()) {
            let absolute = from + pos;
            matches.push(absolute);
            // 按字符边界推进(等价于推进一个字符):+1 字节会落入多字节字符内部,
            // 下一轮 content[from..] 直接 panic。
            from = next_char_boundary(content, absolute + 1);
            if matches.len() > 1 {
                break;
            }
        }
        if matches.is_empty() {
            return Err(format!(
                "oldText not found: {:?}",
                ellipsize(&edit.old_text)
            ));
        }
        if matches.len() > 1 {
            return Err(format!(
                "oldText is not unique ({} matches): {:?}",
                matches.len() + count_rest(content, &edit.old_text, from),
                ellipsize(&edit.old_text)
            ));
        }
        let start = matches[0];
        let range = start..start + edit.old_text.len();
        for (existing_start, existing_end, _) in &ranges {
            if range.start < *existing_end && existing_start < &range.end {
                return Err(format!(
                    "edits overlap near {:?}",
                    ellipsize(&edit.old_text)
                ));
            }
        }
        ranges.push((range.start, range.end, edit));
    }
    ranges.sort_by_key(|(start, _, _)| *start);
    let first_changed_line = content[..ranges[0].0].matches('\n').count() + 1;

    let mut out = String::with_capacity(content.len());
    let mut cursor = 0usize;
    for (start, end, edit) in &ranges {
        out.push_str(&content[cursor..*start]);
        out.push_str(&edit.new_text);
        cursor = *end;
    }
    out.push_str(&content[cursor..]);
    Ok((out, first_changed_line))
}

/// index 不是字符边界时,向后对齐到第一个字符边界(调用方保证 index ≤ len)。
fn next_char_boundary(content: &str, mut index: usize) -> usize {
    while index < content.len() && !content.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// 统计剩余匹配数。前置条件由 apply_edits 保证:from 为字符边界且 ≤ len,needle 非空。
fn count_rest(content: &str, needle: &str, from: usize) -> usize {
    let mut count = 0usize;
    let mut from = from;
    while let Some(pos) = content[from..].find(needle) {
        count += 1;
        from += pos + needle.len().max(1);
    }
    count
}

fn ellipsize(text: &str) -> String {
    if text.chars().count() <= 60 {
        return text.to_string();
    }
    // 按 char 边界截断(字节切片会在多字节字符内部 panic)
    let prefix: String = text.chars().take(57).collect();
    format!("{prefix}…")
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Make exact multi-point replacements in a text file. Each oldText must be \
         unique in the original file and edits must not overlap; all edits are \
         matched against the original content in one call."
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["path", "edits"],
            "properties": {
                "path": {"type": "string"},
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["oldText", "newText"],
                        "properties": {
                            "oldText": {"type": "string"},
                            "newText": {"type": "string"}
                        }
                    }
                }
            }
        })
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some("edit(path, edits): exact string replacements; every oldText must be unique and non-overlapping".into())
    }


    async fn execute(
        &self,
        call: ToolCall,
        _cancel: CancellationToken,
        _updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        let obj = call.args.as_object().ok_or(ToolError::Failed {
            name: "edit".into(),
            message: "arguments must be an object".into(),
        })?;
        let path = obj
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::Failed {
                name: "edit".into(),
                message: "missing required argument `path`".into(),
            })?
            .to_string();
        let edits = parse_edits(&call.args).map_err(|message| ToolError::Failed {
            name: "edit".into(),
            message,
        })?;

        let resolved = if Path::new(&path).is_absolute() {
            std::path::PathBuf::from(&path)
        } else {
            self.cwd.join(&path)
        };
        let bytes = tokio::fs::read(&resolved)
            .await
            .map_err(|e| ToolError::Failed {
                name: "edit".into(),
                message: format!("cannot read `{path}`: {e}"),
            })?;
        let raw = String::from_utf8_lossy(&bytes).into_owned();

        // 剥 BOM + 归一到 LF,写回时恢复(05 文档)
        let (bom, content_raw) = match raw.strip_prefix('\u{feff}') {
            Some(stripped) => (true, stripped.to_string()),
            None => (false, raw.clone()),
        };
        let line_ending = detect_line_ending(&content_raw);
        // 直接在原始内容上替换:old/new 的换行先归一到 LF 再转成文件行尾。
        // 不做"整体归一 → 写回转回":混合行尾文件会把未编辑区域的裸 LF
        // 也改写成 CRLF(超出编辑范围的内容变异)
        let edits_native: Vec<Edit> = edits
            .iter()
            .map(|edit| Edit {
                old_text: edit
                    .old_text
                    .replace("\r\n", "\n")
                    .replace('\n', line_ending),
                new_text: edit
                    .new_text
                    .replace("\r\n", "\n")
                    .replace('\n', line_ending),
            })
            .collect();

        let (edited, first_changed_line) =
            apply_edits(&content_raw, &edits_native).map_err(|message| ToolError::Failed {
                name: "edit".into(),
                message,
            })?;
        let mut output_text = edited;
        if bom {
            output_text.insert(0, '\u{feff}');
        }
        tokio::fs::write(&resolved, output_text)
            .await
            .map_err(|e| ToolError::Failed {
                name: "edit".into(),
                message: format!("cannot write `{path}`: {e}"),
            })?;

        let changed: Vec<serde_json::Value> = edits
            .iter()
            .map(|edit| json!({"oldText": ellipsize(&edit.old_text), "newText": ellipsize(&edit.new_text)}))
            .collect();
        Ok(ToolOutput {
            output: format!(
                "Applied {} edit(s) to `{path}` (first changed line: {first_changed_line}).",
                edits.len()
            ),
            details: json!({"edits": changed, "firstChangedLine": first_changed_line}),
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

    async fn exec(tool: &EditTool, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        tool.execute(
            ToolCall {
                id: "t".into(),
                name: "edit".into(),
                args,
            },
            CancellationToken::new(),
            &Noop,
        )
        .await
    }

    #[tokio::test]
    async fn multi_point_edit_replaces_all_and_restores_crlf() {
        let dir = std::env::temp_dir().join(format!("latent-edit-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("code.txt");
        tokio::fs::write(&path, "alpha beta\r\nsecond line\r\nalpha gamma\r\n")
            .await
            .unwrap();

        let tool = EditTool { cwd: dir.clone() };
        let output = exec(
            &tool,
            serde_json::json!({
                "path": "code.txt",
                "edits": [
                    {"oldText": "beta", "newText": "BETA"},
                    {"oldText": "gamma", "newText": "GAMMA"}
                ]
            }),
        )
        .await
        .unwrap();
        assert!(output.output.contains("Applied 2 edit(s)"));

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("alpha BETA"));
        assert!(content.contains("alpha GAMMA"));
        assert!(content.contains("\r\n"), "应恢复 CRLF 行尾");
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn mixed_line_endings_unedited_lf_lines_are_untouched() {
        let dir = std::env::temp_dir().join(format!("latent-edit-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        // 混合行尾:编辑只允许影响被编辑行,未编辑区域的裸 LF 不得被改写成 CRLF
        let path = dir.join("mixed.txt");
        tokio::fs::write(&path, "crlf line\r\nlf line\nplain crlf\r\n")
            .await
            .unwrap();

        let tool = EditTool { cwd: dir.clone() };
        let output = exec(
            &tool,
            serde_json::json!({
                "path": "mixed.txt",
                "edits": [{"oldText": "crlf line", "newText": "crlf LINE"}]
            }),
        )
        .await
        .unwrap();
        assert!(output.output.contains("Applied 1 edit(s)"));

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(
            content, "crlf LINE\r\nlf line\nplain crlf\r\n",
            "未编辑区域的行尾必须原样保留"
        );
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn non_unique_and_missing_old_text_fail_without_writing() {
        let dir = std::env::temp_dir().join(format!("latent-edit-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("dup.txt");
        tokio::fs::write(&path, "same same different")
            .await
            .unwrap();
        let tool = EditTool { cwd: dir.clone() };

        let err = exec(
            &tool,
            serde_json::json!({"path": "dup.txt", "edits": [{"oldText": "same", "newText": "x"}]}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not unique"));

        let err = exec(
            &tool,
            serde_json::json!({"path": "dup.txt", "edits": [{"oldText": "nope", "newText": "x"}]}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not found"));

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "same same different", "失败时不得写回");
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn multibyte_first_char_match_does_not_panic() {
        let dir = std::env::temp_dir().join(format!("latent-edit-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        // 回归:oldText 首个匹配以多字节字符开头时,扫描起点 +1 字节曾落入字符内部 panic
        let path = dir.join("arrow.txt");
        tokio::fs::write(&path, "前置 →abc 后置\n").await.unwrap();
        let tool = EditTool { cwd: dir.clone() };
        let output = exec(
            &tool,
            serde_json::json!({
                "path": "arrow.txt",
                "edits": [{"oldText": "→abc", "newText": "→XYZ"}]
            }),
        )
        .await
        .unwrap();
        assert!(output.output.contains("Applied 1 edit(s)"));
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("→XYZ 后置"));
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn multibyte_not_unique_reports_error_without_panic() {
        let dir = std::env::temp_dir().join(format!("latent-edit-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        // 回归:第二个匹配起点为多字节字符时,count_rest 的入参 from 曾是非边界
        let path = dir.join("dup_arrow.txt");
        tokio::fs::write(&path, "→ab →ab\n").await.unwrap();
        let tool = EditTool { cwd: dir.clone() };
        let err = exec(
            &tool,
            serde_json::json!({
                "path": "dup_arrow.txt",
                "edits": [{"oldText": "→ab", "newText": "x"}]
            }),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not unique"));
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "→ab →ab\n", "失败时不得写回");
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn four_byte_emoji_first_char_match_does_not_panic() {
        let dir = std::env::temp_dir().join(format!("latent-edit-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        // 4 字节 UTF-8 字符(emoji)开头的匹配,边界对齐需跨 3 字节
        let path = dir.join("emoji.txt");
        tokio::fs::write(&path, "🎉ok end\n").await.unwrap();
        let tool = EditTool { cwd: dir.clone() };
        let output = exec(
            &tool,
            serde_json::json!({
                "path": "emoji.txt",
                "edits": [{"oldText": "🎉ok", "newText": "🎉OK"}]
            }),
        )
        .await
        .unwrap();
        assert!(output.output.contains("Applied 1 edit(s)"));
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("🎉OK end"));
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn empty_old_text_rejected_without_writing() {
        let dir = std::env::temp_dir().join(format!("latent-edit-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        // 回归:空 oldText 曾在 count_rest 中越界 panic,现在校验层直接拒绝
        let path = dir.join("empty.txt");
        tokio::fs::write(&path, "hello\n").await.unwrap();
        let tool = EditTool { cwd: dir.clone() };
        let err = exec(
            &tool,
            serde_json::json!({
                "path": "empty.txt",
                "edits": [{"oldText": "", "newText": "x"}]
            }),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("oldText must not be empty"));
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "hello\n", "失败时不得写回");
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
