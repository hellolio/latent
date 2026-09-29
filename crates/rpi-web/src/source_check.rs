//! source_check artifact 构建(source-check.ts 的移植):
//! 来源 quality 分类、相关句段提取(passage)、sha256 content_hash、
//! 自动 assessment 保守降级(无语义评审能力时明确说明,交人工审阅)。

use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::storage;
use crate::types::{ExtractedContent, SearchResult};

pub type SourceQuality = &'static str;

/// classifySource(source-check.ts:66-94)。
pub fn classify_source(url: &str) -> SourceQuality {
    static OFFICIAL_DOCS_HOSTS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(developers\.|docs\.|learn\.|reference\.)|\.github\.io$").expect("static regex")
    });
    static OFFICIAL_DOCS_PATHS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)/(docs?|reference)(/|\b)").expect("static regex"));
    static VENDOR_DOCS_PATHS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)/(documentation|docs?)/").expect("static regex"));
    static REPO_ISSUE_PATHS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)/(issues|pull|pulls)/").expect("static regex"));
    static BLOG_HOSTS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(medium\.com|substack\.com|dev\.to|hashnode\.)").expect("static regex")
    });
    static BLOG_PATHS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)/blogs?/").expect("static regex"));
    static FORUM_HOSTS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(stackoverflow\.com|serverfault\.com|superuser\.com|discourse\.|community\.)")
            .expect("static regex")
    });
    static FORUM_PATHS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)/(forum|forums|threads)/").expect("static regex"));
    static NEWS_HOSTS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(reuters\.com|bloomberg\.com|techcrunch\.com|theverge\.com|arstechnica\.com|wired\.com|cnet\.com|zdnet\.com)")
            .expect("static regex")
    });
    static NEWS_PATHS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)/news(/|$)").expect("static regex"));

    let Ok(parsed) = url::Url::parse(url) else {
        return "unknown";
    };
    let host = parsed.host_str().unwrap_or_default();
    let path = parsed.path();
    if REPO_ISSUE_PATHS.is_match(path) {
        return "repo_issue";
    }
    if OFFICIAL_DOCS_HOSTS.is_match(host) || OFFICIAL_DOCS_PATHS.is_match(path) {
        return "official_docs";
    }
    if VENDOR_DOCS_PATHS.is_match(path) {
        return "vendor_docs";
    }
    if NEWS_HOSTS.is_match(host) || NEWS_PATHS.is_match(path) {
        return "news";
    }
    if FORUM_HOSTS.is_match(host) || FORUM_PATHS.is_match(path) {
        return "forum";
    }
    if BLOG_HOSTS.is_match(host) || BLOG_PATHS.is_match(path) {
        return "blog";
    }
    "unknown"
}

/// `sha256:<hex>`。
pub fn hash_content(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}

struct Span {
    text: String,
    start: usize,
    end: usize,
}

fn tokenize(value: &str) -> Vec<String> {
    let mut unique: Vec<String> = Vec::new();
    for term in value.to_lowercase().split(|c: char| !c.is_ascii_alphanumeric()) {
        if term.chars().count() > 3 && !unique.iter().any(|existing| existing == term) {
            unique.push(term.to_string());
        }
    }
    unique
}

/// 相关句段:句子切分 → 按提示词命中词数排序 → 取前 3(source-check.ts:106-125)。
fn extract_relevant_spans(content: &str, hint: &str) -> Vec<Span> {
    // 句子切分:`[^.!?]+([.!?]+ +|$)` 的等价手写(Rust regex 无 lookahead):
    // 累积字符,遇到 [.!?] 连续串(后随空白或结尾)即断句
    let mut sentences: Vec<Span> = Vec::new();
    let bytes = content.as_bytes();
    let mut sentence_start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        if matches!(bytes[index], b'.' | b'!' | b'?') {
            while index < bytes.len() && matches!(bytes[index], b'.' | b'!' | b'?') {
                index += 1;
            }
            if index >= bytes.len() || bytes[index].is_ascii_whitespace() {
                let raw = &content[sentence_start..index];
                let text = raw.trim();
                if !text.is_empty() && text.chars().count() <= 400 {
                    let leading = raw.len() - raw.trim_start().len();
                    let start = sentence_start + leading;
                    sentences.push(Span {
                        text: text.to_string(),
                        start,
                        end: start + text.len(),
                    });
                }
                sentence_start = index;
            }
        } else {
            index += 1;
        }
    }
    // 尾句(无终止符)
    if sentence_start < content.len() {
        let raw = &content[sentence_start..];
        let text = raw.trim();
        if !text.is_empty() && text.chars().count() <= 400 {
            let leading = raw.len() - raw.trim_start().len();
            let start = sentence_start + leading;
            sentences.push(Span {
                text: text.to_string(),
                start,
                end: start + text.len(),
            });
        }
    }
    let terms = tokenize(hint);
    if terms.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(usize, &Span, usize)> = sentences
        .iter()
        .enumerate()
        .map(|(index, sentence)| {
            let lower = sentence.text.to_lowercase();
            let score = terms
                .iter()
                .filter(|term| lower.contains(term.as_str()))
                .count();
            (index, sentence, score)
        })
        .filter(|(_, _, score)| *score > 0)
        .collect();
    scored.sort_by(|(index_a, _, score_a), (index_b, _, score_b)| {
        score_b.cmp(score_a).then(index_a.cmp(index_b))
    });
    scored
        .into_iter()
        .take(3)
        .map(|(_, sentence, _)| Span {
            text: sentence.text.clone(),
            start: sentence.start,
            end: sentence.end,
        })
        .collect()
}

fn passage_id(source_rank: usize, index: usize) -> String {
    format!("p-{source_rank}-{index}")
}

/// 来源 + 抓取结果 → passages(snippet 恒为 p-rank-0;全文提取 top-3 句段)。
pub fn build_passages(
    sources: &[serde_json::Value],
    fetched: &[ExtractedContent],
    hint: &str,
) -> Vec<serde_json::Value> {
    let mut passages: Vec<serde_json::Value> = Vec::new();
    for source in sources {
        let rank = source.get("rank").and_then(|value| value.as_u64()).unwrap_or(0) as usize;
        let url = source.get("url").and_then(|value| value.as_str()).unwrap_or_default();
        if let Some(snippet) = source.get("snippet").and_then(|value| value.as_str()).filter(|s| !s.is_empty()) {
            passages.push(serde_json::json!({
                "passage_id": passage_id(rank, 0),
                "source_url": url,
                "source_rank": rank,
                "text": snippet,
                "content_hash": hash_content(snippet),
            }));
        }
        let page = fetched.iter().find(|entry| entry.url == url);
        if let Some(page) = page {
            if page.error.is_none() && !page.content.is_empty() {
                let passage_hint = source
                    .get("snippet")
                    .and_then(|value| value.as_str())
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| hint.to_string());
                for (index, span) in extract_relevant_spans(&page.content, &passage_hint)
                    .into_iter()
                    .enumerate()
                {
                    passages.push(serde_json::json!({
                        "passage_id": passage_id(rank, index + 1),
                        "source_url": url,
                        "source_rank": rank,
                        "text": span.text,
                        "extraction_span": { "start": span.start, "end": span.end },
                        "content_hash": hash_content(&span.text),
                    }));
                }
            }
        }
    }
    passages
}

/// 自动 assessment:无语义评审能力,明确降级说明(source-check.ts:162-174)。
pub fn assess_claim(claim: &str, passages: &[serde_json::Value]) -> serde_json::Value {
    if passages.is_empty() {
        return serde_json::json!({
            "claim": claim,
            "status": "missing-evidence",
            "supporting_passages": [],
            "contradicting_passages": [],
            "rationale": "No passages were retrieved for the claim.",
            "confidence": 0.2,
        });
    }
    serde_json::json!({
        "claim": claim,
        "status": "unclear",
        "supporting_passages": [],
        "contradicting_passages": [],
        "rationale": "Passages were retrieved, but automated semantic support or contradiction assessment is unavailable; review the cited passages manually.",
        "confidence": 0.3,
    })
}

/// artifact 组装输入。
pub struct BuildArtifactInput<'a> {
    pub query: &'a str,
    pub provider: Option<&'a str>,
    pub summary: Option<&'a str>,
    pub results: &'a [(SearchResult, usize)], // (result, rank)
    pub fetched: &'a [ExtractedContent],
    pub recency: Option<crate::types::RecencyFilter>,
    pub domain_filter: Option<&'a [String]>,
}

/// ResearchArtifact(source-check.ts:190-231 的结构契约)。
pub fn build_research_artifact(input: BuildArtifactInput<'_>) -> serde_json::Value {
    let mut sources: Vec<serde_json::Value> = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (result, rank) in input.results {
        if !seen.insert(result.url.as_str()) {
            continue;
        }
        let page = input.fetched.iter().find(|entry| entry.url == result.url);
        let fetched_ok = page.is_some_and(|page| page.error.is_none());
        let mut source = serde_json::json!({
            "rank": rank,
            "url": result.url,
            "title": result.title,
            "quality": classify_source(&result.url),
            "fetched": fetched_ok,
        });
        if !result.snippet.is_empty() {
            source["snippet"] = serde_json::Value::String(result.snippet.clone());
        }
        if let Some(page) = page {
            source["fetch_timestamp"] = serde_json::json!(now_ms());
            if page.error.is_none() {
                source["content_hash"] = serde_json::Value::String(hash_content(&page.content));
            } else {
                source["fetch_error"] = serde_json::json!(page.error.clone().unwrap_or_default());
            }
        }
        sources.push(source);
    }
    let passages = build_passages(&sources, input.fetched, input.query);
    let domain_include: Vec<String> = input
        .domain_filter
        .unwrap_or_default()
        .iter()
        .filter(|domain| !domain.starts_with('-'))
        .cloned()
        .collect();
    let domain_exclude: Vec<String> = input
        .domain_filter
        .unwrap_or_default()
        .iter()
        .filter(|domain| domain.starts_with('-'))
        .map(|domain| domain[1..].to_string())
        .collect();

    let mut artifact = serde_json::json!({
        "id": storage::generate_id(),
        "type": "research",
        "timestamp": now_ms(),
        "query": input.query,
        "sources": sources,
        "passages": passages,
        "filters": {
            "recency": input.recency.map(|r| serde_json::Value::String(r.as_str().to_string())).unwrap_or(serde_json::Value::Null),
            "domain_include": domain_include,
            "domain_exclude": domain_exclude,
        },
    });
    if let Some(provider) = input.provider {
        artifact["provider"] = serde_json::Value::String(provider.to_string());
    }
    if let Some(summary) = input.summary {
        artifact["summary"] = serde_json::Value::String(summary.to_string());
    }
    artifact
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_classification() {
        assert_eq!(classify_source("https://docs.rs/clippy"), "official_docs");
        assert_eq!(classify_source("https://learn.microsoft.com/rust/"), "official_docs");
        // "developer." 不在 developers\. 前缀内(与上游一致,归 unknown)
        assert_eq!(classify_source("https://developer.mozilla.org/en-US/"), "unknown");
        assert_eq!(classify_source("https://example.github.io/page"), "official_docs");
        assert_eq!(classify_source("https://github.com/x/y/issues/12"), "repo_issue");
        assert_eq!(classify_source("https://medium.com/@a/post"), "blog");
        assert_eq!(classify_source("https://stackoverflow.com/questions/1"), "forum");
        assert_eq!(classify_source("https://reuters.com/tech/ai"), "news");
        assert_eq!(classify_source("https://random.example.com/page"), "unknown");
        assert_eq!(classify_source("not a url"), "unknown");
    }

    #[test]
    fn span_extraction_ranks_by_terms() {
        let content = "Irrelevant sentence. Rust async runtime tokio is popular. Another irrelevant. Tokio powers async Rust. Tail sentence.";
        let spans = extract_relevant_spans(content, "tokio async rust");
        assert!(!spans.is_empty());
        assert!(spans[0].text.contains("tokio") || spans[0].text.contains("Tokio"));
        assert!(spans.len() <= 3);
    }

    #[test]
    fn passage_building() {
        let sources = vec![serde_json::json!({
            "rank": 1,
            "url": "https://a.com",
            "snippet": "snippet text",
        })];
        let fetched = vec![ExtractedContent {
            url: "https://a.com".into(),
            title: "A".into(),
            content: "First relevant sentence about tokio. Filler. Second tokio sentence here.".into(),
            error: None,
            mime_type: None,
            status: None,
            duration: None,
        }];
        // snippet 非空时句段提示用 snippet(与上游 passageHint 语义一致)
        let passages = build_passages(&sources, &fetched, "tokio");
        assert_eq!(passages.len(), 1);
        assert_eq!(passages[0]["passage_id"], "p-1-0");
        assert!(passages[0]["content_hash"].as_str().unwrap().starts_with("sha256:"));

        // snippet 为空 → 用 hint 提取句段
        let sources_no_snippet = vec![serde_json::json!({
            "rank": 1,
            "url": "https://a.com",
        })];
        let passages = build_passages(&sources_no_snippet, &fetched, "tokio");
        assert!(passages.len() >= 2, "expected snippet-less hint spans");
        assert!(passages[1]["text"].as_str().unwrap().contains("tokio"));
    }

    #[test]
    fn assessment_degrades_honestly() {
        let empty = assess_claim("claim", &[]);
        assert_eq!(empty["status"], "missing-evidence");
        let with = assess_claim("claim", &[serde_json::json!({})]);
        assert_eq!(with["status"], "unclear");
    }
}
