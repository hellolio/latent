//! 搜索世界的纯类型(分析文档 §3):`SearchResult / SearchResponse / SearchOptions`、
//! domain filter 归一化与结果数 clamp(上游 search-result-count-normalization.ts
//! 与 domain-filter-normalization.ts,多个 provider 文件重复的实现收敛于此)。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecencyFilter {
    Day,
    Week,
    Month,
    Year,
}

impl RecencyFilter {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "day" => Some(Self::Day),
            "week" => Some(Self::Week),
            "month" => Some(Self::Month),
            "year" => Some(Self::Year),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
        }
    }
}

/// 单条来源(provider 的最小公约数据)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub snippet: String,
}

/// 已提取的页面内容(fetch/tavily raw_content/exa text 共用)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractedContent {
    pub url: String,
    #[serde(default)]
    pub title: String,
    pub content: String,
    /// None = 成功;Some = 该 URL 抓取失败的原因
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<u64>,
}

/// provider 的返回:answer 可为空,results 是来源列表。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchResponse {
    #[serde(default)]
    pub answer: String,
    #[serde(default)]
    pub results: Vec<SearchResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inline_content: Vec<ExtractedContent>,
}

/// 单次搜索的选项(provider 只消费这几个字段)。
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub num_results: Option<i64>,
    pub recency_filter: Option<RecencyFilter>,
    pub domain_filter: Option<Vec<String>>,
    /// includeContent(下游全量抓取);tavily/exa 用它决定要不要 raw content
    pub include_content: bool,
}

/// 结果数归一化(上游 search-result-count-normalization.ts):
/// 非有限数 → 5,否则 clamp 到 [1, 20]。
pub fn normalize_search_result_count(value: Option<i64>) -> usize {
    match value {
        None => 5,
        Some(n) if n < 1 => 1,
        Some(n) => n.min(20) as usize,
    }
}

// ---------------------------------------------------------------------------
// domain filter(上游 domain-filter-normalization.ts + 各 provider 的
// matchesDomainFilters 收敛)
// ---------------------------------------------------------------------------

/// 归一化后的过滤集:`-` 前缀进 blocked,其余进 allowed。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NormalizedDomainFilters {
    pub allowed: Vec<String>,
    pub blocked: Vec<String>,
}

impl NormalizedDomainFilters {
    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty() && self.blocked.is_empty()
    }
}

/// 域名归一化:去 `-` 前缀、小写、去开头 `.`、去端口/路径;裸名字含非法字符
/// 或点段为空时返回 None。
pub fn normalize_domain(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let trimmed = trimmed.strip_prefix('-').unwrap_or(trimmed);
    let mut domain = trimmed.to_lowercase();
    while let Some(stripped) = domain.strip_prefix('.') {
        domain = stripped.to_string();
    }
    if domain.is_empty() {
        return None;
    }
    // 去掉端口与路径尾部(pi 的 normalizeDomain 只保留 host 部分)
    let host = domain
        .split(['/', ':', '?', '#'])
        .next()
        .unwrap_or_default();
    if host.is_empty() {
        return None;
    }
    if host.split('.').any(|label| label.is_empty()) {
        return None;
    }
    if host.split('.').any(|label| {
        !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }) {
        return None;
    }
    Some(host.to_string())
}

pub fn normalize_domain_filters(domain_filter: Option<&[String]>) -> NormalizedDomainFilters {
    let mut filters = NormalizedDomainFilters::default();
    for raw in domain_filter.unwrap_or_default() {
        let Some(domain) = normalize_domain(raw) else {
            continue;
        };
        let target = if raw.trim().starts_with('-') {
            &mut filters.blocked
        } else {
            &mut filters.allowed
        };
        if !target.contains(&domain) {
            target.push(domain);
        }
    }
    filters
}

pub fn host_matches_domain(hostname: &str, domain: &str) -> bool {
    hostname == domain || hostname.ends_with(&format!(".{domain}"))
}

/// URL 的 host 是否命中过滤集;空集恒真。
pub fn matches_domain_filters(url: &str, filters: &NormalizedDomainFilters) -> bool {
    if filters.is_empty() {
        return true;
    }
    let hostname = match url_hostname(url) {
        Ok(hostname) => hostname,
        Err(_) => return false,
    };
    if !filters.allowed.is_empty()
        && !filters
            .allowed
            .iter()
            .any(|domain| host_matches_domain(&hostname, domain))
    {
        return false;
    }
    !filters
        .blocked
        .iter()
        .any(|domain| host_matches_domain(&hostname, domain))
}

/// UTF-8 percent-encode(保留字母数字与 -._~,空格转 +)。
pub fn encode_query_param(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// 提取 URL 的 hostname(小写,剥 IPv6 方括号;解析失败返回 Err)。
pub fn url_hostname(input: &str) -> Result<String, String> {
    let parsed = url::Url::parse(input).map_err(|error| error.to_string())?;
    let host = parsed.host_str().ok_or("URL has no host".to_string())?;
    Ok(host.trim_start_matches('[').trim_end_matches(']').to_lowercase())
}

// ---------------------------------------------------------------------------
// 伪造 answer(search-answer-formatting.ts:7-11,模型可见文案契约)
// ---------------------------------------------------------------------------

/// 单条来源行:`${snippet}\nSource: ${title} (${url})`(无 snippet 时省略首行)。
pub fn fake_answer_source_line(result: &SearchResult) -> String {
    if result.snippet.is_empty() {
        format!("Source: {} ({})", result.title, result.url)
    } else {
        format!(
            "{}\nSource: {} ({})",
            result.snippet, result.title, result.url
        )
    }
}

/// 多条以空行连接。
pub fn fake_answer_from_results(results: &[SearchResult]) -> String {
    results
        .iter()
        .map(fake_answer_source_line)
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_clamps_to_range() {
        assert_eq!(normalize_search_result_count(None), 5);
        assert_eq!(normalize_search_result_count(Some(0)), 1);
        assert_eq!(normalize_search_result_count(Some(-3)), 1);
        assert_eq!(normalize_search_result_count(Some(7)), 7);
        assert_eq!(normalize_search_result_count(Some(100)), 20);
    }

    #[test]
    fn domain_normalization() {
        assert_eq!(normalize_domain("Example.COM"), Some("example.com".into()));
        assert_eq!(normalize_domain("-Docs.Rust-lang.org/x"), Some("docs.rust-lang.org".into()));
        assert_eq!(normalize_domain(".a..b"), None);
        assert_eq!(normalize_domain("-"), None);
        assert_eq!(normalize_domain("bad domain"), None);
    }

    #[test]
    fn filter_matching() {
        let filters = normalize_domain_filters(Some(&[
            "rust-lang.org".to_string(),
            "-news.ycombinator.com".to_string(),
        ]));
        assert!(matches_domain_filters(
            "https://doc.rust-lang.org/book/",
            &filters
        ));
        assert!(!matches_domain_filters(
            "https://news.ycombinator.com/item?id=1",
            &filters
        ));
        assert!(!matches_domain_filters("https://example.com/", &filters));
        assert!(matches_domain_filters("anything", &NormalizedDomainFilters::default()));
    }

    #[test]
    fn hostname_extraction() {
        assert_eq!(
            url_hostname("https://user:pw@Example.COM:8443/p?a=b").unwrap(),
            "example.com"
        );
        assert_eq!(url_hostname("http://[::1]:80/x").unwrap(), "::1");
        assert!(url_hostname("not-a-url").is_err());
    }
}
