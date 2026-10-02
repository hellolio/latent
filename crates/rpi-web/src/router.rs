//! 路由/fallback 总入口(gemini-search.ts search() 的移植):
//! 数组扇出 / all / 显式单家 / auto(配置路由 → 硬编码链)。
//! auto 链的原则:每层缺失只跳过、不报错;全失败时聚合各 provider
//! 错误(行规整后)抛出;配置指引 `no_provider_guidance` 由工具层在
//! "全部 query 失败"时追加一次(分析文档 §2/§3)。

use tokio_util::sync::CancellationToken;

use crate::error::{ErrorKind, SearchProviderError};
use crate::providers::{
    all_providers, find_provider, provider_label, ProviderContext, ALL_ELIGIBLE, AUTO_CHAIN,
};
use crate::types::{SearchOptions, SearchResponse, SearchResult};

/// provider 选择(工具参数 provider 字段)。
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderSelection {
    Auto,
    All,
    Single(String),
    Multiple(Vec<String>),
}

impl ProviderSelection {
    pub fn from_value(value: Option<&serde_json::Value>) -> Result<Self, String> {
        match value {
            None => Ok(Self::Auto),
            Some(serde_json::Value::String(value)) => Ok(match value.as_str() {
                "auto" => Self::Auto,
                "all" => Self::All,
                other => Self::Single(other.to_string()),
            }),
            Some(serde_json::Value::Array(items)) => {
                let mut providers = Vec::new();
                for item in items {
                    let Some(name) = item.as_str() else {
                        return Err("provider array entries must be strings".to_string());
                    };
                    if name == "auto" || name == "all" {
                        return Err(format!(
                            "provider array must contain only concrete providers, got `{name}`"
                        ));
                    }
                    if !providers.iter().any(|existing| existing == name) {
                        providers.push(name.to_string());
                    }
                }
                if providers.is_empty() {
                    return Err("provider array must be non-empty".to_string());
                }
                Ok(Self::Multiple(providers))
            }
            Some(_) => Err("provider must be a string or an array of strings".to_string()),
        }
    }
}

/// 带 provider 归属的搜索结果(fan-out 时保留每个 provider 的原始回答,
/// 供 web_search 按 query 渲染)。
#[derive(Debug, Clone, Default)]
pub struct AttributedSearchResponse {
    pub provider: String,
    pub response: SearchResponse,
    /// 仅 fan-out 时有:每个 provider 的独立应答
    pub provider_responses: Vec<ProviderResponse>,
    /// 仅 fan-out 时有:每个 provider 的失败信息
    pub provider_errors: Vec<ProviderFailure>,
}

#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub provider: String,
    pub response: SearchResponse,
}

#[derive(Debug, Clone)]
pub struct ProviderFailure {
    pub provider: String,
    pub error: String,
}

/// 是否因取消而失败。
pub fn is_abort_error(error: &SearchProviderError) -> bool {
    error.kind == ErrorKind::Aborted
}

/// 主入口。`selection` 已经过归一化;`config.search_provider` 在 Auto 时生效。
pub async fn search(
    query: &str,
    selection: &ProviderSelection,
    options: &SearchOptions,
    ctx: &ProviderContext<'_>,
) -> Result<AttributedSearchResponse, SearchProviderError> {
    // Auto 时叠加配置里的 searchProvider(上游:options.provider 未指定时用配置)
    let resolved = match selection {
        ProviderSelection::Auto => {
            provider_selection_from_config(ctx.config.search_provider.as_ref())?
        }
        other => other.clone(),
    };
    match resolved {
        ProviderSelection::Multiple(providers) => {
            search_with_providers(query, options, ctx, &providers).await
        }
        ProviderSelection::All => search_all(query, options, ctx).await,
        ProviderSelection::Single(provider) => {
            let provider_impl = find_provider(&provider).ok_or_else(|| {
                SearchProviderError::classify("router", format!("unknown provider `{provider}`"))
            })?;
            let response = provider_impl.search(query, options, ctx).await?;
            Ok(AttributedSearchResponse {
                provider: provider_impl.id().to_string(),
                response,
                ..Default::default()
            })
        }
        ProviderSelection::Auto => auto_search(query, options, ctx).await,
    }
}

/// 配置里的 searchProvider 值(Auto 时的二级默认)。
fn provider_selection_from_config(
    value: Option<&serde_json::Value>,
) -> Result<ProviderSelection, SearchProviderError> {
    ProviderSelection::from_value(value)
        .map_err(|error| SearchProviderError::classify("router", error))
}

/// all:扇出到全部"合资格且可用"的 provider。
async fn search_all(
    query: &str,
    options: &SearchOptions,
    ctx: &ProviderContext<'_>,
) -> Result<AttributedSearchResponse, SearchProviderError> {
    let providers: Vec<String> = all_providers()
        .into_iter()
        .filter(|provider| ALL_ELIGIBLE.contains(&provider.id()))
        .filter(|provider| provider.is_available(ctx.config))
        .map(|provider| provider.id().to_string())
        .collect();
    if providers.is_empty() {
        return Err(SearchProviderError::classify(
            "router",
            "No configured search provider available for provider \"all\". DuckDuckGo is explicit-only and excluded; configure searxng/exa/brave/tavily.",
        ));
    }
    search_with_providers(query, options, ctx, &providers).await
}

/// 数组扇出(Promise.allSettled 等价):并发调用,按 URL 去重合并,
/// answer 按 provider 分节,失败 provider 进 Provider errors 节。
async fn search_with_providers(
    query: &str,
    options: &SearchOptions,
    ctx: &ProviderContext<'_>,
    providers: &[String],
) -> Result<AttributedSearchResponse, SearchProviderError> {
    let mut futures = Vec::with_capacity(providers.len());
    for name in providers {
        let provider_impl = find_provider(name).ok_or_else(|| {
            SearchProviderError::classify("router", format!("unknown provider `{name}`"))
        })?;
        let future = async move {
            let response = provider_impl.search(query, options, ctx).await;
            (name.clone(), response)
        };
        futures.push(future);
    }
    let settled = futures::future::join_all(futures).await;

    let mut successes: Vec<ProviderResponse> = Vec::new();
    let mut failures: Vec<ProviderFailure> = Vec::new();
    for (name, outcome) in settled {
        match outcome {
            Ok(response) => successes.push(ProviderResponse {
                provider: name,
                response,
            }),
            Err(error) => failures.push(ProviderFailure {
                provider: name,
                error: error.to_string(),
            }),
        }
    }
    if ctx.cancel.is_cancelled() {
        return Err(SearchProviderError::classify("router", "Aborted"));
    }
    if successes.is_empty() {
        let sections = failures
            .iter()
            .map(|failure| compact_error_text(&failure.provider, &failure.error))
            .collect::<Vec<_>>();
        return Err(SearchProviderError::classify(
            "router",
            format!("Selected-provider search failed:\n  - {}", sections.join("\n  - ")),
        ));
    }

    // URL 去重合并
    let mut results: Vec<SearchResult> = Vec::new();
    let mut seen_urls = std::collections::HashSet::new();
    let mut inline_content = Vec::new();
    let mut seen_inline_urls = std::collections::HashSet::new();
    for success in &successes {
        for result in &success.response.results {
            if seen_urls.insert(result.url.clone()) {
                results.push(result.clone());
            }
        }
        for content in &success.response.inline_content {
            if seen_inline_urls.insert(content.url.clone()) {
                inline_content.push(content.clone());
            }
        }
    }
    let mut answer_sections: Vec<String> = successes
        .iter()
        .map(|success| {
            format!(
                "## {}\n\n{}",
                provider_label(&success.provider),
                if success.response.answer.is_empty() {
                    "(No answer text returned.)"
                } else {
                    &success.response.answer
                }
            )
        })
        .collect();
    if !failures.is_empty() {
        answer_sections.push(format!(
            "## Provider errors\n\n{}",
            failures
                .iter()
                .map(|failure| format!("- **{}:** {}", provider_label(&failure.provider), failure.error))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    Ok(AttributedSearchResponse {
        provider: "all".to_string(),
        response: SearchResponse {
            answer: answer_sections.join("\n\n"),
            results,
            inline_content,
        },
        provider_responses: successes,
        provider_errors: failures,
    })
}

/// 配置路由(searchRouting.providers):按序尝试,只有 fallbackOn 命中的
/// 错误类别才换下一家;其余直接抛出。
async fn search_with_configured_routing(
    query: &str,
    options: &SearchOptions,
    ctx: &ProviderContext<'_>,
    routing: &crate::config::SearchRouting,
) -> Result<AttributedSearchResponse, SearchProviderError> {
    let mut diagnostics: Vec<String> = Vec::new();
    for provider in &routing.providers {
        let Some(provider_impl) = find_provider(provider) else {
            diagnostics.push(format!("{provider}: unknown provider"));
            continue;
        };
        if !provider_impl.is_available(ctx.config) {
            diagnostics.push(format!("{provider}: unavailable"));
            continue;
        }
        match provider_impl.search(query, options, ctx).await {
            Ok(response) => {
                return Ok(AttributedSearchResponse {
                    provider: provider_impl.id().to_string(),
                    response,
                    ..Default::default()
                })
            }
            Err(error) => {
                diagnostics.push(compact_error_text(
                    &format!("{} [{}]", provider, error.kind.as_str()),
                    &error.to_string(),
                ));
                let fallback_allowed = routing.fallback_on.iter().any(|kind| match kind {
                    crate::config::FallbackKind::Transient => error.kind == ErrorKind::Transient,
                    crate::config::FallbackKind::Quota => error.kind == ErrorKind::Quota,
                    crate::config::FallbackKind::Network => error.kind == ErrorKind::Network,
                    crate::config::FallbackKind::InvalidResponse => {
                        error.kind == ErrorKind::InvalidResponse
                    }
                    crate::config::FallbackKind::Unsupported => {
                        error.kind == ErrorKind::Unsupported
                    }
                });
                if !fallback_allowed {
                    return Err(error);
                }
            }
        }
    }
    Err(SearchProviderError::classify(
        "router",
        format!(
            "Configured search routing exhausted:\n  - {}",
            diagnostics.join("\n  - ")
        ),
    ))
}

/// auto 硬编码链(分析文档 §3 第 4 点的 rpi 子集)。
async fn auto_search(
    query: &str,
    options: &SearchOptions,
    ctx: &ProviderContext<'_>,
) -> Result<AttributedSearchResponse, SearchProviderError> {
    // 配置了显式 searchProvider 时不走配置路由(那是另一条路径);
    // searchRouting 只在未配置显式 provider 时生效(上游语义)
    if ctx.config.search_provider.is_none() {
        if let Some(routing) = ctx.config.search_routing.clone() {
            if !routing.providers.is_empty() {
                return search_with_configured_routing(query, options, ctx, &routing).await;
            }
        }
    }

    let mut fallback_errors: Vec<String> = Vec::new();
    for provider_id in AUTO_CHAIN {
        let Some(provider_impl) = find_provider(provider_id) else {
            continue;
        };
        if !provider_impl.is_available(ctx.config) {
            continue;
        }
        match provider_impl.search(query, options, ctx).await {
            Ok(response) => {
                return Ok(AttributedSearchResponse {
                    provider: provider_impl.id().to_string(),
                    response,
                    ..Default::default()
                })
            }
            Err(error) => {
                if is_abort_error(&error) {
                    // 取消直接上抛(与上游一致)
                    return Err(error);
                }
                // 凭据错误也不静默,但同样降级为行内诊断
                fallback_errors.push(compact_error_text(
                    &provider_label(provider_id),
                    &error.to_string(),
                ));
            }
        }
    }
    Err(SearchProviderError::classify(
        "router",
        format!("Auto provider search failed:\n  - {}", fallback_errors.join("\n  - ")),
    ))
}

/// 聚合错误单行规整:压掉行内换行(不破坏 `\n  - ` 列表结构)、去掉与
/// 行首重复的 provider 前缀、限长。配置指引由工具层在"全部 query 失败"
/// 时统一追加一次,不进错误串。
fn compact_error_text(label: &str, text: &str) -> String {
    const LINE_CAP: usize = 200;
    // text 已是 "{provider} search failed (...)" 时不再叠加 label,
    // 避免 "Duckduckgo: DuckDuckGo search failed (...)" 式重复
    let line = if text.to_lowercase().starts_with(&label.to_lowercase()) {
        text.to_string()
    } else {
        format!("{label}: {text}")
    };
    let single_line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() > LINE_CAP {
        let mut truncated: String = single_line.chars().take(LINE_CAP).collect();
        truncated.push_str("...");
        truncated
    } else {
        single_line
    }
}

/// 全部 provider 不可用时的兜底指引(gemini-search.ts:815-824 的 rpi 改写)。
pub fn no_provider_guidance() -> String {
    "No search provider available. Either:\n  \
     1. Set braveApiKey, tavilyApiKey, or exaApiKey in web-search.json (~/.rpi/web-search.json or .rpi/web-search.json)\n  \
     2. Set BRAVE_API_KEY, TAVILY_API_KEY, or EXA_API_KEY environment variables\n  \
     3. Set searxngBaseUrl (or SEARXNG_BASE_URL) to a self-hosted SearXNG instance\n  \
     4. Nothing to configure: the auto chain falls back to free DuckDuckGo HTML search"
        .to_string()
}

/// 取消令牌→错误(工具层在等待内部时使用)。
pub async fn with_cancel<T>(
    cancel: &CancellationToken,
    future: impl std::future::Future<Output = T>,
) -> Result<T, SearchProviderError> {
    tokio::select! {
        result = future => Ok(result),
        _ = cancel.cancelled() => Err(SearchProviderError::classify("router", "Aborted")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_parsing() {
        assert_eq!(
            ProviderSelection::from_value(None).unwrap(),
            ProviderSelection::Auto
        );
        assert_eq!(
            ProviderSelection::from_value(Some(&serde_json::json!("all"))).unwrap(),
            ProviderSelection::All
        );
        assert_eq!(
            ProviderSelection::from_value(Some(&serde_json::json!("brave"))).unwrap(),
            ProviderSelection::Single("brave".into())
        );
        assert_eq!(
            ProviderSelection::from_value(Some(&serde_json::json!(["brave", "tavily", "brave"])))
                .unwrap(),
            ProviderSelection::Multiple(vec!["brave".into(), "tavily".into()])
        );
        assert!(ProviderSelection::from_value(Some(&serde_json::json!([]))).is_err());
        assert!(ProviderSelection::from_value(Some(&serde_json::json!(["auto"]))).is_err());
        assert!(ProviderSelection::from_value(Some(&serde_json::json!(42))).is_err());
    }

    #[test]
    fn compact_line_dedupes_prefix_and_collapses_newlines() {
        // Display 前缀与 label 同名:不叠加
        let error = SearchProviderError::classify(
            "duckduckgo",
            "DuckDuckGo search error 202: <!DOCTYPE html>\n<html>noise</html>",
        );
        let line = compact_error_text("Duckduckgo", &error.to_string());
        assert!(!line.contains("Duckduckgo:"));
        assert!(!line.contains('\n'));
        assert!(line.contains("error 202"));

        // Display 前缀与 label 不同名:保留 label
        let line = compact_error_text("brave", "duckduckgo search failed (transient): boom");
        assert_eq!(line, "brave: duckduckgo search failed (transient): boom");
    }

    #[test]
    fn compact_line_caps_length() {
        let long = "x".repeat(500);
        let line = compact_error_text("brave", &long);
        assert_eq!(line.chars().count(), 200 + 3);
        assert!(line.ends_with("..."));
    }
}
