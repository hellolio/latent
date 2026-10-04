//! 模型解析(04 文档 §5 model-resolver 的 M4 子集):`provider/model` 字符串
//! → Model;未知 provider 报错;可注册自定义模型(scoped models 的轻量版)。

use std::collections::{BTreeMap, HashMap};

use rpi_ai::{default_provider_endpoint, Model};

#[derive(Clone, Debug)]
struct ProviderOverride {
    /// None = 保留内置端点 URL(仅覆盖 apiKey/headers 的场景)
    base_url: Option<String>,
    api: String,
    api_key: Option<String>,
    headers: Option<HashMap<String, String>>,
}

#[derive(Default)]
pub struct ModelResolver {
    extra_models: Vec<Model>,
    provider_overrides: BTreeMap<String, ProviderOverride>,
    /// false 时 available_models 不追加内置 provider 默认表(models.json
    /// 顶层 `showBuiltinModels: false`);自定义模型照常列出
    show_builtin_models: bool,
}

/// 工厂:空 resolver(内置 provider 端点表始终可用;默认追加内置默认模型
/// 到 /model 候选)。
pub fn create_model_resolver() -> ModelResolver {
    ModelResolver {
        show_builtin_models: true,
        ..ModelResolver::default()
    }
}

/// 各 provider 的默认模型(pi model-resolver 默认值的对应表)。
pub fn default_model_for(provider_id: &str) -> Option<&'static str> {
    match provider_id {
        "anthropic" => Some("claude-sonnet-4-5"),
        "openai" => Some("gpt-4.1-mini"),
        "deepseek" => Some("deepseek-chat"),
        "groq" => Some("llama-3.3-70b-versatile"),
        "xai" => Some("grok-3"),
        "openrouter" => Some("openrouter/auto"),
        "zai" => Some("glm-4.7"),
        "mistral" => Some("mistral-large-latest"),
        "moonshotai" => Some("kimi-k2-0711-preview"),
        "together" => Some("meta-llama/Llama-3.3-70B-Instruct-Turbo"),
        "fireworks" => Some("accounts/fireworks/models/llama-v3p3-70b-instruct"),
        _ => None,
    }
}

impl ModelResolver {
    pub fn register_model(&mut self, model: Model) {
        self.extra_models.push(model);
    }

    /// `/model` 候选是否追加内置 provider 默认表(缺省 true)。
    pub fn set_show_builtin_models(&mut self, show: bool) {
        self.show_builtin_models = show;
    }

    /// `/model` 选择器的候选清单:models.json 注册的自定义模型在前,其后是
    /// 各内置 provider 的默认模型(`showBuiltinModels: false` 时省略)。
    /// 自定义模型与所在 provider 的内置默认模型**并列**(如 models.json
    /// 覆盖 openai 代理模型时,openai 官方默认模型仍可选);全部候选均可用
    /// `resolve` 解析。
    pub fn available_models(&self) -> Vec<Model> {
        let mut models: Vec<Model> = self.extra_models.clone();
        if self.show_builtin_models {
            for provider in rpi_ai::builtin_providers() {
                if let Some(model_id) = default_model_for(provider) {
                    if let Ok(model) = self.resolve(&format!("{provider}/{model_id}")) {
                        if !models
                            .iter()
                            .any(|m| m.provider == model.provider && m.id == model.id)
                        {
                            models.push(model);
                        }
                    }
                }
            }
        }
        models
    }

    /// 记录 provider 级 override(models.json):覆盖内置端点 URL / 为自定义
    /// provider 提供 api+baseUrl / 为该 provider 的所有模型携带凭据与 headers。
    pub fn set_provider_override(
        &mut self,
        provider_id: &str,
        base_url: Option<String>,
        api: &str,
        api_key: Option<String>,
        headers: Option<HashMap<String, String>>,
    ) {
        self.provider_overrides.insert(
            provider_id.to_string(),
            ProviderOverride {
                base_url,
                api: api.to_string(),
                api_key,
                headers,
            },
        );
    }

    /// 解析 `provider/model`、`provider`(取默认模型)或已注册的自定义模型 id。
    pub fn resolve(&self, spec: &str) -> Result<Model, String> {
        let spec = spec.trim();
        if let Some(model) = self
            .extra_models
            .iter()
            .find(|m| m.id == spec || format!("{}/{}", m.provider, m.id) == spec)
        {
            return Ok(model.clone());
        }

        let (provider_id, model_id) = match spec.split_once('/') {
            Some((provider, model)) => (provider, Some(model)),
            None => (spec, None),
        };
        // provider 级 override(models.json)优先于内置端点表
        let (api, base_url) = match self.provider_overrides.get(provider_id) {
            Some(over) => {
                let base_url = match &over.base_url {
                    Some(url) => url.clone(),
                    None => default_provider_endpoint(provider_id)
                        .map(|(_, url)| url.to_string())
                        .unwrap_or_default(),
                };
                (over.api.clone(), base_url)
            }
            None => default_provider_endpoint(provider_id)
                .map(|(api, url)| (api.to_string(), url.to_string()))
                .ok_or_else(|| format!("unknown provider: {provider_id}"))?,
        };
        let model_id = match model_id {
            Some(model) => model.to_string(),
            None => default_model_for(provider_id)
                .map(|model| model.to_string())
                .or_else(|| {
                    // 自定义 provider(models.json models 列表):首个模型为默认
                    self.extra_models
                        .iter()
                        .find(|m| m.provider == provider_id)
                        .map(|m| m.id.clone())
                })
                .ok_or_else(|| format!("unknown provider `{provider_id}`: no default model"))?,
        };
        let mut model = Model::minimal(model_id, api, provider_id);
        model.base_url = base_url;
        // override 携带的凭据/headers 应用到内置默认模型(如仅覆盖 openai 的
        // 代理 URL 时,apiKey/headers 仍生效)
        if let Some(over) = self.provider_overrides.get(provider_id) {
            model.api_key.clone_from(&over.api_key);
            model.headers.clone_from(&over.headers);
        }
        Ok(model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_provider_model_spec() {
        let resolver = create_model_resolver();
        let model = resolver.resolve("anthropic/claude-opus-4-6").unwrap();
        assert_eq!(model.provider, "anthropic");
        assert_eq!(model.id, "claude-opus-4-6");
        assert_eq!(model.api, "anthropic-messages");
        assert_eq!(model.base_url, "https://api.anthropic.com");

        // 裸 provider → 默认模型
        let model = resolver.resolve("deepseek").unwrap();
        assert_eq!(model.id, "deepseek-chat");

        // 未知 provider
        assert!(resolver.resolve("nonexistent/m").is_err());
    }

    #[test]
    fn registered_custom_model_wins() {
        let mut resolver = create_model_resolver();
        let mut custom = Model::minimal("my-model", "openai-completions", "custom");
        custom.base_url = "http://localhost:8080/v1".into();
        resolver.register_model(custom);

        let model = resolver.resolve("custom/my-model").unwrap();
        assert_eq!(model.base_url, "http://localhost:8080/v1");
        let model = resolver.resolve("my-model").unwrap();
        assert_eq!(model.base_url, "http://localhost:8080/v1");
    }

    #[test]
    fn provider_override_applies_to_builtin_default_models() {
        let mut resolver = create_model_resolver();
        resolver.set_provider_override(
            "openai",
            Some("https://my-proxy.example.com/v1".into()),
            "openai-completions",
            Some("sk-override".into()),
            None,
        );
        // 不重定义 openai 模型,默认模型仍可解析,URL/凭据来自 override
        let model = resolver.resolve("openai").unwrap();
        assert_eq!(model.id, "gpt-4.1-mini");
        assert_eq!(model.base_url, "https://my-proxy.example.com/v1");
        assert_eq!(model.api_key.as_deref(), Some("sk-override"));
    }

    #[test]
    fn override_without_url_keeps_builtin_endpoint() {
        let mut resolver = create_model_resolver();
        resolver.set_provider_override("anthropic", None, "anthropic-messages", None, None);
        let model = resolver.resolve("anthropic/claude-sonnet-4-5").unwrap();
        assert_eq!(model.base_url, "https://api.anthropic.com");
    }

    #[test]
    fn available_models_lists_custom_then_builtin_defaults() {
        let mut resolver = create_model_resolver();
        let mut custom = Model::minimal("my-model", "openai-completions", "custom");
        custom.base_url = "http://localhost:8080/v1".into();
        resolver.register_model(custom);

        let models = resolver.available_models();
        assert_eq!(models.first().unwrap().id, "my-model", "自定义模型在前");
        // 自定义 provider 不应挤掉内置默认表;每个候选都可解析
        let specs: Vec<String> = models
            .iter()
            .map(|m| format!("{}/{}", m.provider, m.id))
            .collect();
        assert!(
            specs.contains(&"anthropic/claude-sonnet-4-5".to_string()),
            "{specs:?}"
        );
        assert!(specs.contains(&"zai/glm-4.7".to_string()), "{specs:?}");
        // 无重复候选(精确 provider+id 去重,自定义模型与内置默认并列)
        let mut sorted = specs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(specs.len(), sorted.len(), "{specs:?}");
        for spec in &specs {
            assert!(resolver.resolve(spec).is_ok(), "{spec} 应可解析");
        }
    }

    #[test]
    fn available_models_keeps_builtin_default_alongside_custom_override() {
        // models.json 给内置 provider 注册代理模型:官方默认模型仍应在列
        let mut resolver = create_model_resolver();
        let mut proxy = Model::minimal("my-proxy-model", "openai-completions", "openai");
        proxy.base_url = "http://localhost:8080/v1".into();
        resolver.register_model(proxy);

        let specs: Vec<String> = resolver
            .available_models()
            .iter()
            .map(|m| format!("{}/{}", m.provider, m.id))
            .collect();
        assert!(
            specs.contains(&"openai/my-proxy-model".to_string()),
            "{specs:?}"
        );
        assert!(
            specs.contains(&"openai/gpt-4.1-mini".to_string()),
            "{specs:?}"
        );
    }
}
