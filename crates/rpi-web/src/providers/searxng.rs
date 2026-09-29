//! SearXNG provider(自托管;searxng.ts 的移植):`{base}/search?format=json`,
//! 支持自定义 headers(校验 RFC 7230 token 名),30s 超时。

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::config::WebSearchConfig;
use crate::error::SearchProviderError;
use crate::http::{send_with_redirects, HttpRequestOptions};
use crate::providers::{ProviderContext, SearchProvider};
use crate::types::encode_query_param;
use crate::types::{
    fake_answer_source_line, matches_domain_filters, normalize_domain_filters,
    normalize_search_result_count, SearchOptions, SearchResponse, SearchResult,
};

const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

pub struct SearXngProvider;

/// base URL 归一化:仅 http(s)、拒绝 userinfo、去尾斜杠与 query/hash。
fn normalize_base_url(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed = url::Url::parse(trimmed).ok()?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return None;
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    let mut base = format!("{}://{}", parsed.scheme(), parsed.host_str()?);
    if let Some(port) = parsed.port() {
        base.push_str(&format!(":{port}"));
    }
    Some(base)
}

fn base_url(config: &WebSearchConfig) -> Option<String> {
    if let Ok(env_value) = std::env::var("SEARXNG_BASE_URL") {
        if let Some(base) = normalize_base_url(&env_value) {
            return Some(base);
        }
        return None;
    }
    config.searxng_base_url.as_deref().and_then(normalize_base_url)
}

/// header 名必须是 RFC 7230 token,值不得含控制字符(searxng.ts 的校验)。
fn normalize_headers(
    value: Option<&[(String, String)]>,
) -> Vec<(String, String)> {
    static TOKEN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^[!#$%&'*+.^_`|~0-9A-Za-z-]+$").expect("static regex")
    });
    let Some(entries) = value else {
        return Vec::new();
    };
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, header_value) in entries {
        let name = name.trim();
        if name.is_empty() || !TOKEN.is_match(name) {
            continue;
        }
        if header_value.is_empty() || header_value.chars().any(char::is_control) {
            continue;
        }
        // 大小写不敏感覆盖默认头
        if let Some(existing) = headers
            .iter_mut()
            .find(|(existing_name, _)| existing_name.eq_ignore_ascii_case(name))
        {
            existing.1 = header_value.clone();
        } else {
            headers.push((name.to_string(), header_value.clone()));
        }
    }
    headers
}

#[derive(Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngItem>,
    #[serde(default)]
    answers: Vec<String>,
}

#[derive(Deserialize)]
struct SearxngItem {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

#[async_trait]
impl SearchProvider for SearXngProvider {
    fn id(&self) -> &'static str {
        "searxng"
    }

    fn label(&self) -> &'static str {
        "SearXNG"
    }

    fn is_available(&self, config: &WebSearchConfig) -> bool {
        base_url(config).is_some()
    }

    async fn search(
        &self,
        query: &str,
        options: &SearchOptions,
        ctx: &ProviderContext<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let label = self.label();
        let Some(base) = base_url(ctx.config) else {
            return Err(SearchProviderError::classify(
                label,
                format!(
                    "{label} base URL is invalid or missing. Either:\n  1. Set searxngBaseUrl in web-search.json\n  2. Set SEARXNG_BASE_URL to an HTTP(S) URL"
                ),
            ));
        };
        let filters = normalize_domain_filters(options.domain_filter.as_deref());
        // searxng 的 buildSearXNGQuery:site:/OR/-site: 折进查询串
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
            parts.push(format!("-site:{domain}"));
        }
        let search_query = parts.join(" ");

        let mut request_url = format!("{base}/search?q={}&format=json", encode_query_param(&search_query));
        if let Some(recency) = options.recency_filter {
            request_url.push_str(&format!("&time_range={}", recency.as_str()));
        }

        let mut headers = vec![("Accept".to_string(), "application/json".to_string())];
        for (name, value) in normalize_headers(ctx.config.searxng_headers.as_deref()) {
            if let Some(existing) = headers
                .iter_mut()
                .find(|(existing_name, _)| existing_name.eq_ignore_ascii_case(&name))
            {
                existing.1 = value;
            } else {
                headers.push((name, value));
            }
        }
        let http_options = HttpRequestOptions {
            proxy: ctx.effective_proxy(),
            timeout: SEARCH_TIMEOUT,
            cancel: Some(ctx.cancel),
            sensitive_headers: &[],
        };
        let response = send_with_redirects(
            reqwest::Method::GET,
            &request_url,
            &headers,
            None,
            &http_options,
        )
        .await
        .map_err(|message| SearchProviderError::classify(label, message))?;
        if response.status != 200 {
            let message = crate::http::http_error_message(label, response.status, &response.body);
            return Err(SearchProviderError::classify(label, message));
        }
        let data: SearxngResponse = serde_json::from_str(&response.body).map_err(|error| {
            SearchProviderError::classify(
                label,
                format!("SearXNG returned invalid JSON: {error}"),
            )
        })?;

        let num_results = normalize_search_result_count(options.num_results);
        let mut results: Vec<SearchResult> = Vec::new();
        for item in data.results {
            let Some(url) = item.url else { continue };
            if !matches_domain_filters(&url, &filters) {
                continue;
            }
            results.push(SearchResult {
                title: item.title.unwrap_or_else(|| url.clone()),
                url,
                snippet: item.content.unwrap_or_default(),
            });
            if results.len() >= num_results {
                break;
            }
        }

        let mut answer_parts: Vec<String> = data
            .answers
            .into_iter()
            .map(|answer| answer.trim().to_string())
            .filter(|answer| !answer.is_empty())
            .collect();
        for result in &results {
            answer_parts.push(fake_answer_source_line(result));
        }
        Ok(SearchResponse {
            answer: answer_parts.join("\n\n"),
            results,
            inline_content: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_normalization() {
        assert_eq!(
            normalize_base_url("https://search.example.com/"),
            Some("https://search.example.com".into())
        );
        assert_eq!(
            normalize_base_url("http://192.168.1.10:8888/searxng/"),
            Some("http://192.168.1.10:8888".into())
        );
        assert_eq!(normalize_base_url("ftp://x"), None);
        assert_eq!(normalize_base_url("https://a:b@x"), None);
        assert_eq!(normalize_base_url(""), None);
    }

    #[test]
    fn header_validation() {
        let headers = normalize_headers(Some(&[
            ("X-API-KEY".to_string(), "v".to_string()),
            ("bad name".to_string(), "v".to_string()),
            ("accept".to_string(), "application/xml".to_string()),
            ("Empty".to_string(), String::new()),
        ]));
        assert_eq!(headers.len(), 2);
        let accept = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("accept"))
            .expect("accept kept");
        assert_eq!(accept.1, "application/xml");
        assert!(headers.iter().any(|(name, _)| name == "X-API-KEY"));
    }
}
