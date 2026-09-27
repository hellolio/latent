//! Model / Provider / URL 配置体系(Pi 风格):`models.json` 声明
//! provider(baseUrl/api/apiKey/headers/compat/models),provider 级字段为
//! 默认值、model 级可覆盖;`settings.json` 提供 defaultProvider/defaultModel。
//!
//! 读取顺序:全局 `~/.rpi/models.json` 为主,项目 `.rpi/models.json` 覆盖
//! (同名 provider 按字段合并,model 按 id 合并,项目侧字段/条目优先)——
//! 覆盖内置 provider 的 baseUrl 时无需重定义其全部模型。
//!
//! apiKey 语义:值优先按环境变量名解析,未命中时当字面值使用(兼容 ollama
//! 等 `apiKey: "ollama"` 写法);真实 key 不应写入 models.json。

use std::collections::BTreeMap;
use std::path::Path;

use rpi_ai::Model;

use crate::model::{create_model_resolver, ModelResolver};

// ---------------------------------------------------------------------------
// models.json 数据模型(serde camelCase,与 Pi 字段层级一致)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelsFile {
    #[serde(default)]
    providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderConfig {
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    compat: Option<serde_json::Value>,
    #[serde(default)]
    models: Vec<ModelConfig>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelConfig {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    compat: Option<serde_json::Value>,
    #[serde(default)]
    reasoning: Option<bool>,
    #[serde(default)]
    input: Option<Vec<String>>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    #[serde(default)]
    sampling_params: Option<serde_json::Map<String, serde_json::Value>>,
}

/// 合并 provider 配置:覆盖方(项目)的非空字段逐项覆盖,models 按 id 合并。
fn merge_provider(base: &mut ProviderConfig, over: ProviderConfig) {
    if over.base_url.is_some() {
        base.base_url = over.base_url;
    }
    if over.api.is_some() {
        base.api = over.api;
    }
    if over.api_key.is_some() {
        base.api_key = over.api_key;
    }
    if over.headers.is_some() {
        base.headers = over.headers;
    }
    if over.compat.is_some() {
        base.compat = over.compat;
    }
    for model in over.models {
        match base.models.iter_mut().find(|m| m.id == model.id) {
            Some(existing) => merge_model(existing, model),
            None => base.models.push(model),
        }
    }
}

/// 合并 model 配置:覆盖方的非空字段逐项覆盖。
fn merge_model(base: &mut ModelConfig, over: ModelConfig) {
    if over.name.is_some() {
        base.name = over.name;
    }
    if over.base_url.is_some() {
        base.base_url = over.base_url;
    }
    if over.api.is_some() {
        base.api = over.api;
    }
    if over.api_key.is_some() {
        base.api_key = over.api_key;
    }
    if over.headers.is_some() {
        base.headers = over.headers;
    }
    if over.compat.is_some() {
        base.compat = over.compat;
    }
    if over.reasoning.is_some() {
        base.reasoning = over.reasoning;
    }
    if over.input.is_some() {
        base.input = over.input;
    }
    if over.context_window.is_some() {
        base.context_window = over.context_window;
    }
    if over.max_tokens.is_some() {
        base.max_tokens = over.max_tokens;
    }
    if over.thinking_level_map.is_some() {
        base.thinking_level_map = over.thinking_level_map;
    }
    if over.sampling_params.is_some() {
        base.sampling_params = over.sampling_params;
    }
}

/// 读单个 models.json;解析失败返回诊断(调用方打 stderr,不阻断)。
/// 文件不存在是常态(未自定义 provider),不算错误、静默跳过。
fn parse_models_file(path: &Path) -> Result<ModelsFile, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ModelsFile::default()),
        Err(e) => return Err(e.to_string()),
    };
    serde_json::from_str::<ModelsFile>(&text).map_err(|e| format!("{}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// 凭据 / headers 解析:env 名优先,未命中当字面值
// ---------------------------------------------------------------------------

fn resolve_credential_value(value: &str) -> Option<String> {
    match std::env::var(value) {
        Ok(env_value) if !env_value.trim().is_empty() => Some(env_value),
        // env 名命中但值为空:视为未配置凭据(None),
        // 不回退字面值 —— 否则 env 变量名会被当作 key 发给 provider
        Ok(_) => None,
        Err(_) => Some(value.to_string()),
    }
}

fn resolve_headers(
    headers: &BTreeMap<String, String>,
) -> std::collections::HashMap<String, String> {
    headers
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                resolve_credential_value(value).unwrap_or_else(|| value.clone()),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 配置 → ModelResolver 注册
// ---------------------------------------------------------------------------

/// 把合并后的 provider/model 配置注册进 resolver。诊断(provider 缺 api、
/// model 缺 id 等)打 stderr 并跳过对应条目,不阻断其余配置。
fn register_config(resolver: &mut ModelResolver, providers: BTreeMap<String, ProviderConfig>) {
    for (provider_id, provider) in providers {
        let api = match provider.api.as_deref() {
            Some(api) => api.to_string(),
            None => match rpi_ai::default_provider_endpoint(&provider_id) {
                Some((api, _)) => api.to_string(),
                None => {
                    eprintln!(
                        "[rpi] models.json: provider `{provider_id}` 缺少 api 字段(且不是内置 provider),已跳过"
                    );
                    continue;
                }
            },
        };
        let api_key = provider
            .api_key
            .as_deref()
            .and_then(resolve_credential_value);
        let headers = provider.headers.as_ref().map(resolve_headers);
        let compat = provider.compat.clone();

        // provider 级 override:覆盖内置端点 / 为裸 provider 解析提供 baseUrl
        resolver.set_provider_override(
            &provider_id,
            provider.base_url.clone(),
            &api,
            api_key.clone(),
            headers.clone(),
        );

        for model in provider.models {
            if model.id.trim().is_empty() {
                eprintln!(
                    "[rpi] models.json: provider `{provider_id}` 存在缺少 id 的 model,已跳过"
                );
                continue;
            }
            let model_api = model.api.as_deref().unwrap_or(&api);
            let mut resolved = Model::minimal(&model.id, model_api, &provider_id);
            resolved.name = model.name.unwrap_or_default();
            resolved.base_url = model
                .base_url
                .or(provider.base_url.clone())
                .unwrap_or_default();
            // 凭据:模型级 > provider 级;env 名优先,未命中当字面值
            resolved.api_key = model
                .api_key
                .as_deref()
                .and_then(resolve_credential_value)
                .or_else(|| api_key.clone());
            resolved.headers = match (model.headers.as_ref().map(resolve_headers), &headers) {
                (Some(model_headers), Some(provider_headers)) => {
                    let mut merged = provider_headers.clone();
                    merged.extend(model_headers);
                    Some(merged.into_iter().collect())
                }
                (Some(model_headers), None) => Some(model_headers.into_iter().collect()),
                (None, provider_headers) => provider_headers.clone(),
            };
            resolved.compat = model.compat.or_else(|| compat.clone());
            resolved.reasoning = model.reasoning.unwrap_or(false);
            if let Some(input) = model.input {
                resolved.input = input;
            }
            if let Some(window) = model.context_window {
                resolved.context_window = window;
            }
            if let Some(max_tokens) = model.max_tokens {
                resolved.max_tokens = max_tokens;
            }
            resolved.thinking_level_map = model.thinking_level_map;
            resolved.sampling_params = model.sampling_params;
            resolver.register_model(resolved);
        }
    }
}

/// 从 项目 `.rpi/models.json` + 全局 `~/.rpi/models.json` 加载配置并构建
/// resolver(内置 provider 端点表始终可用)。文件缺失 = 空配置;解析失败 =
/// 诊断 + 跳过。
pub fn create_model_resolver_from_config(
    project_dir: Option<&Path>,
    home: Option<&Path>,
) -> ModelResolver {
    let mut resolver = create_model_resolver();
    let mut providers = BTreeMap::new();
    for path in models_config_paths(project_dir, home) {
        match parse_models_file(&path) {
            Ok(file) => {
                for (id, provider) in file.providers {
                    match providers.get_mut(&id) {
                        Some(existing) => merge_provider(existing, provider),
                        None => {
                            providers.insert(id, provider);
                        }
                    }
                }
            }
            Err(error) => eprintln!("[rpi] models.json 解析失败:{error}"),
        }
    }
    register_config(&mut resolver, providers);
    resolver
}

/// models.json 候选路径:全局在前,项目在后(后读者覆盖先读者)。
fn models_config_paths(project_dir: Option<&Path>, home: Option<&Path>) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = home {
        paths.push(home.join(".rpi/models.json"));
    }
    if let Some(project) = project_dir {
        paths.push(project.join(".rpi/models.json"));
    }
    paths
}

// ---------------------------------------------------------------------------
// settings.json 默认模型(defaultProvider / defaultModel)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsDefaults {
    #[serde(default, alias = "default_provider")]
    default_provider: Option<String>,
    #[serde(default, alias = "default_model")]
    default_model: Option<String>,
    /// TUI 主题名(ratatui-themes kebab-case,如 `tokyo-night`);仅供
    /// interactive 模式读取,这里只存字符串、不解析
    #[serde(default)]
    theme: Option<String>,
}

/// 默认模型选择:项目 `.rpi/settings.json` 优先于全局,首个非空 defaultProvider
/// /defaultModel 生效。返回 (provider, model),provider 可能缺失(此时
/// defaultModel 应为 `provider/model` 或已注册模型 id)。
pub fn load_default_model_selection(
    project_dir: Option<&Path>,
    home: Option<&Path>,
) -> (Option<String>, Option<String>) {
    let mut paths = Vec::new();
    if let Some(project) = project_dir {
        paths.push(project.join(".rpi/settings.json"));
    }
    if let Some(home) = home {
        paths.push(home.join(".rpi/settings.json"));
    }
    let mut default_provider = None;
    let mut default_model = None;
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(settings) = serde_json::from_str::<SettingsDefaults>(&text) else {
            continue;
        };
        if default_provider.is_none() {
            default_provider = settings.default_provider.filter(|v| !v.trim().is_empty());
        }
        if default_model.is_none() {
            default_model = settings.default_model.filter(|v| !v.trim().is_empty());
        }
    }
    (default_provider, default_model)
}

/// TUI 主题名:项目 `.rpi/settings.json` 优先于全局,首个非空 `theme` 生效。
/// 只返回原始字符串;解析/降级由 rpi-tui 的 `Theme::resolve` 负责。
pub fn load_theme_setting(project_dir: Option<&Path>, home: Option<&Path>) -> Option<String> {
    let mut paths = Vec::new();
    if let Some(project) = project_dir {
        paths.push(project.join(".rpi/settings.json"));
    }
    if let Some(home) = home {
        paths.push(home.join(".rpi/settings.json"));
    }
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(settings) = serde_json::from_str::<SettingsDefaults>(&text) else {
            continue;
        };
        if let Some(theme) = settings.theme.filter(|v| !v.trim().is_empty()) {
            return Some(theme);
        }
    }
    None
}

// 引用 model.rs 的工厂(避免循环 use):见文件底 re-export

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rpi_config_test_{}_{}",
                tag,
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
        fn write(&self, rel: &str, text: &str) {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn custom_provider_resolves_with_url_and_key() {
        let dir = TempDir::new("custom");
        dir.write(
            ".rpi/models.json",
            r#"{
                "providers": {
                    "local": {
                        "baseUrl": "http://localhost:8080/v1",
                        "api": "openai-completions",
                        "apiKey": "LOCAL_API_KEY",
                        "models": [{ "id": "my-model", "contextWindow": 32000 }]
                    }
                }
            }"#,
        );
        // env 未设置 → apiKey 字面值兜底
        std::env::remove_var("LOCAL_API_KEY");
        let resolver = create_model_resolver_from_config(Some(&dir.0), None);
        let model = resolver.resolve("local/my-model").unwrap();
        assert_eq!(model.base_url, "http://localhost:8080/v1");
        assert_eq!(model.api, "openai-completions");
        assert_eq!(model.api_key.as_deref(), Some("LOCAL_API_KEY"));
        assert_eq!(model.context_window, 32000);
        // 裸 provider → 首个注册模型
        let model = resolver.resolve("local").unwrap();
        assert_eq!(model.id, "my-model");
        // 裸模型 id 也可解析
        let model = resolver.resolve("my-model").unwrap();
        assert_eq!(model.provider, "local");
    }

    #[test]
    fn builtin_provider_url_override_keeps_models() {
        let dir = TempDir::new("override");
        dir.write(
            ".rpi/models.json",
            r#"{ "providers": { "openai": { "baseUrl": "https://my-proxy.example.com/v1" } } }"#,
        );
        let resolver = create_model_resolver_from_config(Some(&dir.0), None);
        // 不要求重定义全部 openai 模型:默认模型仍可解析,仅 URL 被覆盖
        let model = resolver.resolve("openai/gpt-4.1-mini").unwrap();
        assert_eq!(model.base_url, "https://my-proxy.example.com/v1");
        assert_eq!(model.api, "openai-completions");
    }

    #[test]
    fn project_overrides_global_per_field() {
        let global = TempDir::new("global");
        let project = TempDir::new("project");
        global.write(
            ".rpi/models.json",
            r#"{ "providers": {
                "proxy": { "baseUrl": "http://global:1/v1", "api": "openai-completions",
                    "models": [{ "id": "m1", "contextWindow": 1000 }, { "id": "m2" }] }
            } }"#,
        );
        project.write(
            ".rpi/models.json",
            r#"{ "providers": { "proxy": { "baseUrl": "http://project:2/v1",
                "models": [{ "id": "m1", "contextWindow": 2000 }] } } }"#,
        );
        let resolver = create_model_resolver_from_config(Some(&project.0), Some(&global.0));
        // 项目覆盖 baseUrl,m2(仅全局定义)保留
        let model = resolver.resolve("proxy/m2").unwrap();
        assert_eq!(model.base_url, "http://project:2/v1");
        // model 按 id 合并:项目侧字段覆盖,未覆盖字段继承全局
        let model = resolver.resolve("proxy/m1").unwrap();
        assert_eq!(model.context_window, 2000);
    }

    #[test]
    fn model_level_overrides_provider_level() {
        let dir = TempDir::new("model_over");
        dir.write(
            ".rpi/models.json",
            r#"{ "providers": {
                "foo": { "baseUrl": "http://foo:1/v1", "api": "openai-completions",
                    "headers": { "X-Provider": "p" },
                    "models": [
                        { "id": "a" },
                        { "id": "b", "api": "anthropic-messages", "headers": { "X-Model": "m" } }
                    ] }
            } }"#,
        );
        let resolver = create_model_resolver_from_config(Some(&dir.0), None);
        let a = resolver.resolve("foo/a").unwrap();
        assert_eq!(a.base_url, "http://foo:1/v1");
        assert_eq!(a.api, "openai-completions");
        assert_eq!(a.headers.as_ref().unwrap()["X-Provider"], "p");
        let b = resolver.resolve("foo/b").unwrap();
        assert_eq!(b.api, "anthropic-messages");
        assert_eq!(b.headers.as_ref().unwrap()["X-Model"], "m");
        assert_eq!(b.headers.as_ref().unwrap()["X-Provider"], "p");
    }

    #[test]
    fn api_key_env_name_resolved_before_literal() {
        let dir = TempDir::new("envkey");
        dir.write(
            ".rpi/models.json",
            r#"{ "providers": { "p1": { "baseUrl": "http://p1/v1", "api": "openai-completions",
                "apiKey": "RPI_CONFIG_TEST_KEY", "models": [{ "id": "m" }] } } }"#,
        );
        std::env::set_var("RPI_CONFIG_TEST_KEY", "env-value");
        let resolver = create_model_resolver_from_config(Some(&dir.0), None);
        let model = resolver.resolve("p1/m").unwrap();
        assert_eq!(model.api_key.as_deref(), Some("env-value"));
        std::env::remove_var("RPI_CONFIG_TEST_KEY");
    }

    #[test]
    fn settings_default_model_selection() {
        let global = TempDir::new("sg");
        let project = TempDir::new("sp");
        global.write(".rpi/settings.json", r#"{ "defaultProvider": "openai" }"#);
        let (provider, model) = load_default_model_selection(Some(&project.0), Some(&global.0));
        assert_eq!(provider.as_deref(), Some("openai"));
        assert_eq!(model, None);
        project.write(
            ".rpi/settings.json",
            r#"{ "defaultProvider": "local", "defaultModel": "my-model" }"#,
        );
        let (provider, model) = load_default_model_selection(Some(&project.0), Some(&global.0));
        assert_eq!(provider.as_deref(), Some("local"));
        assert_eq!(model.as_deref(), Some("my-model"));
    }

    #[test]
    fn theme_setting_project_overrides_global() {
        let global = TempDir::new("tg");
        let project = TempDir::new("tp");
        // 无配置 → None
        assert_eq!(load_theme_setting(Some(&project.0), Some(&global.0)), None);
        global.write(".rpi/settings.json", r#"{ "theme": "nord" }"#);
        assert_eq!(
            load_theme_setting(Some(&project.0), Some(&global.0)).as_deref(),
            Some("nord")
        );
        // 项目覆盖全局;空字符串视为未配置
        project.write(
            ".rpi/settings.json",
            r#"{ "theme": "tokyo-night", "defaultModel": "m" }"#,
        );
        assert_eq!(
            load_theme_setting(Some(&project.0), Some(&global.0)).as_deref(),
            Some("tokyo-night")
        );
        project.write(".rpi/settings.json", r#"{ "theme": "  " }"#);
        assert_eq!(
            load_theme_setting(Some(&project.0), Some(&global.0)).as_deref(),
            Some("nord")
        );
    }
}
