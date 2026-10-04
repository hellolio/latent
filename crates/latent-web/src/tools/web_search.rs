//! web_search 工具(index.ts:1832-2430 的移植):
//! 多 query 并发(限 3)→ 每 query 走 router → 组装有界输出 → 存储 +
//! responseId 检索指引;includeContent 后台抓全文,完成后经 notifier
//! 触发新 turn;workflow=auto-summary 时以摘要替代原始结果。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use latent_agent::{Tool, ToolCall, ToolError, ToolOutput, ToolUpdater};

use crate::prompts;
use crate::providers::ProviderContext;
use crate::router::{self, AttributedSearchResponse};
use crate::storage::{self, QueryResultData, StoredSearchData, StoredType};
use crate::config::SearchWorkflow;
use crate::types::{ExtractedContent, SearchOptions};
use crate::tools::{names, parse_search_params, WebContext};
const MAX_CONCURRENT_QUERIES: usize = 3;

pub struct WebSearchTool {
    context: Arc<WebContext>,
}

impl WebSearchTool {
    pub fn new(context: Arc<WebContext>) -> Self {
        WebSearchTool { context }
    }

    fn search_options(&self, params: &crate::tools::SearchParams) -> SearchOptions {
        SearchOptions {
            num_results: params.num_results,
            recency_filter: params.recency_filter,
            domain_filter: params.domain_filter.clone(),
            include_content: params.include_content,
        }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        names::WEB_SEARCH
    }

    fn description(&self) -> &str {
        // description 含运行时插值(provider 列表),走 lazy 静态
        web_search_description()
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(prompts::WEB_SEARCH_PROMPT_SNIPPET.to_string())
    }

    fn schema(&self) -> serde_json::Value {
        use crate::prompts as p;
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": p::WEB_SEARCH_PARAM_QUERY },
                "queries": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": p::WEB_SEARCH_PARAM_QUERIES
                },
                "numResults": { "type": "integer", "minimum": 1, "maximum": 20, "description": p::WEB_SEARCH_PARAM_NUM_RESULTS },
                "includeContent": { "type": "boolean", "description": p::WEB_SEARCH_PARAM_INCLUDE_CONTENT },
                "recencyFilter": { "type": "string", "enum": ["day", "week", "month", "year"], "description": p::WEB_SEARCH_PARAM_RECENCY },
                "domainFilter": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": p::WEB_SEARCH_PARAM_DOMAIN
                },
                "provider": {
                    "anyOf": [
                        { "type": "string", "enum": ["auto", "all", "searxng", "exa", "brave", "tavily", "duckduckgo"] },
                        { "type": "array", "items": { "type": "string", "enum": ["searxng", "exa", "brave", "tavily", "duckduckgo"] }, "minItems": 1 }
                    ],
                    "description": p::web_search_param_provider()
                },
                "proxy": { "type": "string", "description": p::WEB_SEARCH_PARAM_PROXY }
            },
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        call: ToolCall,
        cancel: CancellationToken,
        updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        let params = parse_search_params(&call.args)
            .map_err(|message| ToolError::Failed { name: call.name.clone(), message })?;
        let max_inline = self.context.max_inline_chars();
        let options = self.search_options(&params);
        let proxy = params.proxy.as_deref();
        let provider_ctx = ProviderContext {
            config: &self.context.config,
            cancel: &cancel,
            proxy,
        };

        // 多 query 并发(限 3;buffer_unordered 保持借用、完成即上报进度),
        // 单 query 失败降级为该 query 的 error 字段
        let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_QUERIES));
        let total = params.query_list.len();
        let selection = params.selection.clone();
        let mut results: Vec<QueryResultData> = Vec::with_capacity(total);
        let mut inline_content: Vec<ExtractedContent> = Vec::new();
        let mut completed = 0usize;
        let mut result_stream = futures::stream::iter(params.query_list.clone())
            .map(|query| {
                let semaphore = semaphore.clone();
                let options = &options;
                let selection = &selection;
                let provider_ctx = &provider_ctx;
                async move {
                    let _permit = semaphore.acquire_owned().await;
                    let outcome = router::search(&query, selection, options, provider_ctx).await;
                    (query, outcome)
                }
            })
            .buffer_unordered(MAX_CONCURRENT_QUERIES);
        use futures::StreamExt as _;
        while let Some((query, outcome)) = result_stream.next().await {
            completed += 1;
            updater
                .update(format!(
                    "Searching \"{query}\" ({completed}/{total} complete)..."
                ))
                .await;
            match outcome {
                Ok(attributed) => {
                    inline_content.extend(attributed.response.inline_content.clone());
                    let providers = providers_of(&attributed);
                    results.push(QueryResultData {
                        query,
                        answer: attributed.response.answer,
                        results: attributed.response.results,
                        error: None,
                        provider: Some(attributed.provider.clone()),
                        providers,
                    });
                }
                Err(error) => {
                    results.push(QueryResultData {
                        query,
                        answer: String::new(),
                        results: Vec::new(),
                        error: Some(error.to_string()),
                        provider: None,
                        providers: Vec::new(),
                    });
                }
            }
        }
        if cancel.is_cancelled() {
            return Err(ToolError::Aborted { name: call.name.clone() });
        }

        // 存储(内存)—— queries 全量结果
        let search_id = storage::generate_id();
        storage::store_result(
            &search_id,
            StoredSearchData {
                id: search_id.clone(),
                stored_type: StoredType::Search,
                timestamp: now_ms(),
                queries: Some(results.clone()),
                urls: None,
                artifact: None,
                fetch_cache: None,
                url_metadata: None,
                fetch_cache_error: None,
            },
        );

        // includeContent:全文已随搜索返回 → 直接入库;否则后台抓取
        let mut fetch_id: Option<String> = None;
        let urls: Vec<String> = results
            .iter()
            .filter(|result| result.error.is_none())
            .flat_map(|result| result.results.iter().map(|source| source.url.clone()))
            .collect();
        let covered = has_full_inline_coverage(&urls, &inline_content);
        if covered && !inline_content.is_empty() {
            let id = storage::generate_id();
            storage::store_fetched_content_result(&id, inline_content.clone(), self.context.cache_limits);
            fetch_id = Some(id);
        } else if params.include_content && !urls.is_empty() {
            let id = storage::generate_id();
            spawn_background_fetch(
                self.context.clone(),
                id.clone(),
                urls.clone(),
                params.proxy.clone(),
                self.search_options(&params),
            );
            fetch_id = Some(id);
        }
        let is_background_fetch = fetch_id.is_some() && !covered;

        let successful = results.iter().filter(|result| result.error.is_none()).count();
        let total_results: usize = results.iter().map(|result| result.results.len()).sum();

        // auto-summary(用户在 web-search.json 配置 workflow = "auto-summary"):
        // 摘要替代原始结果;头部标注摘要属性,尾部附 searchId 取回指引。
        // 摘要文本同样走有界管线(LLM 输出可达 ~16k 字符),不超转录预算
        if self.context.config.workflow() == SearchWorkflow::AutoSummary {
            let summary = crate::summary::generate_summary(
                &self.context,
                &results,
                &cancel,
            )
            .await;
            let (text, meta) = summary;
            let header = "Auto-generated summary of the search results (configured via the `workflow` option in web-search.json).\n\n";
            let mut tail = format!(
                "\n\n---\nFull search results are stored as responseId \"{search_id}\". Use {}({{ responseId: \"{search_id}\", queryIndex: 0, offset: 0, limit: {max_inline} }}) to retrieve them.",
                names::GET_SEARCH_CONTENT
            );
            if total > 1 {
                tail.push_str(&format!(
                    " Repeat with queryIndex 1 through {} for the other queries.",
                    total - 1
                ));
            }
            let presentation = crate::bounded::bound_search_presentation(
                &format!("{header}{text}"),
                &tail,
                &tail,
                max_inline,
            );
            return Ok(ToolOutput {
                output: presentation.text,
                details: json!({
                    "queries": params.query_list,
                    "queryCount": total,
                    "successfulQueries": successful,
                    "totalResults": total_results,
                    "includeContent": params.include_content,
                    "fetchId": fetch_id,
                    "searchId": search_id,
                    "truncated": presentation.truncated,
                    "originalChars": presentation.original_chars,
                    "returnedChars": presentation.returned_chars,
                    "summary": meta,
                }),
                terminate: false,
            });
        }

        // 原始结果组装(index.ts buildSearchReturn 非 curated 路径)。
        // 预算按 query 均分:每条 query 都有内容幸存,被截的条目附取回提示,
        // 而非整体截断后在转录中间产生空洞
        let provider_names: Vec<String> = results
            .iter()
            .map(|result| {
                let providers = if !result.providers.is_empty() {
                    result.providers.clone()
                } else {
                    result.provider.clone().into_iter().collect()
                };
                if providers.is_empty() {
                    "unknown".to_string()
                } else {
                    providers.join(", ")
                }
            })
            .collect();
        let per_query_budget = max_inline / total.max(1);
        let mut output = if total == 1 {
            prompts::provider_header_single(&provider_names[0])
        } else {
            prompts::provider_header_multi(&provider_names)
        };
        for result in results.iter() {
            if total > 1 {
                output.push_str(&prompts::query_header(&result.query));
            }
            let section = if let Some(error) = &result.error {
                format!("Error: {error}\n\n")
            } else {
                format!(
                    "{}\n\n",
                    prompts::format_search_summary(&result.results, &result.answer)
                )
            };
            let section_chars = section.chars().count();
            if section_chars <= per_query_budget {
                output.push_str(&section);
            } else {
                output.push_str(&bound_query_section(
                    &section,
                    per_query_budget,
                    &search_id,
                    names::GET_SEARCH_CONTENT,
                ));
            }
        }
        let unbounded = output.trim().to_string();
        let unbounded = with_all_failed_guidance(&unbounded, successful);

        let build_guidance = |for_truncation: bool| -> String {
            let mut value = String::new();
            if let (true, Some(fetch_id)) = (covered, &fetch_id) {
                value.push_str(&format!(
                    "\n---\nFull content for {} sources is ready as responseId \"{fetch_id}\". ",
                    inline_content.len()
                ));
                value.push_str(&format!(
                    "Use {}({{ responseId: \"{fetch_id}\", urlIndex: 0, offset: 0, limit: {max_inline} }}) to retrieve the first bounded page.",
                    names::GET_SEARCH_CONTENT
                ));
            } else if let (true, Some(fetch_id)) = (is_background_fetch, &fetch_id) {
                value.push_str(&format!(
                    "\n---\nContent fetching in background as responseId \"{fetch_id}\". Will notify when ready."
                ));
            }
            value.push_str(&format!(
                "\n---\nFull search results are stored as responseId \"{search_id}\". "
            ));
            value.push_str(&format!(
                "Use {}({{ responseId: \"{search_id}\", queryIndex: 0, offset: 0, limit: {max_inline} }}) to retrieve the first bounded page{}.",
                names::GET_SEARCH_CONTENT,
                if total > 1 {
                    format!("; repeat with queryIndex 1 through {}", total - 1)
                } else {
                    String::new()
                }
            ));
            // 截断场景:模型最需要的是"去哪取全文",指引明确提示已截断
            if for_truncation {
                value.push_str(
                    "\n---\nThe output above was truncated. Use get_search_content with the responseIds above (offset paging or findText) to retrieve the full stored content.",
                );
            }
            value
        };

        let presentation = crate::bounded::bound_search_presentation(
            &unbounded,
            &build_guidance(false),
            &build_guidance(true),
            max_inline,
        );
        Ok(ToolOutput {
            output: presentation.text,
            details: json!({
                "queries": params.query_list,
                "queryCount": total,
                "successfulQueries": successful,
                "totalResults": total_results,
                "includeContent": params.include_content,
                "fetchId": fetch_id,
                "searchId": search_id,
                "queryProviders": results.iter().map(|result| json!({
                    "query": result.query,
                    "providers": if result.providers.is_empty() {
                        serde_json::Value::Null
                    } else {
                        json!(result.providers)
                    },
                })).collect::<Vec<_>>(),
                "truncated": presentation.truncated,
                "originalChars": presentation.original_chars,
                "returnedChars": presentation.returned_chars,
            }),
            terminate: false,
        })
    }
}

fn providers_of(attributed: &AttributedSearchResponse) -> Vec<String> {
    if !attributed.provider_responses.is_empty() {
        attributed
            .provider_responses
            .iter()
            .map(|response| response.provider.clone())
            .collect()
    } else {
        vec![attributed.provider.clone()]
    }
}

/// 字符边界安全的前缀截取(多字节安全)。
fn take_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// 单条 query 超预算时的有界切片:截断 + 取回提示,总长不超 budget;
/// 未超预算时原样返回。
fn bound_query_section(section: &str, budget: usize, search_id: &str, tool_name: &str) -> String {    let section_chars = section.chars().count();
    if section_chars <= budget {
        return section.to_string();
    }
    let note = format!(
        "\n[This query's output was truncated ({section_chars} chars total). Retrieve the full stored results via {tool_name}(responseId \"{search_id}\").]\n\n"
    );
    let keep = budget.saturating_sub(note.chars().count());
    format!("{}{note}", take_chars(section, keep))
}

/// 全部 query 失败时在末尾追加一次配置指引;部分失败不附,避免
/// "No search provider available" 与实际成功的 provider 矛盾。
fn with_all_failed_guidance(output: &str, successful: usize) -> String {
    if successful == 0 {
        format!(
            "{output}\n\n---\n\n{}",
            crate::router::no_provider_guidance()
        )
    } else {
        output.to_string()
    }
}

fn has_full_inline_coverage(urls: &[String], inline_content: &[ExtractedContent]) -> bool {
    if inline_content.is_empty() {
        return false;
    }
    let covered: std::collections::HashSet<&String> =
        inline_content.iter().map(|content| &content.url).collect();
    urls.iter().all(|url| covered.contains(url))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// 后台抓全文(独立取消令牌,不随工具调用取消;完成经 notifier 驱动新 turn)。
fn spawn_background_fetch(
    context: Arc<WebContext>,
    fetch_id: String,
    urls: Vec<String>,
    proxy: Option<String>,
    options: SearchOptions,
) {
    let cancel = CancellationToken::new();
    tokio::spawn(async move {
        let extracted = crate::content::extract::extract_urls(
            &urls,
            crate::content::extract::FetchMode::Readable,
            proxy.as_deref(),
            &context.config,
            &cancel,
        )
        .await;
        storage::store_fetched_content_result(&fetch_id, extracted, context.cache_limits);
        if let Some(notifier) = &context.notifier {
            let max_inline = context.max_inline_chars();
            notifier
                .notify(format!(
                    "Background content fetch completed as responseId \"{fetch_id}\". Use {}({{ responseId: \"{fetch_id}\", urlIndex: 0, offset: 0, limit: {max_inline} }}) to retrieve the first bounded page.",
                    names::GET_SEARCH_CONTENT
                ))
                .await;
        }
        let _ = options;
    });
}

fn web_search_description() -> &'static str {
    use std::sync::LazyLock;
    static DESCRIPTION: LazyLock<String> = LazyLock::new(prompts::web_search_description);
    &DESCRIPTION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_query_section_passes_through() {
        let section = "1. Title\n   https://a.com\n\n";
        let bounded = bound_query_section(section, 500, "search123", "get_search_content");
        assert_eq!(bounded, section);
    }

    #[test]
    fn guidance_appended_only_when_all_queries_failed() {
        let output = "Auto provider search failed:\n  - duckduckgo: HTTP 202";
        let all_failed = with_all_failed_guidance(output, 0);
        assert!(all_failed.contains("No search provider available"));
        assert_eq!(all_failed.matches("No search provider available").count(), 1);

        let partial = with_all_failed_guidance(output, 2);
        assert_eq!(partial, output);
    }

    #[test]
    fn long_query_section_is_bounded_with_retrieval_note() {
        let section = format!("{}\n\n", "x".repeat(1_000));
        let bounded = bound_query_section(&section, 500, "search123", "get_search_content");
        let chars = bounded.chars().count();
        assert!(chars <= 500, "切片 + 提示不应超预算: {chars}");
        assert!(bounded.contains("truncated (1002 chars total)"));
        assert!(bounded.contains("get_search_content(responseId \"search123\")"));
    }

    #[test]
    fn truncation_guidance_differs_from_normal() {
        // for_truncation 分支必须比未截断指引多出"已截断,去取全文"的提示
        let max_inline = 1_000;
        let result = crate::bounded::bound_search_presentation(
            &"x".repeat(2_000),
            "normal guidance",
            "truncated. Use get_search_content to retrieve the full stored content.",
            max_inline,
        );
        assert!(result.truncated);
        assert!(result.text.ends_with(
            "truncated. Use get_search_content to retrieve the full stored content."
        ));
        assert!(!result.text.contains("normal guidance"));
    }
}
