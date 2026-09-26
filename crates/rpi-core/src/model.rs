//! 模型解析(04 文档 §5 model-resolver 的 M4 子集):`provider/model` 字符串
//! → Model;未知 provider 报错;可注册自定义模型(scoped models 的轻量版)。

use rpi_ai::{default_provider_endpoint, Model};

#[derive(Default)]
pub struct ModelResolver {
    extra_models: Vec<Model>,
}

/// 工厂:空 resolver(内置 provider 端点表始终可用)。
pub fn create_model_resolver() -> ModelResolver {
    ModelResolver::default()
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

    /// 解析 `provider/model`、`provider`(取默认模型)或已注册的自定义模型 id。
    pub fn resolve(&self, spec: &str) -> Result<Model, String> {
        let spec = spec.trim();
        if let Some(model) = self.extra_models.iter().find(|m| m.id == spec || format!("{}/{}", m.provider, m.id) == spec) {
            return Ok(model.clone());
        }

        let (provider_id, model_id) = match spec.split_once('/') {
            Some((provider, model)) => (provider, Some(model)),
            None => (spec, None),
        };
        let model_id = match model_id {
            Some(model) => model.to_string(),
            None => default_model_for(provider_id)
                .ok_or_else(|| format!("unknown provider `{provider_id}`: no default model"))?
                .to_string(),
        };
        let (api, base_url) = default_provider_endpoint(provider_id)
            .ok_or_else(|| format!("unknown provider: {provider_id}"))?;
        let mut model = Model::minimal(model_id, api, provider_id);
        model.base_url = base_url.to_string();
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
}
