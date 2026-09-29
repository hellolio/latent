//! web-search.json 配置(上游 `~/.pi/agent/web-search.json` 的 rpi 对应物):
//! `~/.rpi/web-search.json` + 项目 `.rpi/web-search.json`(后读者逐字段覆盖)。
//! 凭据字段保留原始串(可能是 $ENV / !命令),调用时经 credential 模块解析。

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// fallback 链上允许触发换家的错误类别(上游 SearchRoutingConfig.fallbackOn)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackKind {
    Transient,
    Quota,
    Network,
    InvalidResponse,
    Unsupported,
}

#[derive(Debug, Clone, Default)]
pub struct SearchRouting {
    /// 显式路由顺序(provider id 列表)
    pub providers: Vec<String>,
    pub fallback_on: Vec<FallbackKind>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheLimits {
    pub max_entries: usize,
    pub max_bytes: usize,
}

impl Default for CacheLimits {
    fn default() -> Self {
        // 上游 DEFAULT_CACHE_LIMITS:128 条 / 128MB
        CacheLimits {
            max_entries: 128,
            max_bytes: 128 * 1024 * 1024,
        }
    }
}

/// fetch_content 的域策略(fetchContent.domainPolicy)。
#[derive(Debug, Clone, Default)]
pub struct FetchDomainPolicy {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

/// ssrf 配置(fetch 目标的私网封锁豁免)。
#[derive(Debug, Clone, Default)]
pub struct SsrfSettings {
    /// 豁免 CIDR(如 TUN/fake-IP 代理的 198.18.0.0/15)
    pub allow_ranges: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct WebSearchConfig {
    // --- provider 凭据与端点(原始串,未解析) ---
    pub brave_api_key: Option<String>,
    pub brave_base_url: Option<String>,
    pub tavily_api_key: Option<String>,
    pub tavily_base_url: Option<String>,
    pub exa_api_key: Option<String>,
    pub exa_base_url: Option<String>,
    pub searxng_base_url: Option<String>,
    pub searxng_headers: Option<Vec<(String, String)>>,

    // --- 路由 ---
    /// 显式 provider:string("brave") / "all" / "auto" / 数组
    pub search_provider: Option<serde_json::Value>,
    pub search_routing: Option<SearchRouting>,

    // --- 输出与行为 ---
    /// 有界输出上限(默认 30000,上限 200000)
    pub max_inline_content_chars: Option<usize>,
    /// 每次出网请求的默认代理(http/https/socks5h;工具调用可逐次覆盖)
    pub proxy: Option<String>,
    /// 摘要 / answer 模式小模型("provider/model-id");未配置回退当前主模型
    pub summary_model: Option<String>,

    // --- 存储 ---
    pub cache_limits: Option<CacheLimits>,

    // --- fetch 安全 ---
    pub fetch_domain_policy: Option<FetchDomainPolicy>,
    pub ssrf: Option<SsrfSettings>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigFile {
    #[serde(alias = "brave_api_key")]
    brave_api_key: Option<String>,
    brave_base_url: Option<String>,
    tavily_api_key: Option<String>,
    tavily_base_url: Option<String>,
    exa_api_key: Option<String>,
    exa_base_url: Option<String>,
    searxng_base_url: Option<String>,
    searxng_headers: Option<std::collections::BTreeMap<String, String>>,
    search_provider: Option<serde_json::Value>,
    search_routing: Option<RoutingFile>,
    max_inline_content_chars: Option<usize>,
    proxy: Option<String>,
    summary_model: Option<String>,
    cache: Option<CacheFile>,
    fetch_content: Option<FetchContentFile>,
    ssrf: Option<SsrfFile>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoutingFile {
    providers: Vec<String>,
    #[serde(default)]
    fallback_on: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CacheFile {
    max_entries: Option<usize>,
    max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FetchContentFile {
    domain_policy: Option<DomainPolicyFile>,
}

#[derive(Debug, Deserialize)]
struct DomainPolicyFile {
    allow: Option<Vec<String>>,
    deny: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SsrfFile {
    allow_ranges: Option<Vec<String>>,
}

impl From<RoutingFile> for SearchRouting {
    fn from(file: RoutingFile) -> Self {
        SearchRouting {
            providers: file.providers,
            fallback_on: file
                .fallback_on
                .iter()
                .filter_map(|kind| match kind.as_str() {
                    "transient" => Some(FallbackKind::Transient),
                    "quota" => Some(FallbackKind::Quota),
                    "network" => Some(FallbackKind::Network),
                    "invalid-response" => Some(FallbackKind::InvalidResponse),
                    "unsupported" => Some(FallbackKind::Unsupported),
                    _ => None,
                })
                .collect(),
        }
    }
}

/// 合并:project 有值(含显式 null 覆盖语义之上的 Some)则覆盖 global。
fn merge(global: &mut WebSearchConfig, project: WebSearchConfig) {
    macro_rules! take {
        ($field:ident) => {
            if project.$field.is_some() {
                global.$field = project.$field.clone();
            }
        };
    }
    take!(brave_api_key);
    take!(brave_base_url);
    take!(tavily_api_key);
    take!(tavily_base_url);
    take!(exa_api_key);
    take!(exa_base_url);
    take!(searxng_base_url);
    take!(searxng_headers);
    take!(search_provider);
    take!(search_routing);
    take!(max_inline_content_chars);
    take!(proxy);
    take!(summary_model);
    take!(cache_limits);
    take!(fetch_domain_policy);
    take!(ssrf);
}

fn parse_file(path: &Path) -> Result<Option<WebSearchConfig>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    serde_json::from_str::<ConfigFile>(&text)
        .map(|file| Some(from_config_file(file)))
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn from_config_file(file: ConfigFile) -> WebSearchConfig {
    let ConfigFile {
        brave_api_key,
        brave_base_url,
        tavily_api_key,
        tavily_base_url,
        exa_api_key,
        exa_base_url,
        searxng_base_url,
        searxng_headers,
        search_provider,
        search_routing,
        max_inline_content_chars,
        proxy,
        summary_model,
        cache,
        fetch_content,
        ssrf,
    } = file;
    WebSearchConfig {
        brave_api_key,
        brave_base_url,
        tavily_api_key,
        tavily_base_url,
        exa_api_key,
        exa_base_url,
        searxng_base_url,
        searxng_headers: searxng_headers.map(|map| map.into_iter().collect()),
        search_provider,
        search_routing: search_routing.map(Into::into),
        max_inline_content_chars: max_inline_content_chars.map(|n| n.clamp(1, 200_000)),
        proxy,
        summary_model,
        cache_limits: cache.map(|limits| CacheLimits {
            max_entries: limits
                .max_entries
                .unwrap_or_else(|| CacheLimits::default().max_entries),
            max_bytes: limits
                .max_bytes
                .unwrap_or_else(|| CacheLimits::default().max_bytes),
        }),
        fetch_domain_policy: fetch_content.map(|fetch| FetchDomainPolicy {
            allow: fetch.domain_policy.as_ref().and_then(|p| p.allow.clone()).unwrap_or_default(),
            deny: fetch.domain_policy.as_ref().and_then(|p| p.deny.clone()).unwrap_or_default(),
        }),
        ssrf: ssrf.map(|ssrf| SsrfSettings {
            allow_ranges: ssrf.allow_ranges.unwrap_or_default(),
        }),
    }
}

/// 配置文件候选路径:全局在前,项目在后(与 models.json 同序)。
pub fn config_paths(project_dir: Option<&Path>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = home {
        paths.push(home.join(".rpi/web-search.json"));
    }
    if let Some(project) = project_dir {
        paths.push(project.join(".rpi/web-search.json"));
    }
    paths
}

/// 加载 + 合并;解析失败打 stderr 跳过对应文件(容错风格同 models.json)。
pub fn load_web_search_config(project_dir: Option<&Path>, home: Option<&Path>) -> WebSearchConfig {
    let mut merged = WebSearchConfig::default();
    for path in config_paths(project_dir, home) {
        match parse_file(&path) {
            Ok(Some(file)) => merge(&mut merged, file),
            Ok(None) => {}
            Err(error) => eprintln!("[rpi][web] web-search.json 解析失败:{error}"),
        }
    }
    merged
}

/// 配置目录(缓存落盘位置):~/.rpi/。
pub fn config_dir(home: Option<&Path>) -> PathBuf {
    home.map(|home| home.join(".rpi"))
        .unwrap_or_else(|| PathBuf::from(".rpi"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let file: ConfigFile = serde_json::from_str(
            r#"{
                "braveApiKey": "$BRAVE_API_KEY",
                "tavilyApiKey": "!op read tavily",
                "searxngBaseUrl": "https://search.example.com/",
                "searchProvider": ["brave", "tavily"],
                "searchRouting": {"providers": ["searxng", "brave"], "fallbackOn": ["network", "transient"]},
                "maxInlineContentChars": 50000,
                "summaryModel": "anthropic/claude-haiku-4-5",
                "cache": {"maxEntries": 16, "maxBytes": 1048576},
                "fetchContent": {"domainPolicy": {"allow": ["example.com"], "deny": ["evil.example"]}},
                "ssrf": {"allowRanges": ["198.18.0.0/15"]}
            }"#,
        )
        .unwrap();
        let config = from_config_file(file);
        assert_eq!(config.brave_api_key.as_deref(), Some("$BRAVE_API_KEY"));
        assert_eq!(config.tavily_api_key.as_deref(), Some("!op read tavily"));
        assert_eq!(config.searxng_base_url.as_deref(), Some("https://search.example.com/"));
        assert_eq!(
            config.search_provider,
            Some(serde_json::json!(["brave", "tavily"]))
        );
        let routing = config.search_routing.unwrap();
        assert_eq!(routing.providers, vec!["searxng", "brave"]);
        assert_eq!(routing.fallback_on, vec![FallbackKind::Network, FallbackKind::Transient]);
        assert_eq!(config.max_inline_content_chars, Some(50000));
        assert_eq!(config.summary_model.as_deref(), Some("anthropic/claude-haiku-4-5"));
        assert_eq!(
            config.cache_limits,
            Some(CacheLimits { max_entries: 16, max_bytes: 1_048_576 })
        );
        assert_eq!(
            config.fetch_domain_policy.unwrap().allow,
            vec!["example.com"]
        );
        assert_eq!(config.ssrf.unwrap().allow_ranges, vec!["198.18.0.0/15"]);
    }

    #[test]
    fn inline_cap_is_clamped() {
        let file: ConfigFile = serde_json::from_str(r#"{"maxInlineContentChars": 999999}"#).unwrap();
        let config = from_config_file(file);
        assert_eq!(config.max_inline_content_chars, Some(200_000));
    }
}
