//! Exa provider(exa.ts 的 key 通道移植):带 key 时,无过滤参数走
//! `/answer`,否则走 `/search`(highlights/text 组装 answer);上游的
//! 零配置 Exa MCP 通道在 rpi 中省略(取舍已确认),无 key 即不可用。

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::config::WebSearchConfig;
use crate::credential::{has_credential_source, redact_credential, resolve_credential};
use crate::error::SearchProviderError;
use crate::http::{send_with_redirects, HttpRequestOptions};
use crate::providers::{resolve_api_base_url, ProviderContext, SearchProvider};
use crate::types::{
    normalize_search_result_count, ExtractedContent, SearchOptions, SearchResponse, SearchResult,
};

const EXA_API_BASE_URL: &str = "https://api.exa.ai";
const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);

pub struct ExaProvider;

fn env_key() -> Option<String> {
    std::env::var("EXA_API_KEY").ok()
}

fn api_base_url(config: &WebSearchConfig) -> String {
    resolve_api_base_url(config.exa_base_url.as_deref(), "EXA_BASE_URL", EXA_API_BASE_URL)
}

#[derive(Deserialize)]
struct ExaAnswerResponse {
    #[serde(default)]
    answer: Option<String>,
    #[serde(default)]
    citations: Vec<ExaItem>,
}

#[derive(Deserialize, Default)]
struct ExaSearchResponse {
    #[serde(default)]
    results: Vec<ExaItem>,
}

#[derive(Deserialize, Default)]
struct ExaItem {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    highlights: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct ExaRequestShape {
    search: bool,
}

fn request_shape(options: &SearchOptions, num_results: usize) -> ExaRequestShape {
    // 上游 useSearch:includeContent / recency / domain / 非默认 numResults
    // 任一存在 → /search,否则 /answer
    let search = options.include_content
        || options.recency_filter.is_some()
        || options.domain_filter.as_ref().is_some_and(|f| !f.is_empty())
        || num_results != 5;
    ExaRequestShape { search }
}

fn recency_start_date(filter: crate::types::RecencyFilter) -> String {
    let days = match filter {
        crate::types::RecencyFilter::Day => 1,
        crate::types::RecencyFilter::Week => 7,
        crate::types::RecencyFilter::Month => 30,
        crate::types::RecencyFilter::Year => 365,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let start = now.as_secs().saturating_sub(days as u64 * 86_400);
    // ISO 8601(UTC)
    let (year, month, day) = epoch_to_utc_date(start);
    format!("{year:04}-{month:02}-{day:02}T00:00:00.000Z")
}

fn epoch_to_utc_date(epoch_secs: u64) -> (i64, u32, u32) {
    // civil-from-days(Howard Hinnant 算法)
    let days = (epoch_secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// highlights 优先,退化为 text 前 1000 字符(exa.ts buildAnswerFromSearchResults)。
fn build_answer_from_results(results: &[ExaItem]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (index, item) in results.iter().enumerate() {
        let Some(url) = item.url.as_ref() else {
            continue;
        };
        let highlights: Vec<&str> = item
            .highlights
            .iter()
            .map(|highlight| highlight.trim())
            .filter(|highlight| !highlight.is_empty())
            .collect();
        let content = if !highlights.is_empty() {
            highlights.join(" ")
        } else {
            item.text.as_deref().unwrap_or_default().trim().chars().take(1000).collect()
        };
        if content.is_empty() {
            continue;
        }
        let title = item
            .title
            .clone()
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| format!("Source {}", index + 1));
        parts.push(format!("{content}\nSource: {title} ({url})"));
    }
    parts.join("\n\n")
}

fn map_results(results: &[ExaItem]) -> Vec<SearchResult> {
    let mut mapped: Vec<SearchResult> = Vec::new();
    for item in results {
        let Some(url) = item.url.as_ref() else {
            continue;
        };
        mapped.push(SearchResult {
            title: item
                .title
                .clone()
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| format!("Source {}", mapped.len() + 1)),
            url: url.clone(),
            snippet: String::new(),
        });
    }
    mapped
}

fn map_inline_content(results: &[ExaItem]) -> Vec<ExtractedContent> {
    results
        .iter()
        .filter_map(|item| {
            let url = item.url.as_ref()?;
            let text = item.text.as_deref().filter(|text| !text.is_empty())?;
            Some(ExtractedContent {
                url: url.clone(),
                title: item.title.clone().unwrap_or_default(),
                content: text.to_string(),
                error: None,
                mime_type: None,
                status: None,
                duration: None,
            })
        })
        .collect()
}

#[async_trait]
impl SearchProvider for ExaProvider {
    fn id(&self) -> &'static str {
        "exa"
    }

    fn label(&self) -> &'static str {
        "Exa"
    }

    fn is_available(&self, config: &WebSearchConfig) -> bool {
        has_credential_source(config.exa_api_key.as_deref(), env_key().as_deref())
    }

    async fn search(
        &self,
        query: &str,
        options: &SearchOptions,
        ctx: &ProviderContext<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let label = self.label();
        let api_key = resolve_credential(label, ctx.config.exa_api_key.as_deref(), env_key().as_deref())
            .await
            .map_err(|message| SearchProviderError::classify(label, message))?
            .ok_or_else(|| {
                SearchProviderError::classify(
                    label,
                    format!(
                        "{label} API key not found. Either:\n  1. Set exaApiKey in web-search.json\n  2. Set EXA_API_KEY environment variable\nGet a key at https://exa.ai/"
                    ),
                )
            })?;

        let num_results = normalize_search_result_count(options.num_results);
        let shape = request_shape(options, num_results);
        let base = api_base_url(ctx.config);
        let headers = vec![
            ("x-api-key".to_string(), api_key.clone()),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        let http_options = HttpRequestOptions {
            proxy: ctx.effective_proxy(),
            timeout: SEARCH_TIMEOUT,
            cancel: Some(ctx.cancel),
            sensitive_headers: &["x-api-key"],
        };

        if !shape.search {
            let body = serde_json::json!({ "query": query });
            let response = send_with_redirects(
                reqwest::Method::POST,
                &format!("{base}/answer"),
                &headers,
                Some(body.to_string().into_bytes()),
                &http_options,
            )
            .await
            .map_err(|message| {
                SearchProviderError::classify(label, redact_credential(&message, Some(&api_key)))
            })?;
            if response.status != 200 {
                let body_text = redact_credential(&response.body, Some(&api_key));
                let message =
                    format!("Exa API error {}: {}", response.status, truncate(&body_text));
                return Err(SearchProviderError::classify(label, message));
            }
            let data: ExaAnswerResponse = serde_json::from_str(&response.body).map_err(|error| {
                SearchProviderError::classify(
                    label,
                    format!("Exa API returned invalid JSON: {error}"),
                )
            })?;
            return Ok(SearchResponse {
                answer: data.answer.unwrap_or_default(),
                results: map_results(&data.citations),
                inline_content: Vec::new(),
            });
        }

        let mut body = serde_json::json!({
            "query": query,
            "type": "auto",
            "numResults": num_results,
            "contents": if options.include_content {
                serde_json::json!({ "text": true, "highlights": true })
            } else {
                serde_json::json!({ "highlights": true })
            },
        });
        let filters = options.domain_filter.as_deref().unwrap_or_default();
        let include: Vec<String> = filters
            .iter()
            .map(|domain| domain.trim())
            .filter(|domain| !domain.is_empty() && !domain.starts_with('-'))
            .map(|domain| domain.to_string())
            .collect();
        let exclude: Vec<String> = filters
            .iter()
            .filter(|domain| domain.trim().starts_with('-'))
            .map(|domain| domain.trim()[1..].trim().to_string())
            .filter(|domain| !domain.is_empty())
            .collect();
        if !include.is_empty() {
            body["includeDomains"] = serde_json::json!(include);
        }
        if !exclude.is_empty() {
            body["excludeDomains"] = serde_json::json!(exclude);
        }
        if let Some(recency) = options.recency_filter {
            body["startPublishedDate"] = serde_json::json!(recency_start_date(recency));
        }

        let response = send_with_redirects(
            reqwest::Method::POST,
            &format!("{base}/search"),
            &headers,
            Some(body.to_string().into_bytes()),
            &http_options,
        )
        .await
        .map_err(|message| {
            SearchProviderError::classify(label, redact_credential(&message, Some(&api_key)))
        })?;
        if response.status != 200 {
            let body_text = redact_credential(&response.body, Some(&api_key));
            let message = format!("Exa API error {}: {}", response.status, truncate(&body_text));
            return Err(SearchProviderError::classify(label, message));
        }
        let data: ExaSearchResponse = serde_json::from_str(&response.body).map_err(|error| {
            SearchProviderError::classify(
                label,
                format!("Exa API returned invalid JSON: {error}"),
            )
        })?;
        Ok(SearchResponse {
            answer: build_answer_from_results(&data.results),
            results: map_results(&data.results),
            inline_content: if options.include_content {
                map_inline_content(&data.results)
            } else {
                Vec::new()
            },
        })
    }
}

fn truncate(body: &str) -> String {
    let mut snippet: String = body.chars().take(300).collect();
    if body.chars().count() > 300 {
        snippet.push('…');
        snippet.push_str("...");
    }
    snippet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_selection_matches_upstream() {
        let plain = SearchOptions::default();
        assert!(!request_shape(&plain, 5).search);
        assert!(request_shape(&plain, 10).search);
        assert!(request_shape(
            &SearchOptions {
                include_content: true,
                ..Default::default()
            },
            5
        )
        .search);
    }

    #[test]
    fn date_conversion() {
        // 2026-09-29 00:00:00 UTC = 1790640000;2026-09-30 = 1790726400
        assert_eq!(epoch_to_utc_date(1_790_640_000), (2026, 9, 29));
        assert_eq!(epoch_to_utc_date(1_790_726_400), (2026, 9, 30));
        assert_eq!(epoch_to_utc_date(0), (1970, 1, 1));
    }

    #[test]
    fn answer_prefers_highlights() {
        let results = vec![ExaItem {
            title: Some("T".into()),
            url: Some("https://a".into()),
            text: Some("full text".into()),
            highlights: vec!["highlight".into()],
        }];
        assert_eq!(
            build_answer_from_results(&results),
            "highlight\nSource: T (https://a)"
        );
    }
}
