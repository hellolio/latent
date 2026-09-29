//! fetch_content 工具(index.ts:2530-2884 的 rpi 子集):
//! url/urls 并发抓取,readable/raw/answer 三模式;结果入库返回 responseId
//! 与首片段。PDF/GitHub/YouTube/视频/图片参数省略(已确认取舍)。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use rpi_agent::{Tool, ToolCall, ToolError, ToolOutput, ToolUpdater};

use crate::content::extract;
use crate::prompts;
use crate::storage::{self, StoredType};
use crate::tools::{names, parse_proxy_arg, WebContext};

pub struct FetchContentTool {
    context: Arc<WebContext>,
}

impl FetchContentTool {
    pub fn new(context: Arc<WebContext>) -> Self {
        FetchContentTool { context }
    }

    fn description_text(&self) -> &'static str {
        use std::sync::LazyLock;
        static DESCRIPTION: LazyLock<String> = LazyLock::new(prompts::fetch_content_description);
        &DESCRIPTION
    }
}

struct Params {
    urls: Vec<String>,
    mode: FetchMode,
    prompt: Option<String>,
    proxy: Option<String>,
}

fn parse_params(args: &serde_json::Value) -> Result<Params, String> {
    let obj = args.as_object().ok_or("arguments must be an object")?;
    let mut urls: Vec<String> = Vec::new();
    if let Some(list) = obj.get("urls") {
        let items = list
            .as_array()
            .ok_or("`urls` must be an array of strings")?;
        for item in items {
            let url = item
                .as_str()
                .ok_or("`urls` entries must be strings")?
                .trim()
                .to_string();
            if !url.is_empty() && !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    if urls.is_empty() {
        if let Some(url) = obj.get("url") {
            let value = url
                .as_str()
                .ok_or("`url` must be a string")?
                .trim()
                .to_string();
            if !value.is_empty() {
                urls.push(value);
            }
        }
    }
    if urls.is_empty() {
        return Err("Provide `url` or `urls`".to_string());
    }
    if urls.len() > 10 {
        return Err("`urls` supports at most 10 entries".to_string());
    }
    let mode = match obj.get("mode") {
        Some(value) if !value.is_null() => {
            let value = value.as_str().ok_or("`mode` must be a string")?;
            match value {
                "readable" => FetchMode::Readable,
                "raw" => FetchMode::Raw,
                "answer" => FetchMode::Answer,
                other => return Err(format!("`mode` must be readable, raw, or answer, got `{other}`")),
            }
        }
        _ => FetchMode::Readable,
    };
    let prompt = match obj.get("prompt") {
        Some(value) if !value.is_null() => Some(
            value
                .as_str()
                .ok_or("`prompt` must be a string")?
                .to_string(),
        ),
        _ => None,
    };
    if mode == FetchMode::Answer && prompt.as_deref().map(str::trim).unwrap_or_default().is_empty()
    {
        return Err("answer mode requires `prompt` (the page-local question)".to_string());
    }
    let proxy = parse_proxy_arg(args)?;
    Ok(Params {
        urls,
        mode,
        prompt,
        proxy,
    })
}

// answer 与 readable/raw 的 enum 区分放在这里,避免 parse 返回复合值
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchMode {
    Readable,
    Raw,
    Answer,
}

#[async_trait]
impl Tool for FetchContentTool {
    fn name(&self) -> &str {
        names::FETCH_CONTENT
    }

    fn description(&self) -> &str {
        self.description_text()
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(prompts::FETCH_CONTENT_PROMPT_SNIPPET.to_string())
    }

    fn schema(&self) -> serde_json::Value {
        use crate::prompts as p;
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": p::FETCH_CONTENT_PARAM_URL },
                "urls": { "type": "array", "items": { "type": "string" }, "description": p::FETCH_CONTENT_PARAM_URLS },
                "prompt": { "type": "string", "description": p::FETCH_CONTENT_PARAM_PROMPT },
                "mode": { "type": "string", "enum": ["readable", "raw", "answer"], "description": p::fetch_content_param_mode() },
                "proxy": { "type": "string", "description": p::FETCH_CONTENT_PARAM_PROXY }
            },
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        call: ToolCall,
        cancel: CancellationToken,
        _updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        let params = parse_params(&call.args)
            .map_err(|message| ToolError::Failed { name: call.name.clone(), message })?;
        let max_inline = self.context.max_inline_chars();

        let extracted = extract::extract_urls(
            &params.urls,
            match params.mode {
                FetchMode::Readable => extract::FetchMode::Readable,
                FetchMode::Raw => extract::FetchMode::Raw,
                FetchMode::Answer => extract::FetchMode::Readable, // answer 先抓正文再问答
            },
            params.proxy.as_deref(),
            &self.context.config,
            &cancel,
        )
        .await;
        if cancel.is_cancelled() {
            return Err(ToolError::Aborted { name: call.name.clone() });
        }

        // answer 模式:对第一个成功抓取的页面问答
        if params.mode == FetchMode::Answer {
            let page = extracted
                .iter()
                .find(|entry| entry.error.is_none() && !entry.content.is_empty())
                .ok_or_else(|| ToolError::Failed {
                    name: call.name.clone(),
                    message: "answer mode requires at least one successfully fetched page".to_string(),
                })?;
            let answer = crate::content::page_query::answer_from_page(
                &self.context.llm,
                &self.context.config,
                params.prompt.as_deref().expect("validated"),
                &page.content,
                &page.url,
                &cancel,
            )
            .await
            .map_err(|message| ToolError::Failed { name: call.name.clone(), message })?;
            let fetch_id = storage::generate_id();
            storage::store_fetched_content_result(&fetch_id, extracted, self.context.cache_limits);
            let output = format!(
                "{}\n\n---\n\nFull page content is stored as responseId \"{fetch_id}\". Use {}({{ responseId: \"{fetch_id}\", urlIndex: 0, offset: 0, limit: {max_inline} }}) to retrieve the first bounded page.",
                answer.text,
                names::GET_SEARCH_CONTENT,
            );
            return Ok(ToolOutput {
                output,
                details: json!({
                    "mode": "answer",
                    "responseId": fetch_id,
                    "answerModel": answer.model,
                    "inputChars": answer.input_chars,
                    "originalInputChars": answer.original_input_chars,
                    "truncated": answer.truncated,
                }),
                terminate: false,
            });
        }

        // readable / raw:入库 + 返回首片段(每个 URL 一节)
        let fetch_id = storage::generate_id();
        let stored = storage::store_fetched_content_result(
            &fetch_id,
            extracted.clone(),
            self.context.cache_limits,
        );
        let metadata = storage::fetch_url_metadata(&stored);
        let mut output = if extracted.len() == 1 {
            String::new()
        } else {
            format!("Fetched {} URLs.\n\n", extracted.len())
        };
        for (index, entry) in extracted.iter().enumerate() {
            if extracted.len() > 1 {
                output.push_str(&format!("## [{index}] {}\n\n", entry.url));
            }
            if let Some(error) = &entry.error {
                output.push_str(&format!("Error: {error}\n\n"));
                continue;
            }
            let end_offset = max_inline.min(entry.content.chars().count());
            let slice: String = entry.content.chars().take(end_offset).collect();
            output.push_str(&slice);
            if end_offset < entry.content.chars().count() {
                output.push_str(&format!(
                    "\n\n---\n[Output truncated.] Showing chars 0-{end_offset} of {}. Use {}({{ responseId: \"{fetch_id}\", urlIndex: {index}, offset: {end_offset}, limit: {max_inline} }}) for the next slice.",
                    entry.content.chars().count(),
                    names::GET_SEARCH_CONTENT
                ));
            }
            output.push_str("\n\n");
        }
        let _ = metadata;
        Ok(ToolOutput {
            output: output.trim().to_string(),
            details: json!({
                "responseId": fetch_id,
                "mode": match params.mode { FetchMode::Readable => "readable", FetchMode::Raw => "raw", FetchMode::Answer => "answer" },
                "storedType": StoredType::Fetch,
                "urlCount": extracted.len(),
                "urls": extracted.iter().map(|entry| json!({
                    "url": entry.url,
                    "error": entry.error,
                    "contentLength": entry.content.chars().count(),
                })).collect::<Vec<_>>(),
            }),
            terminate: false,
        })
    }
}
