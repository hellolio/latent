//! web 工具层:5 个工具 + 注入面(WebContext)。
//!
//! 工具只消费 WebContext,不自读配置/环境 —— 与 subagent 工具同款 DI 纪律;
//! 对 rpi-core 的两个能力(激活工具集、后台完成通知)以 trait 注入,
//! 实现留在 rpi-cli 装配层,保持本 crate 不依赖 rpi-core。

pub mod fetch_content;
pub mod get_search_content;
pub mod source_check;
pub mod web_access;
pub mod web_search;

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::WebSearchConfig;
use crate::llm::LlmDeps;

/// web_access 需要的"把工具加入激活集"能力(set_active_tools_by_name 的桥)。
#[async_trait]
pub trait ToolSetActivator: Send + Sync {
    async fn activate(&self, names: &[String]) -> Result<(), String>;
}

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
    /// web_access 用;None = 懒激活不可用(web_access 直接报错)
    pub activator: Option<Arc<dyn ToolSetActivator>>,
    /// includeContent 后台通知;None = 静默完成
    pub notifier: Option<Arc<dyn BackgroundNotifier>>,
    pub cwd: std::path::PathBuf,
}

impl WebContext {
    pub fn max_inline_chars(&self) -> usize {
        crate::bounded::effective_max_inline_chars(self.config.max_inline_content_chars)
    }
}

/// 出口:按懒激活约定返回 5 个工具(web_access 恒激活,其余默认不进激活集)。
pub fn create_web_tools(context: Arc<WebContext>) -> Vec<Arc<dyn rpi_agent::Tool>> {
    vec![
        Arc::new(web_search::WebSearchTool::new(context.clone())),
        Arc::new(source_check::SourceCheckTool::new(context.clone())),
        Arc::new(fetch_content::FetchContentTool::new(context.clone())),
        Arc::new(get_search_content::GetSearchContentTool::new(context.clone())),
        Arc::new(web_access::WebEnableTool::new(context)),
    ]
}

/// 工具名常量(rpi 内不提供改名面,与上游 toolNames 默认一致)。
pub mod names {
    pub const WEB_SEARCH: &str = "web_search";
    pub const SOURCE_CHECK: &str = "source_check";
    pub const FETCH_CONTENT: &str = "fetch_content";
    pub const GET_SEARCH_CONTENT: &str = "get_search_content";
    pub const WEB_ACCESS: &str = "web_access";

    /// 懒激活的目标工具(web_access 之外的全部)。
    pub const ACTIVATABLE: [&str; 4] = [
        WEB_SEARCH,
        SOURCE_CHECK,
        FETCH_CONTENT,
        GET_SEARCH_CONTENT,
    ];

    pub fn activatable_strings() -> Vec<String> {
        ACTIVATABLE.iter().map(|name| (*name).to_string()).collect()
    }
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
    pub workflow_none: bool,
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
    let workflow = match obj.get("workflow") {
        Some(value) if !value.is_null() => {
            let value = value.as_str().ok_or("`workflow` must be a string")?;
            match value {
                "none" => true,
                "auto-summary" => false,
                "summary-review" => {
                    return Err(
                        "workflow \"summary-review\" is not available in rpi (browser curator omitted); use \"auto-summary\""
                            .to_string(),
                    )
                }
                other => {
                    return Err(format!(
                        "`workflow` must be none or auto-summary, got `{other}`"
                    ))
                }
            }
        }
        _ => true,
    };
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
        workflow_none: workflow,
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
