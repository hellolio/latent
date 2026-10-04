//! source_check 工具(index.ts:2431-2529 的移植):
//! 论断 → 搜索(queries 默认 = claim)→ 可选抓取前 5 页提取 passage →
//! ResearchArtifact 入库 → 有界渲染。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use latent_agent::{Tool, ToolCall, ToolError, ToolOutput, ToolUpdater};

use crate::prompts;
use crate::providers::ProviderContext;
use crate::router;
use crate::source_check as artifact;
use crate::storage::{self, QueryResultData, StoredSearchData, StoredType};
use crate::types::{normalize_search_result_count, SearchOptions};
use crate::tools::{names, parse_proxy_arg, WebContext};

const MAX_FETCHED_PAGES: usize = 5;

pub struct SourceCheckTool {
    context: Arc<WebContext>,
}

impl SourceCheckTool {
    pub fn new(context: Arc<WebContext>) -> Self {
        SourceCheckTool { context }
    }
}

#[async_trait]
impl Tool for SourceCheckTool {
    fn name(&self) -> &str {
        names::SOURCE_CHECK
    }

    fn description(&self) -> &str {
        prompts::SOURCE_CHECK_DESCRIPTION
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(prompts::SOURCE_CHECK_PROMPT_SNIPPET.to_string())
    }

    fn schema(&self) -> serde_json::Value {
        use crate::prompts as p;
        json!({
            "type": "object",
            "required": ["claim"],
            "properties": {
                "claim": { "type": "string", "description": p::SOURCE_CHECK_PARAM_CLAIM },
                "queries": { "type": "array", "items": { "type": "string" }, "description": p::SOURCE_CHECK_PARAM_QUERIES },
                "numResults": { "type": "integer", "minimum": 1, "maximum": 20, "description": p::SOURCE_CHECK_PARAM_NUM_RESULTS },
                "fetchContent": { "type": "boolean", "description": p::SOURCE_CHECK_PARAM_FETCH_CONTENT },
                "recencyFilter": { "type": "string", "enum": ["day", "week", "month", "year"], "description": p::SOURCE_CHECK_PARAM_RECENCY },
                "domainFilter": { "type": "array", "items": { "type": "string" }, "description": p::SOURCE_CHECK_PARAM_DOMAIN },
                "provider": {
                    "anyOf": [
                        { "type": "string", "enum": ["auto", "all", "searxng", "exa", "brave", "tavily", "duckduckgo"] },
                        { "type": "array", "items": { "type": "string", "enum": ["searxng", "exa", "brave", "tavily", "duckduckgo"] }, "minItems": 1 }
                    ],
                    "description": p::source_check_param_provider()
                },
                "proxy": { "type": "string", "description": p::SOURCE_CHECK_PARAM_PROXY }
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
        let args = call.args.as_object().ok_or(ToolError::Failed {
            name: call.name.clone(),
            message: "arguments must be an object".to_string(),
        })?;
        let claim = args
            .get("claim")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(ToolError::Failed {
                name: call.name.clone(),
                message: "missing required argument `claim`".to_string(),
            })?;
        let queries: Vec<String> = match args.get("queries") {
            Some(serde_json::Value::Array(items)) if !items.is_empty() => items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(String::from)
                .collect(),
            _ => vec![claim.to_string()],
        };
        let num_results = normalize_search_result_count(
            args.get("numResults").and_then(|value| value.as_i64()),
        );
        let fetch_content = args
            .get("fetchContent")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let recency = match args.get("recencyFilter").and_then(|value| value.as_str()) {
            Some(value) if !value.is_empty() => Some(
                crate::types::RecencyFilter::parse(value).ok_or(ToolError::Failed {
                    name: call.name.clone(),
                    message: "`recencyFilter` must be one of day/week/month/year".to_string(),
                })?,
            ),
            _ => None,
        };
        let domain_filter: Option<Vec<String>> = match args.get("domainFilter") {
            Some(serde_json::Value::Array(items)) if !items.is_empty() => Some(
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .collect(),
            ),
            _ => None,
        };
        let selection = router::ProviderSelection::from_value(args.get("provider"))
            .map_err(|message| ToolError::Failed { name: call.name.clone(), message })?;
        let proxy = parse_proxy_arg(&call.args).map_err(|message| ToolError::Failed {
            name: call.name.clone(),
            message,
        })?;

        // 搜索(与 web_search 相同管线:并发限 3、错误降级)
        let provider_ctx = ProviderContext {
            config: &self.context.config,
            cancel: &cancel,
            proxy: proxy.as_deref(),
        };
        let options = SearchOptions {
            num_results: Some(num_results as i64),
            recency_filter: recency,
            domain_filter: domain_filter.clone(),
            include_content: false,
        };
        let semaphore = Arc::new(tokio::sync::Semaphore::new(3));
        let total = queries.len();
        let mut per_query: Vec<QueryResultData> = Vec::with_capacity(total);
        let mut errors: Vec<serde_json::Value> = Vec::new();
        let mut stream = futures::stream::iter(queries.clone())
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
            .buffer_unordered(3);
        use futures::StreamExt as _;
        while let Some((query, outcome)) = stream.next().await {
            updater
                .update(format!("Source check searching \"{query}\"..."))
                .await;
            match outcome {
                Ok(attributed) => per_query.push(QueryResultData {
                    query,
                    answer: attributed.response.answer,
                    results: attributed.response.results,
                    error: None,
                    provider: Some(attributed.provider),
                    providers: attributed
                        .provider_responses
                        .iter()
                        .map(|response| response.provider.clone())
                        .collect(),
                }),
                Err(error) => {
                    errors.push(json!({ "query": query, "error": error.to_string() }));
                }
            }
        }
        if cancel.is_cancelled() {
            return Err(ToolError::Aborted { name: call.name.clone() });
        }

        // 合并去重 + 编号
        let mut ranked: Vec<(crate::types::SearchResult, usize)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for query_data in &per_query {
            for result in &query_data.results {
                if seen.insert(result.url.clone()) {
                    ranked.push((result.clone(), ranked.len() + 1));
                }
            }
        }

        // 可选抓取前 5 页提取 passage
        let mut fetched: Vec<crate::types::ExtractedContent> = Vec::new();
        if fetch_content && !ranked.is_empty() {
            updater
                .update("Fetching result pages for passage extraction...".to_string())
                .await;
            let urls: Vec<String> = ranked
                .iter()
                .take(MAX_FETCHED_PAGES)
                .map(|(result, _)| result.url.clone())
                .collect();
            fetched = crate::content::extract::extract_urls(
                &urls,
                crate::content::extract::FetchMode::Readable,
                proxy.as_deref(),
                &self.context.config,
                &cancel,
            )
            .await;
        }

        let summary = prompts::build_deterministic_summary(&per_query);
        let provider = per_query
            .iter()
            .find_map(|query_data| query_data.provider.clone());
        let mut result = artifact::build_research_artifact(artifact::BuildArtifactInput {
            query: claim,
            provider: provider.as_deref(),
            summary: Some(&summary),
            results: &ranked,
            fetched: &fetched,
            recency,
            domain_filter: domain_filter.as_deref(),
        });
        result["errors"] = serde_json::Value::Array(errors.clone());
        // 自动 assessment(保守降级)
        let passages = result.get("passages").cloned().unwrap_or_default();
        let empty_passages: Vec<serde_json::Value> = Vec::new();
        let assessment =
            artifact::assess_claim(claim, passages.as_array().unwrap_or(&empty_passages));
        result["claims"] = json!([assessment]);

        let artifact_id = result
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let timestamp = result
            .get("timestamp")
            .and_then(|value| value.as_u64())
            .unwrap_or_else(now_ms);
        storage::store_result(
            &artifact_id,
            StoredSearchData {
                id: artifact_id.clone(),
                stored_type: StoredType::Research,
                timestamp,
                queries: None,
                urls: None,
                artifact: Some(result.clone()),
                fetch_cache: None,
                url_metadata: None,
                fetch_cache_error: None,
            },
        );

        // 全部 query 失败:追加一次配置指引(部分失败时不附)
        let mut output = prompts::format_source_check_result(&result);
        if total > 0 && errors.len() == total {
            output.push_str("\n\n---\n");
            output.push_str(&router::no_provider_guidance());
        }

        Ok(ToolOutput {
            output,
            details: json!({
                "responseId": artifact_id,
                "claim": claim,
                "queryCount": total,
                "sourceCount": ranked.len(),
                "passageCount": result.get("passages").and_then(|value| value.as_array()).map(Vec::len).unwrap_or(0),
                "errors": errors,
            }),
            terminate: false,
        })
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
