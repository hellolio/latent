//! Brave provider(brave.ts 的移植,GET-JSON 风格代表):
//! domain filter 折进查询串(site:/NOT site:)+ 结果后过滤,30s 超时。

use std::time::Duration;

use async_trait::async_trait;

use crate::config::WebSearchConfig;
use crate::credential::{has_credential_source, redact_credential, resolve_credential};
use crate::error::SearchProviderError;
use crate::http::{send_with_redirects, HttpRequestOptions};
use crate::providers::{resolve_api_base_url, ProviderContext, SearchProvider};
use crate::types::encode_query_param;
use crate::types::{
    fake_answer_from_results, matches_domain_filters, normalize_domain_filters,
    normalize_search_result_count, RecencyFilter, SearchOptions, SearchResponse, SearchResult,
};

const BRAVE_API_BASE_URL: &str = "https://api.search.brave.com/res/v1";
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

pub struct BraveProvider;

fn env_key() -> Option<String> {
    std::env::var("BRAVE_API_KEY").ok()
}

fn api_base_url(config: &WebSearchConfig) -> String {
    resolve_api_base_url(config.brave_base_url.as_deref(), "BRAVE_BASE_URL", BRAVE_API_BASE_URL)
}

/// domain filter 折进查询串(brave 的 buildBraveQuery)。
fn build_brave_query(query: &str, domain_filter: Option<&[String]>) -> String {
    let filters = normalize_domain_filters(domain_filter);
    let mut parts = vec![query.to_string()];
    if filters.allowed.len() == 1 {
        parts.push(format!("site:{}", filters.allowed[0]));
    } else if filters.allowed.len() > 1 {
        parts.push(
            filters
                .allowed
                .iter()
                .map(|domain| format!("site:{domain}"))
                .collect::<Vec<_>>()
                .join(" OR "),
        );
    }
    for domain in &filters.blocked {
        parts.push(format!("NOT site:{domain}"));
    }
    parts.join(" ")
}

#[derive(serde::Deserialize)]
struct BraveResponse {
    #[serde(default)]
    web: Option<BraveWeb>,
}

#[derive(serde::Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveItem>,
}

#[derive(serde::Deserialize)]
struct BraveItem {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[async_trait]
impl SearchProvider for BraveProvider {
    fn id(&self) -> &'static str {
        "brave"
    }

    fn label(&self) -> &'static str {
        "Brave"
    }

    fn is_available(&self, config: &WebSearchConfig) -> bool {
        has_credential_source(config.brave_api_key.as_deref(), env_key().as_deref())
    }

    async fn search(
        &self,
        query: &str,
        options: &SearchOptions,
        ctx: &ProviderContext<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let label = self.label();
        let api_key = resolve_credential(label, ctx.config.brave_api_key.as_deref(), env_key().as_deref())
            .await
            .map_err(|message| SearchProviderError::classify(label, message))?
            .ok_or_else(|| {
                SearchProviderError::classify(
                    label,
                    format!(
                        "{label} Search API key not found. Either:\n  1. Set braveApiKey in web-search.json\n  2. Set BRAVE_API_KEY environment variable\nGet a key at https://brave.com/search/api/"
                    ),
                )
            })?;

        let num_results = normalize_search_result_count(options.num_results);
        let filters = normalize_domain_filters(options.domain_filter.as_deref());
        let search_query = build_brave_query(query, options.domain_filter.as_deref());
        let mut count = num_results.to_string();
        if filters.allowed.len() + filters.blocked.len() > 0 {
            count = "20".to_string();
        }
        let mut request_url = format!(
            "{}/web/search?q={}&count={count}",
            api_base_url(ctx.config),
            encode_query_param(&search_query)
        );
        if let Some(freshness) = recency_freshness(options.recency_filter) {
            request_url.push_str(&format!("&freshness={freshness}"));
        }

        let headers = vec![
            ("X-Subscription-Token".to_string(), api_key.clone()),
            ("Accept".to_string(), "application/json".to_string()),
        ];
        let http_options = HttpRequestOptions {
            proxy: ctx.effective_proxy(),
            timeout: SEARCH_TIMEOUT,
            cancel: Some(ctx.cancel),
            sensitive_headers: &["x-subscription-token"],
        };
        let response = send_with_redirects(
            reqwest::Method::GET,
            &request_url,
            &headers,
            None,
            &http_options,
        )
        .await
        .map_err(|message| {
            SearchProviderError::classify(
                label,
                redact_credential(&message, Some(&api_key)),
            )
        })?;
        if response.status != 200 {
            let body = redact_credential(&response.body, Some(&api_key));
            let message = crate::http::http_error_message(label, response.status, &body);
            return Err(SearchProviderError::classify(label, message));
        }

        let data: BraveResponse = serde_json::from_str(&response.body)
            .map_err(|error| {
                SearchProviderError::classify(label, format!("invalid response JSON: {error}"))
            })?;

        let mut results: Vec<SearchResult> = Vec::new();
        for item in data.web.map(|web| web.results).unwrap_or_default() {
            let Some(url) = item.url else { continue };
            if !matches_domain_filters(&url, &filters) {
                continue;
            }
            results.push(SearchResult {
                title: item.title.unwrap_or_else(|| url.clone()),
                url,
                snippet: item.description.unwrap_or_default(),
            });
            if results.len() >= num_results {
                break;
            }
        }

        let answer = fake_answer_from_results(&results);
        Ok(SearchResponse {
            answer,
            results,
            inline_content: Vec::new(),
        })
    }
}

fn recency_freshness(filter: Option<RecencyFilter>) -> Option<&'static str> {
    match filter {
        Some(RecencyFilter::Day) => Some("pd"),
        Some(RecencyFilter::Week) => Some("pw"),
        Some(RecencyFilter::Month) => Some("pm"),
        Some(RecencyFilter::Year) => Some("py"),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_building_folds_domain_filters() {
        assert_eq!(build_brave_query("rust async", None), "rust async");
        assert_eq!(
            build_brave_query("rust", Some(&["docs.rs".to_string()])),
            "rust site:docs.rs"
        );
        assert_eq!(
            build_brave_query(
                "rust",
                Some(&["docs.rs".to_string(), "-forum.rust-lang.org".to_string()])
            ),
            "rust site:docs.rs NOT site:forum.rust-lang.org"
        );
    }

    #[test]
    fn freshness_mapping() {
        assert_eq!(recency_freshness(Some(RecencyFilter::Day)), Some("pd"));
        assert_eq!(recency_freshness(Some(RecencyFilter::Year)), Some("py"));
        assert_eq!(recency_freshness(None), None);
    }
}
