//! fetch 管线(extract.ts 的 HTML 子集移植):
//! readable(正文提取 → markdown)/ raw(原文)两模式;SSRF 首跳校验 +
//! 重定向每一跳重新校验(手动跟重定向),并发限 3。
//! 上游的图片缩放/PDF/GitHub/YouTube/本地视频通道不做(已确认取舍)。

use std::time::Duration;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::config::WebSearchConfig;
use crate::content::ssrf;
use crate::http::{send_single_hop, HttpRequestOptions};
use crate::types::ExtractedContent;

const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];
const MAX_CONCURRENT_FETCHES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMode {
    Readable,
    Raw,
}

impl FetchMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Readable => "readable",
            Self::Raw => "raw",
        }
    }
}

/// 多 URL 并发抓取(单 URL 失败降级为该条目的 error 字段)。
pub async fn extract_urls(
    urls: &[String],
    mode: FetchMode,
    proxy: Option<&str>,
    config: &WebSearchConfig,
    cancel: &CancellationToken,
) -> Vec<ExtractedContent> {
    let owned_urls: Vec<String> = urls.to_vec();
    let futures = owned_urls.into_iter().map(|url| {
        async move {
            match extract_single(&url, mode, proxy, config, cancel).await {
                Ok(extracted) => extracted,
                Err((message, status, duration)) => ExtractedContent {
                    url,
                    title: String::new(),
                    content: String::new(),
                    error: Some(message),
                    mime_type: None,
                    status,
                    duration: duration.map(|duration| duration.as_millis() as u64),
                },
            }
        }
    });
    use futures::StreamExt as _;
    futures::stream::iter(futures)
        .buffer_unordered(MAX_CONCURRENT_FETCHES)
        .collect::<Vec<_>>()
        .await
}

/// 单 URL:每跳 SSRF 校验的手动重定向循环(extract.ts fetchRemoteUrl 语义)。
async fn extract_single(
    raw_url: &str,
    mode: FetchMode,
    proxy: Option<&str>,
    config: &WebSearchConfig,
    cancel: &CancellationToken,
) -> Result<ExtractedContent, (String, Option<u16>, Option<Duration>)> {
    let started = Instant::now();
    let ssrf_settings = config.ssrf.clone().unwrap_or_default();
    let domain_policy = config.fetch_domain_policy.clone().unwrap_or_default();

    // 首跳校验(目标 URL 不享受任何 loopback 豁免)
    let mut current = ssrf::validate_remote_url(raw_url, &ssrf_settings, &domain_policy)
        .await
        .map_err(|message| (message, None, None))?;
    let empty_headers: Vec<(String, String)> = Vec::new();

    let response = loop {
        let http_options = HttpRequestOptions {
            proxy,
            timeout: FETCH_TIMEOUT,
            cancel: Some(cancel),
            sensitive_headers: &[],
        };
        let response = send_single_hop(
            reqwest::Method::GET,
            current.as_str(),
            &empty_headers,
            None,
            &http_options,
        )
        .await
        .map_err(|message| (message, None, Some(started.elapsed())))?;
        if !REDIRECT_STATUSES.contains(&response.status) {
            break response;
        }
        let Some(location) = &response.location else {
            break response;
        };
        // 下一跳重新校验(重定向目标不得绕过私网封锁/域策略)
        // 相对 Location:基于当前 URL 解析后再校验
        let absolute = crate::http::resolve_redirect_target(&response.url, location)
            .map_err(|message| (message, Some(response.status), Some(started.elapsed())))?;
        current = ssrf::validate_remote_url(&absolute, &ssrf_settings, &domain_policy)
            .await
            .map_err(|message| (message, Some(response.status), Some(started.elapsed())))?;
    };

    let content_type = response.content_type.clone().unwrap_or_default();
    let content = if content_type.starts_with("text/html")
        || content_type.starts_with("application/xhtml")
    {
        match mode {
            FetchMode::Readable => html_to_markdown(&response.body),
            FetchMode::Raw => response.body,
        }
    } else if content_type.starts_with("text/")
        || content_type == "application/json"
        || content_type.starts_with("application/xml")
        || content_type.is_empty()
    {
        response.body
    } else {
        return Err((
            format!(
                "Unsupported content type {content_type}; only HTML and text are supported (PDF/GitHub/YouTube/images are not available in rpi)"
            ),
            Some(response.status),
            Some(started.elapsed()),
        ));
    };
    Ok(ExtractedContent {
        url: response.url,
        title: String::new(),
        content,
        error: None,
        mime_type: Some(content_type),
        status: Some(response.status),
        duration: Some(started.elapsed().as_millis() as u64),
    })
}

// ---------------------------------------------------------------------------
// HTML → markdown(正文启发式提取 + htmd 转换)
// ---------------------------------------------------------------------------

/// 正文启发式:去 script/style/nav 等噪声节点,优先 article/main,否则全片段。
pub fn html_to_markdown(html: &str) -> String {
    let document = scraper::Html::parse_document(html);
    let noise: [&str; 12] = [
        "script", "style", "noscript", "iframe", "svg", "nav", "footer", "aside", "form",
        "button", "select", "input",
    ];
    // 正文根:article > main > [role=main] > body;找不到就整篇
    let mut fragment_html: Option<String> = None;
    for selector in ["article", "main", "[role=main]", "body"] {
        if let Ok(parsed) = scraper::Selector::parse(selector) {
            if let Some(element) = document.select(&parsed).next() {
                fragment_html = Some(element.inner_html());
                break;
            }
        }
    }
    let fragment = scraper::Html::parse_fragment(
        fragment_html.as_deref().unwrap_or(html),
    );
    let mut cleaned = String::new();
    for child in fragment.root_element().children() {
        if let Some(element_ref) = scraper::ElementRef::wrap(child) {
            if noise.contains(&element_ref.value().name()) {
                continue;
            }
            cleaned.push_str(&element_ref.html());
        } else if let Some(text) = scraper::Node::as_text(child.value()) {
            cleaned.push_str(text);
        }
    }
    let markdown = htmd::convert(&cleaned).unwrap_or_else(|_| {
        fragment
            .root_element()
            .text()
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    });
    markdown.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_extraction_drops_noise() {
        let html = r#"<html><head><style>body{color:red}</style></head><body>
            <nav>menu items</nav>
            <article><h1>Title</h1><p>Real content paragraph.</p></article>
            <footer>copyright</footer>
            <script>track()</script>
        </body></html>"#;
        let markdown = html_to_markdown(html);
        assert!(markdown.contains("Title"), "got: {markdown}");
        assert!(markdown.contains("Real content paragraph."));
        assert!(!markdown.contains("track()"));
        assert!(!markdown.contains("copyright"));
    }

    #[test]
    fn plain_fragment_safe() {
        assert_eq!(html_to_markdown("<p>plain</p>"), "plain");
    }
}
