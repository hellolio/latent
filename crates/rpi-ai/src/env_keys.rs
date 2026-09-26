//! 环境变量 → provider 凭据映射(pi 的 env-api-keys.ts 子集,02 文档 §5)。
//!
//! M1 只覆盖常用 API-key 型 provider;OAuth 型凭据(github-copilot 等)随 M6 补齐。

/// provider 支持的环境变量名(按优先级)。
pub fn env_api_key_vars(provider: &str) -> Option<&'static [&'static str]> {
    const ANTHROPIC: &[&str] = &["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"];
    const OPENAI: &[&str] = &["OPENAI_API_KEY"];
    const AZURE: &[&str] = &["AZURE_OPENAI_API_KEY"];
    const DEEPSEEK: &[&str] = &["DEEPSEEK_API_KEY"];
    const GOOGLE: &[&str] = &["GEMINI_API_KEY"];
    const GROQ: &[&str] = &["GROQ_API_KEY"];
    const CEREBRAS: &[&str] = &["CEREBRAS_API_KEY"];
    const XAI: &[&str] = &["XAI_API_KEY"];
    const OPENROUTER: &[&str] = &["OPENROUTER_API_KEY"];
    const ZAI: &[&str] = &["ZAI_API_KEY"];
    const MISTRAL: &[&str] = &["MISTRAL_API_KEY"];
    const MOONSHOT: &[&str] = &["MOONSHOT_API_KEY"];
    const TOGETHER: &[&str] = &["TOGETHER_API_KEY"];
    const FIREWORKS: &[&str] = &["FIREWORKS_API_KEY"];
    const MINIMAX: &[&str] = &["MINIMAX_API_KEY"];
    const XIAOMI: &[&str] = &["XIAOMI_API_KEY"];
    const PI_MESSAGES: &[&str] = &["RADIUS_API_KEY"];

    match provider {
        "anthropic" => Some(ANTHROPIC),
        "openai" | "openai-codex" => Some(OPENAI),
        "azure-openai-responses" => Some(AZURE),
        "deepseek" => Some(DEEPSEEK),
        "google" => Some(GOOGLE),
        "groq" => Some(GROQ),
        "cerebras" => Some(CEREBRAS),
        "xai" => Some(XAI),
        "openrouter" => Some(OPENROUTER),
        "zai" | "zai-coding-cn" => Some(ZAI),
        "mistral" => Some(MISTRAL),
        "moonshotai" | "moonshotai-cn" | "kimi-coding" => Some(MOONSHOT),
        "together" => Some(TOGETHER),
        "fireworks" => Some(FIREWORKS),
        "minimax" | "minimax-cn" => Some(MINIMAX),
        "xiaomi" => Some(XIAOMI),
        "radius" | "pi-messages" => Some(PI_MESSAGES),
        _ => None,
    }
}

/// 从已知环境变量取 provider 的 API key(与 pi getEnvApiKey 请求侧一致;
/// 发现/状态语义含 ANTHROPIC_AUTH_TOKEN 的部分随 M4 的 auth 模块补齐)。
/// 未配置返回 None。
pub fn get_env_api_key(provider: &str) -> Option<String> {
    let vars = env_api_key_vars(provider)?;
    for var in vars {
        if let Ok(value) = std::env::var(var) {
            if !value.trim().is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// 解析请求使用的 API key:显式 opts 优先,其次环境变量(02 文档 §5 resolve 子集)。
pub fn resolve_api_key(provider: &str, explicit: Option<&str>) -> Option<String> {
    if let Some(key) = explicit {
        if !key.trim().is_empty() {
            return Some(key.to_string());
        }
    }
    get_env_api_key(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_skips_auth_token_for_requests() {
        // AUTH_TOKEN 参与发现(get_env_api_key 不含它)
        let vars = env_api_key_vars("anthropic").unwrap();
        assert!(!vars.contains(&"ANTHROPIC_AUTH_TOKEN"));
        assert!(vars.contains(&"ANTHROPIC_API_KEY"));
    }

    #[test]
    fn explicit_key_wins_over_env() {
        assert_eq!(resolve_api_key("openai", Some("sk-explicit")), Some("sk-explicit".into()));
        assert_eq!(resolve_api_key("unknown-provider", Some("x")), Some("x".into()));
        assert_eq!(resolve_api_key("unknown-provider", None), None);
    }
}
