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

/// 请求侧统一凭据解析(models.json 体系的关键点:自定义 provider 不被
/// env 白名单提前拦截):
/// 1. 显式凭据(models.json apiKey 解析结果 / opts.api_key)非空 → 直接使用;
/// 2. 内置 provider(env 白名单内)→ 读对应环境变量,缺失报错;
/// 3. 自定义 provider(白名单外)→ 允许无凭据(请求不带 Authorization)。
pub fn resolve_request_credential(
    provider: &str,
    explicit: Option<&str>,
) -> Result<Option<String>, String> {
    if let Some(key) = explicit {
        if !key.trim().is_empty() {
            return Ok(Some(key.to_string()));
        }
    }
    if let Some(key) = get_env_api_key(provider) {
        return Ok(Some(key));
    }
    if env_api_key_vars(provider).is_some() {
        return Err(format!(
            "No API key for provider: {provider}(请在环境变量或 models.json 的 apiKey 中配置)"
        ));
    }
    Ok(None)
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
        assert_eq!(
            resolve_api_key("openai", Some("sk-explicit")),
            Some("sk-explicit".into())
        );
        assert_eq!(
            resolve_api_key("unknown-provider", Some("x")),
            Some("x".into())
        );
        assert_eq!(resolve_api_key("unknown-provider", None), None);
    }

    #[test]
    fn request_credential_explicit_beats_env_and_allows_keyless_custom() {
        // 显式凭据(含字面值)优先
        assert_eq!(
            resolve_request_credential("openai", Some("sk-from-config")).unwrap(),
            Some("sk-from-config".into())
        );
        // 白名单 provider 缺 env → 报错(不静默发无 key 请求)
        assert!(resolve_request_credential("openai", None).is_err());
        // 白名单外自定义 provider → 允许无凭据
        assert_eq!(resolve_request_credential("my-proxy", None).unwrap(), None);
        // 空白显式值视为未提供
        assert_eq!(
            resolve_request_credential("my-proxy", Some("  ")).unwrap(),
            None
        );
    }
}
