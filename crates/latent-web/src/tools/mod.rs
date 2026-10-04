//! web 工具层:4 个工具 + 注入面(WebContext)。
//!
//! 工具只消费 WebContext,不自读配置/环境 —— 与 subagent 工具同款 DI 纪律;
//! 对 latent-core 的后台完成通知能力以 trait 注入,
//! 实现留在 latent-cli 装配层,保持本 crate 不依赖 latent-core。

pub mod fetch_content;
pub mod get_search_content;
pub mod source_check;
pub mod web_search;

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::WebSearchConfig;
use crate::llm::LlmDeps;

/// includeContent 后台抓取完成后的新 turn 通知(subagent supervisor 的
/// follow_up 模式)。
#[async_trait]
pub trait BackgroundNotifier: Send + Sync {
    async fn notify(&self, text: String);
}

/// 工具共享上下文。
pub struct WebContext {
    pub config: WebSearchConfig,
    /// 磁盘 fetch 缓存上限
    pub cache_limits: crate::config::CacheLimits,
    pub llm: LlmDeps,
    /// includeContent 后台通知;None = 静默完成
    pub notifier: Option<Arc<dyn BackgroundNotifier>>,
    /// agent 转录裁剪上限(与 AgentLoopConfig.tool_result_max_chars 同一
    /// 配置源;0 = agent 层未限制)。工具输出预算据此派生,保证输出不触发
    /// agent 层的头尾裁剪。
    pub tool_result_max_chars: usize,
    pub cwd: std::path::PathBuf,
}

impl WebContext {
    /// web 输出预算:配置值与 agent 派生值取较小。
    /// 所有分页指引/默认 limit/切片大小都应从本函数取值,与模型实际
    /// 看到的内容严格一致。
    pub fn max_inline_chars(&self) -> usize {
        let configured = crate::bounded::effective_max_inline_chars(
            self.config.max_inline_content_chars,
        );
        let agent_limit = latent_agent::tool_self_output_limit(self.tool_result_max_chars);
        if agent_limit == 0 {
            configured
        } else {
            configured.min(agent_limit)
        }
    }
}

/// 出口:返回 4 个 web 工具(随会话常驻,无懒激活)。
pub fn create_web_tools(context: Arc<WebContext>) -> Vec<Arc<dyn latent_agent::Tool>> {
    vec![
        Arc::new(web_search::WebSearchTool::new(context.clone())),
        Arc::new(source_check::SourceCheckTool::new(context.clone())),
        Arc::new(fetch_content::FetchContentTool::new(context.clone())),
        Arc::new(get_search_content::GetSearchContentTool::new(context)),
    ]
}

/// 工具名常量(latent 内不提供改名面,与上游 toolNames 默认一致)。
pub mod names {
    pub const WEB_SEARCH: &str = "web_search";
    pub const SOURCE_CHECK: &str = "source_check";
    pub const FETCH_CONTENT: &str = "fetch_content";
    pub const GET_SEARCH_CONTENT: &str = "get_search_content";
}

// ---------------------------------------------------------------------------
// 共享参数解析
// ---------------------------------------------------------------------------

pub(crate) struct SearchParams {
    pub query_list: Vec<String>,
    pub num_results: Option<i64>,
    pub include_content: bool,
    pub recency_filter: Option<crate::types::RecencyFilter>,
    pub domain_filter: Option<Vec<String>>,
    pub selection: crate::router::ProviderSelection,
    pub proxy: Option<String>,
}

/// 容错解析 query/queries(query 展开优先级与上游一致:queries 存在时优先)。
pub(crate) fn parse_search_params(args: &serde_json::Value) -> Result<SearchParams, String> {
    let obj = args
        .as_object()
        .ok_or("arguments must be an object")?;
    let mut query_list: Vec<String> = Vec::new();
    if let Some(queries) = obj.get("queries") {
        let Some(items) = queries.as_array() else {
            return Err("`queries` must be an array of strings".to_string());
        };
        for item in items {
            let Some(value) = item.as_str() else {
                return Err("`queries` entries must be strings".to_string());
            };
            let trimmed = value.trim();
            if !trimmed.is_empty() && !query_list.iter().any(|existing| existing == trimmed) {
                query_list.push(trimmed.to_string());
            }
        }
    }
    if query_list.is_empty() {
        if let Some(query) = obj.get("query") {
            let value = query
                .as_str()
                .ok_or("`query` must be a string")?
                .trim()
                .to_string();
            if !value.is_empty() {
                query_list.push(value);
            }
        }
    }
    if query_list.is_empty() {
        return Err("Provide `query` or `queries` (1-4 varied angles)".to_string());
    }
    let num_results = match obj.get("numResults") {
        Some(value) if !value.is_null() => Some(
            value
                .as_i64()
                .ok_or("`numResults` must be an integer")?,
        ),
        _ => None,
    };
    let include_content = obj
        .get("includeContent")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let recency_filter = match obj.get("recencyFilter") {
        Some(value) if !value.is_null() => Some(
            crate::types::RecencyFilter::parse(value.as_str().ok_or(
                "`recencyFilter` must be a string",
            )?)
            .ok_or("`recencyFilter` must be one of day/week/month/year")?,
        ),
        _ => None,
    };
    let domain_filter = match obj.get("domainFilter") {
        Some(value) if !value.is_null() => {
            let items = value
                .as_array()
                .ok_or("`domainFilter` must be an array of strings")?;
            let mut domains = Vec::new();
            for item in items {
                let domain = item
                    .as_str()
                    .ok_or("`domainFilter` entries must be strings")?
                    .trim()
                    .to_string();
                if !domain.is_empty() {
                    domains.push(domain);
                }
            }
            Some(domains)
        }
        _ => None,
    };
    let selection = crate::router::ProviderSelection::from_value(obj.get("provider"))?;
    // workflow 不暴露给模型:由 web-search.json 的 `workflow` 配置决定
    // (none = 原始结果,auto-summary = 摘要替代),见 WebSearchConfig::workflow
    let proxy = match obj.get("proxy") {
        Some(value) if !value.is_null() => Some(
            value
                .as_str()
                .ok_or("`proxy` must be a string")?
                .to_string(),
        ),
        _ => None,
    };
    Ok(SearchParams {
        query_list,
        num_results,
        include_content,
        recency_filter,
        domain_filter,
        selection,
        proxy,
    })
}

pub(crate) fn parse_proxy_arg(args: &serde_json::Value) -> Result<Option<String>, String> {
    match args.get("proxy") {
        Some(value) if !value.is_null() => Ok(Some(
            value
                .as_str()
                .ok_or("`proxy` must be a string")?
                .to_string(),
        )),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(
        max_inline_content_chars: Option<usize>,
        tool_result_max_chars: usize,
    ) -> WebContext {
        WebContext {
            config: crate::config::WebSearchConfig {
                max_inline_content_chars,
                ..Default::default()
            },
            cache_limits: Default::default(),
            llm: crate::llm::LlmDeps {
                provider: latent_ai::create_mock_provider("x"),
                resolve_model: Arc::new(|spec| Ok(latent_ai::Model::minimal(spec, "mock", "mock"))),
                current_model: Arc::new(|| None),
            },
            notifier: None,
            tool_result_max_chars,
            cwd: std::path::PathBuf::from("."),
        }
    }

    #[test]
    fn max_inline_is_aligned_with_agent_transcript_cap() {
        // 默认装配:agent 20k 派生 18k,配置 30k 被压到 18k(原 P0 错位点)
        assert_eq!(context(None, 20_000).max_inline_chars(), 18_000);
        // agent 层未限制(0)→ 用配置值
        assert_eq!(context(None, 0).max_inline_chars(), 30_000);
        // 配置更小 → 取配置值
        assert_eq!(context(Some(5_000), 20_000).max_inline_chars(), 5_000);
        // agent 上限更大 → 仍受配置 clamp
        assert_eq!(context(Some(100_000), 200_000).max_inline_chars(), 100_000);
    }
}
