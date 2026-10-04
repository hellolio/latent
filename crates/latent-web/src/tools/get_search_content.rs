//! get_search_content 工具(index.ts:2885-3260 的移植):
//! responseId 检索三分支(research artifact / search 全量结果 / fetch 全文),
//! offset/limit 分页切片、findText 定位(exact/case-insensitive/fuzzy)、
//! 错误时给模型可自纠的指引文案。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use latent_agent::{Tool, ToolCall, ToolError, ToolOutput, ToolUpdater};

use crate::content::find::{find_content, FindMode};
use crate::prompts;
use crate::storage::{self, StoredType};
use crate::tools::{names, WebContext};

pub struct GetSearchContentTool {
    context: Arc<WebContext>,
}

impl GetSearchContentTool {
    pub fn new(context: Arc<WebContext>) -> Self {
        GetSearchContentTool { context }
    }
}

/// 参数(已归一化:findText 提供时忽略 offset/limit)。
struct Params {
    response_id: String,
    query: Option<String>,
    query_index: Option<usize>,
    url: Option<String>,
    url_index: Option<usize>,
    offset: Option<i64>,
    limit: Option<i64>,
    find_text: Option<Vec<String>>,
    find_mode: Option<FindMode>,
}

fn parse_params(args: &serde_json::Value) -> Result<Params, String> {
    let obj = args.as_object().ok_or("arguments must be an object")?;
    let response_id = obj
        .get("responseId")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .ok_or("missing required argument `responseId`")?
        .to_string();
    let string_field = |key: &str| -> Result<Option<String>, String> {
        match obj.get(key) {
            Some(value) if !value.is_null() => Ok(Some(
                value
                    .as_str()
                    .ok_or(format!("`{key}` must be a string"))?
                    .trim()
                    .to_string(),
            )),
            _ => Ok(None),
        }
    };
    let index_field = |key: &str| -> Result<Option<usize>, String> {
        match obj.get(key) {
            Some(value) if !value.is_null() => {
                let raw = value.as_i64().ok_or(format!("`{key}` must be an integer"))?;
                if raw < 0 {
                    return Err(format!("`{key}` must be a non-negative integer"));
                }
                Ok(Some(raw as usize))
            }
            _ => Ok(None),
        }
    };
    let query = string_field("query")?;
    let url = string_field("url")?;
    let find_text = match obj.get("findText") {
        Some(value) if !value.is_null() => {
            let mut texts: Vec<String> = Vec::new();
            match value {
                serde_json::Value::String(text) => texts.push(text.clone()),
                serde_json::Value::Array(items) => {
                    for item in items {
                        texts.push(
                            item.as_str()
                                .ok_or("`findText` entries must be strings")?
                                .to_string(),
                        );
                    }
                }
                _ => return Err("`findText` must be a string or an array of strings".to_string()),
            }
            if texts.is_empty() || texts.len() > 10 {
                return Err("`findText` must contain 1-10 entries".to_string());
            }
            Some(texts)
        }
        _ => None,
    };
    let find_mode = match obj.get("findMode") {
        Some(value) if !value.is_null() => Some(
            FindMode::parse(value.as_str().ok_or("`findMode` must be a string")?)
                .ok_or("`findMode` must be exact, case-insensitive, or fuzzy")?,
        ),
        _ => None,
    };
    if find_mode.is_some() && find_text.is_none() {
        return Err("findMode requires findText; provide findText or omit findMode".to_string());
    }
    Ok(Params {
        response_id,
        query,
        query_index: index_field("queryIndex")?,
        url,
        url_index: index_field("urlIndex")?,
        offset: int_field(obj, "offset")?,
        limit: int_field(obj, "limit")?,
        find_text,
        find_mode,
    })
}

fn int_field(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Option<i64>, String> {
    match obj.get(key) {
        Some(value) if !value.is_null() => {
            Ok(Some(value.as_i64().ok_or(format!("`{key}` must be an integer"))?))
        }
        _ => Ok(None),
    }
}

/// 错误以 ToolError::Failed 返回(latent 约定),消息保留上游可自纠指引原文。
fn fail(call: &ToolCall, message: impl Into<String>) -> ToolError {
    ToolError::Failed {
        name: call.name.clone(),
        message: message.into(),
    }
}

fn format_input_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => serde_json::to_string(text).unwrap_or_default(),
        other => other.to_string(),
    }
}

#[async_trait]
impl Tool for GetSearchContentTool {
    fn name(&self) -> &str {
        names::GET_SEARCH_CONTENT
    }

    fn description(&self) -> &str {
        use std::sync::LazyLock;
        static DESCRIPTION: LazyLock<String> =
            LazyLock::new(prompts::get_search_content_description);
        &DESCRIPTION
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(prompts::get_search_content_prompt_snippet())
    }

    fn schema(&self) -> serde_json::Value {
        use crate::prompts as p;
        let max_inline = self.context.max_inline_chars();
        json!({
            "type": "object",
            "required": ["responseId"],
            "properties": {
                "responseId": { "type": "string", "description": p::get_search_content_param_response_id() },
                "query": { "type": "string", "description": p::SEARCH_QUERY_DESCRIPTION },
                "queryIndex": { "type": "integer", "minimum": 0, "description": p::GET_SEARCH_CONTENT_PARAM_QUERY_INDEX },
                "url": { "type": "string", "description": p::GET_SEARCH_CONTENT_PARAM_URL },
                "urlIndex": { "type": "integer", "minimum": 0, "description": p::GET_SEARCH_CONTENT_PARAM_URL_INDEX },
                "offset": { "type": "integer", "minimum": 0, "description": p::GET_SEARCH_CONTENT_PARAM_OFFSET },
                "limit": { "type": "integer", "minimum": 1, "maximum": max_inline, "description": p::GET_SEARCH_CONTENT_PARAM_LIMIT },
                "findText": {
                    "anyOf": [
                        { "type": "string", "minLength": 1, "maxLength": 500 },
                        { "type": "array", "items": { "type": "string", "minLength": 1, "maxLength": 500 }, "minItems": 1, "maxItems": 10 }
                    ],
                    "description": p::GET_SEARCH_CONTENT_PARAM_FIND_TEXT
                },
                "findMode": { "type": "string", "enum": ["exact", "case-insensitive", "fuzzy"], "description": p::GET_SEARCH_CONTENT_PARAM_FIND_MODE }
            },
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        call: ToolCall,
        _cancel: CancellationToken,
        _updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        let params = parse_params(&call.args).map_err(|message| fail(&call, message))?;
        let max_inline = self.context.max_inline_chars();
        let data = storage::get_result(&params.response_id);
        let Some(data) = data else {
            return Err(fail(
                &call,
                format!(
                    "Error: No stored results for responseId {}. Use a responseId returned by {}.",
                    format_input_value(&json!(params.response_id)),
                    prompts::STORED_CONTENT_SOURCES
                ),
            ));
        };
        // findText 语义:提供时忽略 offset/limit
        let find_text = params.find_text.clone();
        let find_mode = params.find_mode.unwrap_or(FindMode::CaseInsensitive);

        match data.stored_type {
            StoredType::Research => {
                let Some(artifact) = data.artifact.clone() else {
                    return Err(fail(
                        &call,
                        format!(
                            "Error: stored research artifact for responseId {} was not found. Use a responseId returned by {}.",
                            format_input_value(&json!(params.response_id)),
                            prompts::STORED_CONTENT_SOURCES
                        ),
                    ));
                };
                // artifact 是给模型读的数据:紧凑 JSON(键已语义化,
                // pretty 缩进只多耗 ~20-30% token)
                let serialized = serde_json::to_string(&artifact).unwrap_or_else(|_| "{}".to_string());
                if let Some(texts) = &find_text {
                    let found = find_content(&serialized, texts, find_mode);
                    return Ok(ToolOutput {
                        output: found.text,
                        details: json!({
                            "responseId": params.response_id,
                            "type": "research",
                            "contentLength": serialized.chars().count(),
                            "findMode": find_mode.as_str(),
                            "matchCount": found.match_count,
                            "returnedMatches": found.returned_matches,
                        }),
                        terminate: false,
                    });
                }
                let (offset, limit) = validate_offset_limit(
                    &call,
                    params.offset,
                    params.limit,
                    max_inline,
                    None,
                )?;
                let chars: Vec<char> = serialized.chars().collect();
                if offset > chars.len() {
                    return Err(fail(
                        &call,
                        format!(
                            "Offset {offset} is out of range for responseId {}. Received offset {offset}; valid range is 0-{}. Use an offset within that range.",
                            format_input_value(&json!(params.response_id)),
                            chars.len(),
                        ),
                    ));
                }
                let end_offset = (offset + limit).min(chars.len());
                let slice: String = chars[offset..end_offset].iter().collect();
                let has_more = end_offset < chars.len();
                Ok(ToolOutput {
                    output: slice,
                    details: json!({
                        "responseId": params.response_id,
                        "type": "research",
                        "contentLength": chars.len(),
                        "offset": offset,
                        "limit": limit,
                        "returnedChars": end_offset - offset,
                        "nextOffset": if has_more { serde_json::json!(end_offset) } else { serde_json::Value::Null },
                        "truncated": has_more,
                    }),
                    terminate: false,
                })
            }
            StoredType::Search => {
                let Some(queries) = data.queries.clone() else {
                    return Err(fail(
                        &call,
                        format!(
                            "Invalid stored data for responseId {}: received type search without queries. Use a responseId returned by {}.",
                            format_input_value(&json!(params.response_id)),
                            prompts::STORED_CONTENT_SOURCES
                        ),
                    ));
                };
                // query / queryIndex 选择
                let (query_data, query_index) = if let Some(query) = &params.query {
                    match queries.iter().position(|entry| &entry.query == query) {
                        Some(index) => (&queries[index], index),
                        None => {
                            let available = queries
                                .iter()
                                .map(|entry| format!("\"{}\"", entry.query))
                                .collect::<Vec<_>>()
                                .join(", ");
                            return Err(fail(
                                &call,
                                format!(
                                    "Query {} was not found for responseId {}. Received query={}. Available queries: {available}. Use one of the available queries or queryIndex.",
                                    format_input_value(&json!(query)),
                                    format_input_value(&json!(params.response_id)),
                                    format_input_value(&json!(query)),
                                ),
                            ));
                        }
                    }
                } else if let Some(query_index) = params.query_index {
                    match queries.get(query_index) {
                        Some(entry) => (entry, query_index),
                        None => {
                            let available = queries
                                .iter()
                                .enumerate()
                                .map(|(index, entry)| format!("{index}: \"{}\"", entry.query))
                                .collect::<Vec<_>>()
                                .join(", ");
                            return Err(fail(
                                &call,
                                format!(
                                    "Query index {} is out of range for responseId {}. Received queryIndex={}; valid indexes are 0-{}. Available queries: {available}. Use one of the available indexes.",
                                    format_input_value(&json!(params.query_index.unwrap_or(0) as i64)),
                                    format_input_value(&json!(params.response_id)),
                                    params.query_index.unwrap_or(0),
                                    queries.len().saturating_sub(1),
                                ),
                            ));
                        }
                    }
                } else if queries.len() == 1 {
                    // 单 query 免选:唯一条目默认取 index 0,省一次必错往返
                    let index = 0;
                    (&queries[index], index)
                } else {
                    let available = queries
                        .iter()
                        .enumerate()
                        .map(|(index, entry)| format!("{index}: \"{}\"", entry.query))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(fail(
                        &call,
                        format!(
                            "Specify query or queryIndex for responseId {}. Available queries: {available}.",
                            format_input_value(&json!(params.response_id))
                        ),
                    ));
                };
                if let Some(error) = &query_data.error {
                    return Err(fail(
                        &call,
                        format!(
                            "Error retrieving query {} from responseId {}: {error}. Check the stored search result and retry with another query or queryIndex if needed.",
                            format_input_value(&json!(query_data.query)),
                            format_input_value(&json!(params.response_id)),
                        ),
                    ));
                }
                let full_results = prompts::format_full_results(query_data);
                if let Some(texts) = &find_text {
                    let found = find_content(&full_results, texts, find_mode);
                    return Ok(ToolOutput {
                        output: found.text,
                        details: json!({
                            "responseId": params.response_id,
                            "query": query_data.query,
                            "resultCount": query_data.results.len(),
                            "contentLength": full_results.chars().count(),
                            "findMode": find_mode.as_str(),
                            "matchCount": found.match_count,
                            "returnedMatches": found.returned_matches,
                        }),
                        terminate: false,
                    });
                }
                let (offset, limit) = validate_offset_limit(
                    &call,
                    params.offset,
                    params.limit,
                    max_inline,
                    Some(&query_data.query),
                )?;
                let chars: Vec<char> = full_results.chars().collect();
                let result = paginate_slice(
                    &call,
                    chars,
                    offset,
                    limit,
                    max_inline,
                    params.response_id.clone(),
                    query_index,
                )?;
                let mut details = result.details;
                if let Some(object) = details.as_object_mut() {
                    object.insert("query".to_string(), json!(query_data.query));
                    object.insert("resultCount".to_string(), json!(query_data.results.len()));
                }
                Ok(ToolOutput {
                    output: result.output,
                    details,
                    terminate: false,
                })
            }
            StoredType::Fetch => {
                let Some(urls) = data.urls.clone() else {
                    return Err(fail(
                        &call,
                        format!(
                            "Invalid stored data for responseId {}: fetched content is unavailable.",
                            format_input_value(&json!(params.response_id))
                        ),
                    ));
                };
                // url / urlIndex 选择
                let (url_data, selected_url_index) = if let Some(url) = &params.url {
                    match urls.iter().position(|entry| &entry.url == url) {
                        Some(index) => (&urls[index], index),
                        None => {
                            let available = urls
                                .iter()
                                .map(|entry| entry.url.clone())
                                .collect::<Vec<_>>()
                                .join("\n  ");
                            return Err(fail(
                                &call,
                                format!(
                                    "URL {} was not found for responseId {}. Received url={}. Available URLs:\n  {available}\nUse one of the available URLs or urlIndex.",
                                    format_input_value(&json!(url)),
                                    format_input_value(&json!(params.response_id)),
                                    format_input_value(&json!(url)),
                                ),
                            ));
                        }
                    }
                } else if let Some(url_index) = params.url_index {
                    match urls.get(url_index) {
                        Some(entry) => (entry, url_index),
                        None => {
                            let available = urls
                                .iter()
                                .enumerate()
                                .map(|(index, entry)| format!("{index}: {}", entry.url))
                                .collect::<Vec<_>>()
                                .join("\n  ");
                            return Err(fail(
                                &call,
                                format!(
                                    "URL index {} is out of range for responseId {}. Received urlIndex={}; valid indexes are 0-{}. Available URLs:\n  {available}\nUse one of the available indexes.",
                                    params.url_index.unwrap_or(0),
                                    format_input_value(&json!(params.response_id)),
                                    params.url_index.unwrap_or(0),
                                    urls.len().saturating_sub(1),
                                ),
                            ));
                        }
                    }
                } else {
                    let available = urls
                        .iter()
                        .enumerate()
                        .map(|(index, entry)| format!("{index}: {}", entry.url))
                        .collect::<Vec<_>>()
                        .join("\n  ");
                    return Err(fail(
                        &call,
                        format!(
                            "Specify url or urlIndex for responseId {}. Available URLs:\n  {available}",
                            format_input_value(&json!(params.response_id))
                        ),
                    ));
                };
                if let Some(error) = &url_data.error {
                    return Err(fail(
                        &call,
                        format!(
                            "Error retrieving URL {} from responseId {}: {error}. Check the stored fetch result and retry with another URL or urlIndex if needed.",
                            format_input_value(&json!(url_data.url)),
                            format_input_value(&json!(params.response_id)),
                        ),
                    ));
                }
                if let Some(texts) = &find_text {
                    let found = find_content(&url_data.content, texts, find_mode);
                    return Ok(ToolOutput {
                        output: format!("# {}\n\n{}", if url_data.title.is_empty() { &url_data.url } else { &url_data.title }, found.text),
                        details: json!({
                            "url": url_data.url,
                            "title": url_data.title,
                            "contentLength": url_data.content.chars().count(),
                            "findMode": find_mode.as_str(),
                            "matchCount": found.match_count,
                            "returnedMatches": found.returned_matches,
                        }),
                        terminate: false,
                    });
                }
                let (offset, limit) = validate_offset_limit(&call, params.offset, params.limit, max_inline, None)?;
                let chars: Vec<char> = url_data.content.chars().collect();
                if offset > chars.len() {
                    return Err(fail(
                        &call,
                        format!(
                            "Offset {offset} is out of range for URL {} in responseId {}. Received offset {offset}; valid range is 0-{}. Use an offset within that range.",
                            format_input_value(&json!(url_data.url)),
                            format_input_value(&json!(params.response_id)),
                            chars.len(),
                        ),
                    ));
                }
                let end_offset = (offset + limit).min(chars.len());
                let content_slice: String = chars[offset..end_offset].iter().collect();
                let has_more = end_offset < chars.len();
                let mut text = format!(
                    "# {}\n\n{}",
                    if url_data.title.is_empty() { &url_data.url } else { &url_data.title },
                    content_slice
                );
                if has_more || offset > 0 {
                    text.push_str(&format!(
                        "\n\n---\nShowing chars {offset}-{end_offset} of {}.",
                        chars.len()
                    ));
                    if has_more {
                        text.push_str(&format!(
                            " Use {}({{ responseId: \"{}\", urlIndex: {selected_url_index}, offset: {end_offset}, limit: {limit} }}) for the next slice.",
                            names::GET_SEARCH_CONTENT,
                            params.response_id
                        ));
                    }
                }
                Ok(ToolOutput {
                    output: text,
                    details: json!({
                        "url": url_data.url,
                        "title": url_data.title,
                        "contentLength": chars.len(),
                        "offset": offset,
                        "limit": limit,
                        "returnedChars": end_offset - offset,
                        "nextOffset": if has_more { serde_json::json!(end_offset) } else { serde_json::Value::Null },
                        "truncated": has_more,
                    }),
                    terminate: false,
                })
            }
        }
    }
}

/// offset/limit 校验(非负、1..=max);query 场景错误文案带 query 名。
fn validate_offset_limit(
    call: &ToolCall,
    offset: Option<i64>,
    limit: Option<i64>,
    max_inline: usize,
    query: Option<&str>,
) -> Result<(usize, usize), ToolError> {
    let context = match query {
        Some(query) => format!(" for query {}", format_input_value(&json!(query))),
        None => String::new(),
    };
    let offset = offset.unwrap_or(0);
    if offset < 0 {
        return Err(fail(
            call,
            format!(
                "Invalid offset: received {offset}{context}; offset must be a non-negative integer. Use 0 or a larger integer."
            ),
        ));
    }
    let limit = limit.unwrap_or(max_inline as i64);
    if limit <= 0 || limit > max_inline as i64 {
        return Err(fail(
            call,
            format!(
                "Invalid limit: received {limit}{context}; limit must be an integer from 1 to {max_inline}. Use a value in that range."
            ),
        ));
    }
    Ok((offset as usize, limit as usize))
}

/// search 分支的分页切片(带 continuation 指引,指引占超上限时压缩 returnedChars;
/// index.ts:3050-3077)。
fn paginate_slice(
    call: &ToolCall,
    chars: Vec<char>,
    offset: usize,
    limit: usize,
    cap: usize,
    response_id: String,
    query_index: usize,
) -> Result<ToolOutput, ToolError> {
    let total = chars.len();
    if offset > total {
        return Err(fail(
            call,
            format!(
                "Offset {offset} is out of range for query in responseId {response_id}. Received offset {offset}; valid range is 0-{total}. Use an offset within that range."
            ),
        ));
    }
    let mut returned = limit.min(total - offset);
    let mut end_offset = offset + returned;
    let mut continuation = String::new();
    while end_offset < total {
        continuation = format!(
            "\n\n---\nShowing chars {offset}-{end_offset} of {total}. Use {}({{ responseId: \"{response_id}\", queryIndex: {query_index}, offset: {end_offset}, limit: {limit} }}) for the next slice.",
            names::GET_SEARCH_CONTENT
        );
        // 正文 + 指引贴住 cap(上游:overflow = returnedChars + continuation
        // - max;overflow <= 0 直接采用,否则压缩正文)
        match (returned + continuation.chars().count()).checked_sub(cap) {
            None => break,
            Some(over) if over > 0 && returned > over => {
                returned -= over;
            }
            Some(_) => break,
        }
        end_offset = offset + returned;
    }
    let slice: String = chars[offset..end_offset].iter().collect();
    let has_more = end_offset < total;
    let text = format!("{slice}{}", if has_more { &continuation } else { "" });
    Ok(ToolOutput {
        output: text,
        details: json!({
            "responseId": response_id,
            "queryIndex": query_index,
            "contentLength": total,
            "offset": offset,
            "limit": limit,
            "returnedChars": returned,
            "nextOffset": if has_more { serde_json::json!(end_offset) } else { serde_json::Value::Null },
            "truncated": has_more,
        }),
        terminate: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{QueryResultData, StoredSearchData};
    use crate::types::SearchResult;
    use latent_agent::{Tool, ToolUpdater};
    use std::sync::Arc;

    struct NoopUpdater;
    #[async_trait::async_trait]
    impl ToolUpdater for NoopUpdater {
        async fn update(&self, _partial: String) {}
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    fn context() -> Arc<WebContext> {
        Arc::new(WebContext {
            config: crate::config::WebSearchConfig::default(),
            cache_limits: Default::default(),
            llm: crate::llm::LlmDeps {
                provider: latent_ai::create_mock_provider("x"),
                resolve_model: Arc::new(|spec| Ok(latent_ai::Model::minimal(spec, "mock", "mock"))),
                current_model: Arc::new(|| None),
            },
            notifier: None,
            tool_result_max_chars: 0,
            cwd: std::env::temp_dir(),
        })
    }

    fn store_search(queries: Vec<QueryResultData>) -> String {
        let id = crate::storage::generate_id();
        crate::storage::store_result(
            &id,
            StoredSearchData {
                id: id.clone(),
                stored_type: StoredType::Search,
                timestamp: now_ms(),
                queries: Some(queries),
                urls: None,
                artifact: None,
                fetch_cache: None,
                url_metadata: None,
                fetch_cache_error: None,
            },
        );
        id
    }

    async fn call(tool: &GetSearchContentTool, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        tool.execute(
            ToolCall {
                id: "test".into(),
                name: "get_search_content".into(),
                args,
            },
            CancellationToken::new(),
            &NoopUpdater,
        )
        .await
    }

    fn sample_query() -> QueryResultData {
        QueryResultData {
            query: "rust async".into(),
            answer: "Short answer.".into(),
            results: vec![SearchResult {
                title: "Title".into(),
                url: "https://example.com".into(),
                snippet: "Snippet".into(),
            }],
            error: None,
            provider: Some("brave".into()),
            providers: vec!["brave".into()],
        }
    }

    #[tokio::test]
    async fn paginates_search_results_with_continuation() {
        let tool = GetSearchContentTool::new(context());
        let id = store_search(vec![sample_query()]);
        let output = call(
            &tool,
            serde_json::json!({ "responseId": id, "queryIndex": 0, "offset": 0, "limit": 30 }),
        )
        .await
        .expect("ok");
        assert!(output.output.contains("## Results for: \"rust async\""));
        assert!(output.output.contains("Showing chars 0-"));
        assert!(output
            .output
            .contains(") for the next slice."));
        assert_eq!(
            output.details["truncated"], serde_json::json!(true),
            "limit 30 < full results → hasMore"
        );
    }

    #[tokio::test]
    async fn full_page_has_no_continuation() {
        let tool = GetSearchContentTool::new(context());
        let id = store_search(vec![sample_query()]);
        let output = call(
            &tool,
            serde_json::json!({ "responseId": id, "queryIndex": 0 }),
        )
        .await
        .expect("ok");
        assert!(!output.output.contains("next slice"));
        assert_eq!(output.details["truncated"], serde_json::json!(false));
        assert_eq!(output.details["query"], "rust async");
    }

    #[tokio::test]
    async fn query_lookup_errors_are_self_correcting() {
        let tool = GetSearchContentTool::new(context());
        // 单 query 免选:未指定 query 默认取 index 0
        let single_id = store_search(vec![sample_query()]);
        let output = call(&tool, serde_json::json!({ "responseId": single_id.clone() }))
            .await
            .expect("single query defaults to index 0");
        assert!(output.output.contains("Results for"));
        // 多 query 未指定:报错并列出可用 query(可自纠)
        let multi_id = store_search(vec![sample_query(), sample_query()]);
        let error = call(&tool, serde_json::json!({ "responseId": multi_id }))
            .await
            .expect_err("no query specified");
        assert!(error.to_string().contains("Specify query or queryIndex"));
        // 未命中的 query
        let error = call(
            &tool,
            serde_json::json!({ "responseId": single_id, "query": "missing" }),
        )
        .await
        .expect_err("query not found");
        assert!(error.to_string().contains("Available queries"));
    }

    #[tokio::test]
    async fn errored_query_reports_with_guidance() {
        let tool = GetSearchContentTool::new(context());
        let mut failed = sample_query();
        failed.query = "broken".into();
        failed.error = Some("provider exploded".into());
        let id = store_search(vec![failed]);
        let error = call(
            &tool,
            serde_json::json!({ "responseId": id, "queryIndex": 0 }),
        )
        .await
        .expect_err("stored error");
        assert!(error.to_string().contains("provider exploded"));
    }

    #[tokio::test]
    async fn find_text_on_search_results() {
        let tool = GetSearchContentTool::new(context());
        let id = store_search(vec![sample_query()]);
        let output = call(
            &tool,
            serde_json::json!({ "responseId": id, "queryIndex": 0, "findText": "short answer" }),
        )
        .await
        .expect("ok");
        assert!(output.output.contains("Text matches (case-insensitive)"));
        assert_eq!(output.details["matchCount"], serde_json::json!(1));
    }

    #[tokio::test]
    async fn research_branch_serializes_artifact() {
        let tool = GetSearchContentTool::new(context());
        let id = crate::storage::generate_id();
        crate::storage::store_result(
            &id,
            StoredSearchData {
                id: id.clone(),
                stored_type: StoredType::Research,
                timestamp: now_ms(),
                queries: None,
                urls: None,
                artifact: Some(serde_json::json!({ "query": "claim", "sources": [] })),
                fetch_cache: None,
                url_metadata: None,
                fetch_cache_error: None,
            },
        );
        let output = call(
            &tool,
            serde_json::json!({ "responseId": id, "offset": 0, "limit": 50 }),
        )
        .await
        .expect("ok");
        assert!(output.output.contains("\"claim\""));
        assert_eq!(output.details["type"], "research");
    }

    #[tokio::test]
    async fn unknown_response_id_fails_with_sources_hint() {
        let tool = GetSearchContentTool::new(context());
        let error = call(
            &tool,
            serde_json::json!({ "responseId": "nope123" }),
        )
        .await
        .expect_err("unknown id");
        assert!(error.to_string().contains("No stored results for responseId"));
        assert!(error.to_string().contains("web_search, source_check, or fetch_content"));
    }
}
