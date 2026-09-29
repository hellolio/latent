//! provider 错误模型与分类器(gemini-search.ts:54-65, 329-370 的移植):
//! 从 HTTP 状态码 + 错误消息正则推断错误类别,路由 fallback 链据此决定
//! 是否换下一家。错误消息格式(`"{Provider} ... error {status}: ..."`)与
//! 上游保持一致,分类正则依赖这些前缀。

use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    Transient,
    Quota,
    Network,
    Credential,
    Config,
    Auth,
    InvalidRequest,
    InvalidResponse,
    Unsupported,
    Aborted,
    Unknown,
}

impl ErrorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Quota => "quota",
            Self::Network => "network",
            Self::Credential => "credential",
            Self::Config => "config",
            Self::Auth => "auth",
            Self::InvalidRequest => "invalid-request",
            Self::InvalidResponse => "invalid-response",
            Self::Unsupported => "unsupported",
            Self::Aborted => "aborted",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "transient" => Self::Transient,
            "quota" => Self::Quota,
            "network" => Self::Network,
            "credential" => Self::Credential,
            "config" => Self::Config,
            "auth" => Self::Auth,
            "invalid-request" => Self::InvalidRequest,
            "invalid-response" => Self::InvalidResponse,
            "unsupported" => Self::Unsupported,
            "aborted" => Self::Aborted,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }
}

/// 带 provider 归属与类别的搜索错误(Display 文本与上游
/// SearchProviderError 一致:`"{provider} search failed ({kind}): {message}"`)。
#[derive(Debug, Clone)]
pub struct SearchProviderError {
    pub provider: String,
    pub kind: ErrorKind,
    pub message: String,
    pub status: Option<u16>,
}

impl SearchProviderError {
    pub fn new(
        provider: impl Into<String>,
        kind: ErrorKind,
        message: impl Into<String>,
        status: Option<u16>,
    ) -> Self {
        SearchProviderError {
            provider: provider.into(),
            kind,
            message: message.into(),
            status,
        }
    }

    /// 把任意错误消息包装为分类后的 provider 错误。
    pub fn classify(provider: impl Into<String>, message: impl Into<String>) -> Self {
        let provider = provider.into();
        let message = message.into();
        let (kind, status) = classify_message(&provider, &message);
        SearchProviderError {
            provider,
            kind,
            message,
            status,
        }
    }
}

impl std::fmt::Display for SearchProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} search failed ({}): {}",
            self.provider,
            self.kind.as_str(),
            self.message
        )
    }
}

impl std::error::Error for SearchProviderError {}

/// 中止统一表达(取消令牌触发时手工构造)。
pub fn aborted(provider: impl Into<String>) -> SearchProviderError {
    SearchProviderError::new(provider, ErrorKind::Aborted, "Aborted", None)
}

fn provider_error_status(message: &str) -> Option<u16> {
    // 与上游一致:\b(?:error|status|http)\s+(\d{3})\b
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)\b(?:error|status|http)\s+(\d{3})\b").unwrap());
    re.captures(message)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

fn classify_message(provider: &str, message: &str) -> (ErrorKind, Option<u16>) {
    let lower = message.to_lowercase();
    let status = provider_error_status(message);
    static CREDENTIAL: OnceLock<Regex> = OnceLock::new();
    let credential = CREDENTIAL
        .get_or_init(|| Regex::new(r"(?i)(?:api )?key (?:not found|missing)|credential resolution").unwrap());
    static UNSUPPORTED: OnceLock<Regex> = OnceLock::new();
    let unsupported = UNSUPPORTED.get_or_init(|| {
        Regex::new(r"(?i)(?:web[_ -]?search|web[_ -]?search_preview|(?:the )?tool)\b.*\b(?:unsupported|not supported|does not support|doesn't support|unknown|unrecognized|unavailable|not found)|\b(?:unsupported|not supported|does not support|doesn't support|unknown|unrecognized|unavailable|not found)\b.*\b(?:web[_ -]?search|web[_ -]?search_preview|(?:the )?tool)").unwrap()
    });
    static QUOTA: OnceLock<Regex> = OnceLock::new();
    let quota = QUOTA.get_or_init(|| Regex::new(r"(?i)rate limit|quota|too many requests").unwrap());
    static AUTH: OnceLock<Regex> = OnceLock::new();
    let auth = AUTH.get_or_init(|| Regex::new(r"(?i)unauthorized|forbidden|permission denied").unwrap());
    static INVALID_REQUEST: OnceLock<Regex> = OnceLock::new();
    let invalid_request =
        INVALID_REQUEST.get_or_init(|| Regex::new(r"(?i)bad request|invalid request").unwrap());
    static INVALID_RESPONSE: OnceLock<Regex> = OnceLock::new();
    let invalid_response = INVALID_RESPONSE.get_or_init(|| {
        Regex::new(r"(?i)invalid json|no parseable response|no parseable results|invalid response|returned empty response|no web_search_call").unwrap()
    });
    static TRANSIENT: OnceLock<Regex> = OnceLock::new();
    let transient = TRANSIENT
        .get_or_init(|| Regex::new(r"(?i)temporar|service unavailable|server error").unwrap());
    static NETWORK: OnceLock<Regex> = OnceLock::new();
    let network = NETWORK.get_or_init(|| {
        Regex::new(r"(?i)fetch failed|network|econnreset|econnrefused|enotfound|etimedout|timed out|socket").unwrap()
    });
    static CONFIG: OnceLock<Regex> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        Regex::new(r"(?i)invalid or missing|invalid config|failed to parse|must be an? |configuration").unwrap()
    });

    let kind = if credential.is_match(&lower) {
        ErrorKind::Credential
    } else if lower.contains("abort") {
        ErrorKind::Aborted
    } else if status == Some(401) || status == Some(403) {
        ErrorKind::Auth
    } else if status == Some(400) || status == Some(422) {
        if provider == "openai" && unsupported.is_match(&lower) {
            ErrorKind::Unsupported
        } else {
            ErrorKind::InvalidRequest
        }
    } else if status == Some(402)
        || status == Some(429)
        || (provider == "tavily" && status == Some(432))
    {
        ErrorKind::Quota
    } else if status.is_some_and(|s| s == 408 || s == 425 || s >= 500) {
        ErrorKind::Transient
    } else if quota.is_match(&lower) {
        ErrorKind::Quota
    } else if auth.is_match(&lower) {
        ErrorKind::Auth
    } else if invalid_request.is_match(&lower) {
        ErrorKind::InvalidRequest
    } else if invalid_response.is_match(&lower) {
        ErrorKind::InvalidResponse
    } else if transient.is_match(&lower) {
        ErrorKind::Transient
    } else if network.is_match(&lower) {
        ErrorKind::Network
    } else if config.is_match(&lower) {
        ErrorKind::Config
    } else {
        ErrorKind::Unknown
    };
    (kind, status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_of(message: &str) -> ErrorKind {
        SearchProviderError::classify("brave", message).kind
    }

    #[test]
    fn classification_matches_upstream() {
        assert_eq!(
            kind_of("Brave Search API error 401: bad key"),
            ErrorKind::Auth
        );
        assert_eq!(
            kind_of("Tavily API error 429: rate limited"),
            ErrorKind::Quota
        );
        assert_eq!(
            kind_of("Tavily API error 432: quota"),
            ErrorKind::Quota
        );
        assert_eq!(
            kind_of("SearXNG search error 502: upstream"),
            ErrorKind::Transient
        );
        assert_eq!(
            kind_of("Exa search returned invalid JSON: unexpected token"),
            ErrorKind::InvalidResponse
        );
        assert_eq!(
            kind_of("Brave API key not found. Either:"),
            ErrorKind::Credential
        );
        assert_eq!(
            kind_of("error sending request: ECONNRESET"),
            ErrorKind::Network
        );
        assert_eq!(kind_of("AbortError"), ErrorKind::Aborted);
        assert_eq!(
            SearchProviderError::classify("tavily", "weird").kind,
            ErrorKind::Unknown
        );
    }

    #[test]
    fn display_format_matches_upstream() {
        let error = SearchProviderError::new("brave", ErrorKind::Auth, "boom", Some(401));
        assert_eq!(error.to_string(), "brave search failed (auth): boom");
    }
}
