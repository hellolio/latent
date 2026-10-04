//! DuckDuckGo provider(零配置兜底;duckduckgo.ts 的移植):
//! GET html.duckduckgo.com/html/,HTML 解析 `.result` 节点,30s 超时。

use std::time::Duration;

use async_trait::async_trait;
use scraper::{ElementRef, Selector};

use crate::config::WebSearchConfig;
use crate::error::SearchProviderError;
use crate::http::{send_with_redirects, HttpRequestOptions};
use crate::providers::{ProviderContext, SearchProvider};
use crate::types::encode_query_param;
use crate::types::{
    matches_domain_filters, normalize_domain_filters, normalize_search_result_count, SearchOptions,
    SearchResponse, SearchResult,
};

const SEARCH_URL: &str = "https://html.duckduckgo.com/html/";
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

pub struct DuckDuckGoProvider;

#[async_trait]
impl SearchProvider for DuckDuckGoProvider {
    fn id(&self) -> &'static str {
        "duckduckgo"
    }

    fn label(&self) -> &'static str {
        "DuckDuckGo"
    }

    fn is_available(&self, _config: &WebSearchConfig) -> bool {
        true
    }

    async fn search(
        &self,
        query: &str,
        options: &SearchOptions,
        ctx: &ProviderContext<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let label = self.label();
        let request_url = format!("{SEARCH_URL}?q={}", encode_query_param(query));
        let headers = vec![
            ("Accept".to_string(), "text/html".to_string()),
        ];
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

        let filters = normalize_domain_filters(options.domain_filter.as_deref());
        let num_results = normalize_search_result_count(options.num_results);
        let document = scraper::Html::parse_document(&response.body);

        let result_selector = Selector::parse(".result").expect("static selector");
        let ad_selector = Selector::parse(".result--ad").expect("static selector");
        let anchor_selector = Selector::parse(".result__a").expect("static selector");
        let snippet_selector = Selector::parse(".result__snippet").expect("static selector");

        let mut results: Vec<SearchResult> = Vec::new();
        let mut parseable_results = 0usize;
        for container in document.select(&result_selector) {
            let is_ad = container.value().has_class("result--ad", scraper::CaseSensitivity::CaseSensitive)
                || container.select(&ad_selector).next().is_some();
            if is_ad {
                continue;
            }
            let Some(anchor) = container.select(&anchor_selector).next() else {
                continue;
            };
            let title = text_of(anchor);
            let href = anchor
                .value()
                .attr("href")
                .unwrap_or_default()
                .trim()
                .to_string();
            let Some(result_url) = decode_result_url(&href) else {
                continue;
            };
            if title.is_empty() {
                continue;
            }
            parseable_results += 1;
            if !matches_domain_filters(&result_url, &filters) {
                continue;
            }
            let snippet = container
                .select(&snippet_selector)
                .next()
                .map(text_of)
                .unwrap_or_default();
            results.push(SearchResult {
                title,
                url: result_url,
                snippet,
            });
            if results.len() >= num_results {
                break;
            }
        }
        if parseable_results == 0 {
            return Err(SearchProviderError::classify(
                label,
                "DuckDuckGo returned no parseable results (invalid response)",
            ));
        }

        // 伪造 answer(与上游 search-answer-formatting 一致)
        let answer = results
            .iter()
            .map(|result| {
                if result.snippet.is_empty() {
                    format!("Source: {} ({})", result.title, result.url)
                } else {
                    format!(
                        "{}\nSource: {} ({})",
                        result.snippet, result.title, result.url
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok(SearchResponse {
            answer,
            results,
            inline_content: Vec::new(),
        })
    }
}

fn text_of(element: ElementRef<'_>) -> String {
    element.text().collect::<String>().trim().to_string()
}

/// 解析 DDG 重定向链接(`//duckduckgo.com/l/?uddg=<encoded>`),非重定向原样返回。
fn decode_result_url(href: &str) -> Option<String> {
    let candidate = if let Some(rest) = href.strip_prefix("//") {
        format!("https://{rest}")
    } else if href.starts_with('/') {
        format!("{SEARCH_URL}{}", href)
    } else {
        href.to_string()
    };
    let parsed = url::Url::parse(&candidate).ok()?;
    let destination = parsed
        .query_pairs()
        .find(|(name, _)| name == "uddg")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| parsed.to_string());
    let parsed = url::Url::parse(&destination).ok()?;
    if parsed.scheme() == "http" || parsed.scheme() == "https" {
        Some(parsed.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_redirect_links() {
        assert_eq!(
            decode_result_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa&rut=xyz")
                .unwrap(),
            "https://example.com/a"
        );
        assert_eq!(
            decode_result_url("https://direct.example.com/").unwrap(),
            "https://direct.example.com/"
        );
        assert!(decode_result_url("javascript:void(0)").is_none());
    }

    #[test]
    fn parses_result_html() {
        let html = r#"
        <div class="result result--ad"><a class="result__a" href="/l/?uddg=https%3A%2F%2Fad.example">Ad</a></div>
        <div class="result">
          <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fone">First</a>
          <a class="result__snippet">Snippet one</a>
        </div>
        <div class="result">
          <a class="result__a" href="https://example.com/two">Second</a>
          <a class="result__snippet">Snippet two</a>
        </div>"#;
        let document = scraper::Html::parse_document(html);
        let result_selector = Selector::parse(".result").unwrap();
        let anchor_selector = Selector::parse(".result__a").unwrap();
        let ad_selector = Selector::parse(".result--ad").unwrap();
        let mut urls = Vec::new();
        for container in document.select(&result_selector) {
            let is_ad = container.value().has_class("result--ad", scraper::CaseSensitivity::CaseSensitive)
                || container.select(&ad_selector).next().is_some();
            if is_ad {
                continue;
            }
            if let Some(anchor) = container.select(&anchor_selector).next() {
                if let Some(url) = decode_result_url(anchor.value().attr("href").unwrap_or("")) {
                    urls.push(url);
                }
            }
        }
        assert_eq!(urls, vec!["https://example.com/one", "https://example.com/two"]);
    }
}
