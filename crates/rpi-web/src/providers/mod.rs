//! provider 抽象与注册表(上游 gemini-search.ts 的文件约定收拢为 trait):
//! - `is_available` 是纯本地检查(配置/env),不发网络请求、零开销;
//! - `search` 只消费 SearchOptions;
//! - 路由链(auto)与 `all` 扇出的合资格集合在此定义。
//!
//! 精选 5 家(取舍已确认):duckduckgo(零配置兜底)/ searxng(自托管)/
//! brave(GET-JSON 风格)/ tavily(POST-JSON + key-pool 风格)/ exa(key REST)。

pub mod brave;
pub mod duckduckgo;
pub mod exa;
pub mod searxng;
pub mod tavily;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::config::WebSearchConfig;
use crate::error::SearchProviderError;
use crate::types::SearchOptions;
use crate::types::SearchResponse;

/// provider 执行期上下文(每次调用组装,不驻留)。
pub struct ProviderContext<'a> {
    pub config: &'a WebSearchConfig,
    pub cancel: &'a CancellationToken,
    /// 工具调用级的代理覆盖;None = 不指定(provider 可回退 config.proxy)
    pub proxy: Option<&'a str>,
}

impl ProviderContext<'_> {
    /// 代理解析:工具参数 > 配置;空串 = 强制直连。
    pub fn effective_proxy(&self) -> Option<&str> {
        match self.proxy {
            Some(proxy) => {
                if proxy.trim().is_empty() {
                    None
                } else {
                    Some(proxy)
                }
            }
            None => self.config.proxy.as_deref(),
        }
    }
}

#[async_trait]
pub trait SearchProvider: Send + Sync {
    /// 小写 id(provider 枚举值)。
    fn id(&self) -> &'static str;
    /// 展示名(错误文案/结果标注用)。
    fn label(&self) -> &'static str;
    /// 纯本地可用性检查:配置里有 key / env 存在 / base url 有效。
    fn is_available(&self, config: &WebSearchConfig) -> bool;
    async fn search(
        &self,
        query: &str,
        options: &SearchOptions,
        ctx: &ProviderContext<'_>,
    ) -> Result<SearchResponse, SearchProviderError>;
}

/// auto 链顺序(上游硬编码链的 rpi 子集):自托管 → key 通道 → 零配置兜底。
pub const AUTO_CHAIN: [&str; 5] = ["searxng", "exa", "brave", "tavily", "duckduckgo"];

/// `all` 扇出合资格集合(上游原则:opt-in 的 duckduckgo 不进 all,
/// 免得零 key 用户没点名就被扇出)。
pub const ALL_ELIGIBLE: [&str; 4] = ["searxng", "exa", "brave", "tavily"];

/// 注册表(顺序即 AUTO_CHAIN)。
pub fn all_providers() -> Vec<Box<dyn SearchProvider>> {
    vec![
        Box::new(searxng::SearXngProvider),
        Box::new(exa::ExaProvider),
        Box::new(brave::BraveProvider),
        Box::new(tavily::TavilyProvider),
        Box::new(duckduckgo::DuckDuckGoProvider),
    ]
}

pub fn find_provider(id: &str) -> Option<Box<dyn SearchProvider>> {
    all_providers().into_iter().find(|provider| provider.id() == id)
}

/// provider label(上游 providerLabel 的子集)。
pub fn provider_label(id: &str) -> String {
    let mut chars = id.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// 解析 API base URL:显式配置 > 环境变量 > 默认端点(上游 resolveApiBaseUrl)。
pub fn resolve_api_base_url(
    configured: Option<&str>,
    environment_name: &str,
    default_value: &str,
) -> String {
    if let Some(configured) = configured {
        let trimmed = configured.trim();
        if !trimmed.is_empty() {
            return trimmed.trim_end_matches('/').to_string();
        }
    }
    if let Ok(value) = std::env::var(environment_name) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.trim_end_matches('/').to_string();
        }
    }
    default_value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_covers_chain() {
        let providers = all_providers();
        let ids: Vec<&str> = providers.iter().map(|p| p.id()).collect();
        for id in AUTO_CHAIN {
            assert!(ids.contains(&id), "auto chain provider {id} not registered");
        }
        for id in ALL_ELIGIBLE {
            assert!(ids.contains(&id), "all-eligible provider {id} not registered");
        }
    }

    #[test]
    fn base_url_resolution() {
        assert_eq!(
            resolve_api_base_url(Some("https://x.example/"), "X_BASE_URL", "https://default"),
            "https://x.example"
        );
        unsafe { std::env::set_var("RPI_TEST_BASE_URL", "https://env.example/") };
        assert_eq!(
            resolve_api_base_url(None, "RPI_TEST_BASE_URL", "https://default"),
            "https://env.example"
        );
        assert_eq!(
            resolve_api_base_url(None, "RPI_TEST_MISSING_BASE_URL", "https://default"),
            "https://default"
        );
    }

    #[test]
    fn label_capitalizes() {
        assert_eq!(provider_label("brave"), "Brave");
        assert_eq!(provider_label("duckduckgo"), "Duckduckgo");
    }
}
