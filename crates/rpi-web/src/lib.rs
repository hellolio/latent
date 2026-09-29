//! 分层:仅依赖 rpi-agent(Tool trait)+ rpi-ai(LLM 注入),宿主能力
//! (模型解析、工具集切换、后台通知)经 `WebDeps` 注入,不依赖 rpi-core。
//!
//! 核心设计(分析文档 §1/§5):搜索结果不直接全文塞给模型 —— 有界输出 +
//! 存储 + responseId 二次检索;provider 的 `is_available` 纯本地零开销,
//! auto 链按"有 key 用 key、没 key 用免费"逐层降级。

pub mod bounded;
pub mod content;
pub mod prompts;
pub mod config;
pub mod credential;
pub mod error;
pub mod http;
pub mod llm;
pub mod providers;
pub mod router;
pub mod source_check;
pub mod summary;
pub mod storage;
pub mod tools;
pub mod types;

pub use tools::{create_web_tools, BackgroundNotifier, ToolSetActivator, WebContext};

/// 有界输出与存储的装配参数(工具层构造)。
pub struct WebRuntime {
    pub config: config::WebSearchConfig,
    /// 磁盘 fetch 缓存上限(配置 cache 段,默认 128 条/128MB)
    pub cache_limits: config::CacheLimits,
}

impl WebRuntime {
    pub fn load(project_dir: Option<&std::path::Path>, home: Option<&std::path::Path>) -> Self {
        let config = config::load_web_search_config(project_dir, home);
        let cache_limits = config.cache_limits.unwrap_or_default();
        storage::set_fetch_cache_dir(config::config_dir(home).join("web-search-cache"));
        WebRuntime {
            config,
            cache_limits,
        }
    }

    pub fn max_inline_chars(&self) -> usize {
        bounded::effective_max_inline_chars(self.config.max_inline_content_chars)
    }
}
