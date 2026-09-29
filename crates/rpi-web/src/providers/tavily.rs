//! Tavily provider(tavily.ts 的移植,POST-JSON 风格代表):
//! Bearer 认证、`TAVILY_API_KEY_1..20` key-pool failover(401/402/403/429/432
//! 换下一把)、include_raw_content 支持,60s 超时。

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::config::WebSearchConfig;
use crate::credential::{has_credential_source, redact_credential, resolve_credential};
use crate::error::SearchProviderError;
use crate::http::{send_with_redirects, HttpRequestOptions};
use crate::providers::{resolve_api_base_url, ProviderContext, SearchProvider};
use crate::types::{
    normalize_domain_filters, normalize_search_result_count, ExtractedContent, SearchOptions,
    SearchResponse, SearchResult,
};

const TAVILY_API_BASE_URL: &str = "https://api.tavily.com";
const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);
/// 触发换下一把 key 的状态码(上游 TAVILY_KEY_POOL_RETRIES)。
const KEY_POOL_RETRIES: [u16; 5] = [401, 402, 403, 429, 432];

pub struct TavilyProvider;

fn env_key() -> Option<String> {
    std::env::var("TAVILY_API_KEY").ok()
}

fn api_base_url(config: &WebSearchConfig) -> String {
    resolve_api_base_url(config.tavily_base_url.as_deref(), "TAVILY_BASE_URL", TAVILY_API_BASE_URL)
}

/// key-pool:TAVILY_API_KEY_1..20(可由 TAVILY_API_KEY_INDEX 指定起始槽位)。
fn tavily_key_pool() -> Vec<String> {
    let mut slots: Vec<(u32, String)> = Vec::new();
    for slot in 1..=20u32 {
        if let Ok(key) = std::env::var(format!("TAVILY_API_KEY_{slot}")) {
            let key = key.trim().to_string();
            if !key.is_empty() {
                slots.push((slot, key));
            }
        }
    }
    let requested_slot: u32 = std::env::var("TAVILY_API_KEY_INDEX")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|slot| *slot > 0)
        .unwrap_or(1);
    let start = slots
        .iter()
        .position(|(slot, _)| *slot >= requested_slot)
        .unwrap_or(0);
    let rotated: Vec<String> = slots[start..]
        .iter()
        .chain(slots[..start].iter())
        .map(|(_, key)| key.clone())
        .collect();
    let mut unique: Vec<String> = Vec::new();
    for key in rotated {
        if !unique.contains(&key) {
            unique.push(key);
        }
    }
    unique
}

#[derive(Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    answer: Option<String>,
    #[serde(default)]
    results: Vec<TavilyItem>,
}

#[derive(Deserialize)]
struct TavilyItem {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    raw_content: Option<String>,
}

#[async_trait]
impl SearchProvider for TavilyProvider {
    fn id(&self) -> &'static str {
        "tavily"
    }

    fn label(&self) -> &'static str {
        "Tavily"
    }

    fn is_available(&self, config: &WebSearchConfig) -> bool {
        has_credential_source(config.tavily_api_key.as_deref(), env_key().as_deref())
            || !tavily_key_pool().is_empty()
    }

    async fn search(
        &self,
        query: &str,
        options: &SearchOptions,
        ctx: &ProviderContext<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let num_results = normalize_search_result_count(options.num_results);
        let filters = normalize_domain_filters(options.domain_filter.as_deref());

        let mut body = serde_json::json!({
            "query": query,
            "search_depth": "basic",
            "max_results": num_results,
            "include_answer": "basic",
            "include_raw_content": serde_json::Value::String(if options.include_content { "markdown".to_string() } else { "false".to_string() }),
        });
        if let Some(recency) = options.recency_filter {
            body["time_range"] = serde_json::Value::String(recency.as_str().to_string());
        }
        if !filters.allowed.is_empty() {
            body["include_domains"] = serde_json::json!(filters.allowed);
        }
        if !filters.blocked.is_empty() {
            body["exclude_domains"] = serde_json::json!(filters.blocked);
        }

        let data = self.request_with_key_failover(&api_base_url(ctx.config), body, ctx).await?;

        let mut results: Vec<SearchResult> = Vec::new();
        let mut inline_content: Vec<ExtractedContent> = Vec::new();
        for item in data.results {
            let Some(url) = item.url else { continue };
            let snippet = item
                .content
                .as_deref()
                .map(collapse_whitespace)
                .unwrap_or_default();
            let title = item
                .title
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| format!("Source {}", results.len() + 1));
            if options.include_content {
                if let Some(raw) = item.raw_content.filter(|raw| !raw.trim().is_empty()) {
                    inline_content.push(ExtractedContent {
                        url: url.clone(),
                        title: title.clone(),
                        content: raw,
                        error: None,
                        mime_type: None,
                        status: None,
                        duration: None,
                    });
                }
            }
            results.push(SearchResult {
                title,
                url,
                snippet,
            });
            if results.len() >= num_results {
                break;
            }
        }

        Ok(SearchResponse {
            answer: data.answer.unwrap_or_default(),
            results,
            inline_content,
        })
    }
}

impl TavilyProvider {
    async fn request_with_key_failover(
        &self,
        api_url: &str,
        body: serde_json::Value,
        ctx: &ProviderContext<'_>,
    ) -> Result<TavilyResponse, SearchProviderError> {
        let label = self.label();
        let pool = tavily_key_pool();
        let mut keys = pool.clone();
        let mut fallback_checked = pool.is_empty();
        if keys.is_empty() {
            let key = resolve_credential(
                label,
                ctx.config.tavily_api_key.as_deref(),
                env_key().as_deref(),
            )
            .await
            .map_err(|message| SearchProviderError::classify(label, message))?
            .ok_or_else(|| {
                SearchProviderError::classify(
                    label,
                    format!(
                        "{label} API key not found. Either:\n  1. Set tavilyApiKey in web-search.json\n  2. Set TAVILY_API_KEY environment variable\nGet a key at https://app.tavily.com/"
                    ),
                )
            })?;
            keys.push(key);
        }

        let mut index = 0usize;
        loop {
            let key = keys[index].clone();
            match self.tavily_request(api_url, &key, &body, ctx).await {
                Ok(data) => return Ok(data),
                Err(error) => {
                    let status_matches = error.status.is_some_and(|status| KEY_POOL_RETRIES.contains(&status));
                    if !status_matches {
                        return Err(error);
                    }
                    if index + 1 < keys.len() {
                        index += 1;
                        continue;
                    }
                    if fallback_checked {
                        return Err(error);
                    }
                    // 全部编号 key 失败后,才解析独立凭据(tavily.ts 语义)
                    fallback_checked = true;
                    let fallback = resolve_credential(
                        label,
                        ctx.config.tavily_api_key.as_deref(),
                        env_key().as_deref(),
                    )
                    .await
                    .map_err(|message| SearchProviderError::classify(label, message))?;
                    match fallback {
                        Some(fallback) if !keys.contains(&fallback) => keys.push(fallback),
                        _ => return Err(error),
                    }
                }
            }
        }
    }

    async fn tavily_request(
        &self,
        api_url: &str,
        api_key: &str,
        body: &serde_json::Value,
        ctx: &ProviderContext<'_>,
    ) -> Result<TavilyResponse, SearchProviderError> {
        let label = self.label();
        let headers = vec![
            ("Authorization".to_string(), format!("Bearer {api_key}")),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        let http_options = HttpRequestOptions {
            proxy: ctx.effective_proxy(),
            timeout: SEARCH_TIMEOUT,
            cancel: Some(ctx.cancel),
            sensitive_headers: &["authorization"],
        };
        let response = send_with_redirects(
            reqwest::Method::POST,
            api_url,
            &headers,
            Some(body.to_string().into_bytes()),
            &http_options,
        )
        .await
        .map_err(|message| {
            SearchProviderError::classify(label, redact_credential(&message, Some(api_key)))
        })?;
        if response.status != 200 {
            let body_text = redact_credential(&response.body, Some(api_key));
            let message = format!("Tavily API error {}: {}", response.status, truncate_body(&body_text));
            return Err(SearchProviderError::classify(label, message));
        }
        serde_json::from_str(&response.body).map_err(|error| {
            SearchProviderError::classify(
                label,
                format!("Tavily API returned invalid JSON: {error}"),
            )
        })
    }
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_body(body: &str) -> String {
    let mut snippet: String = body.chars().take(300).collect();
    if body.chars().count() > 300 {
        snippet.push_str("...");
    }
    snippet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_pool_rotation_respects_index() {
        unsafe {
            std::env::remove_var("TAVILY_API_KEY_INDEX");
            std::env::set_var("TAVILY_API_KEY_2", "k2");
            std::env::set_var("TAVILY_API_KEY_5", "k5");
        }
        assert_eq!(tavily_key_pool(), vec!["k2", "k5"]);
        unsafe { std::env::set_var("TAVILY_API_KEY_INDEX", "5") };
        assert_eq!(tavily_key_pool(), vec!["k5", "k2"]);
        unsafe {
            std::env::remove_var("TAVILY_API_KEY_2");
            std::env::remove_var("TAVILY_API_KEY_5");
            std::env::remove_var("TAVILY_API_KEY_INDEX");
        }
    }

    #[test]
    fn whitespace_collapses() {
        assert_eq!(collapse_whitespace("a\n  b\tc"), "a b c");
    }
}
