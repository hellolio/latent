//! Model / Provider / URL 配置体系(Pi 风格):`models.json` 声明
//! provider(baseUrl/api/apiKey/headers/compat/models),provider 级字段为
//! 默认值、model 级可覆盖;`settings.json` 提供 defaultProvider/defaultModel。
//!
//! 读取顺序:全局 `~/.latent/models.json` 为主,项目 `.latent/models.json` 覆盖
//! (同名 provider 按字段合并,model 按 id 合并,项目侧字段/条目优先)——
//! 覆盖内置 provider 的 baseUrl 时无需重定义其全部模型。
//!
//! apiKey 语义:值优先按环境变量名解析,未命中时当字面值使用(兼容 ollama
//! 等 `apiKey: "ollama"` 写法);真实 key 不应写入 models.json。

use std::collections::BTreeMap;
use std::path::Path;

use latent_ai::Model;

use crate::model::{create_model_resolver, ModelResolver};

// ---------------------------------------------------------------------------
// models.json 数据模型(serde camelCase,与 Pi 字段层级一致)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelsFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    show_builtin_models: Option<bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    providers: BTreeMap<String, ProviderConfig>,
    /// 保留未知顶层字段(/model 配置入口写回时不丢用户手写的其它配置)
    #[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
    extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compat: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    models: Vec<ModelConfig>,
    /// 保留未知 provider 级字段
    #[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
    extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelConfig {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compat: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sampling_params: Option<serde_json::Map<String, serde_json::Value>>,
    /// 保留未知 model 级字段
    #[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
    extra: BTreeMap<String, serde_json::Value>,
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
            None => match latent_ai::default_provider_endpoint(&provider_id) {
                Some((api, _)) => api.to_string(),
                None => {
                    eprintln!(
                        "[latent] models.json: provider `{provider_id}` 缺少 api 字段(且不是内置 provider),已跳过"
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
                    "[latent] models.json: provider `{provider_id}` 存在缺少 id 的 model,已跳过"
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

/// 从 项目 `.latent/models.json` + 全局 `~/.latent/models.json` 加载配置并构建
/// resolver(内置 provider 端点表始终可用)。文件缺失 = 空配置;解析失败 =
/// 诊断 + 跳过。顶层 `showBuiltinModels`(项目覆盖全局)控制 `/model`
/// 候选是否追加内置 provider 默认表。
pub fn create_model_resolver_from_config(
    project_dir: Option<&Path>,
    home: Option<&Path>,
) -> ModelResolver {
    let mut resolver = create_model_resolver();
    let mut providers = BTreeMap::new();
    let mut show_builtin_models = true;
    for path in models_config_paths(project_dir, home) {
        match parse_models_file(&path) {
            Ok(file) => {
                if let Some(flag) = file.show_builtin_models {
                    show_builtin_models = flag;
                }
                for (id, provider) in file.providers {
                    match providers.get_mut(&id) {
                        Some(existing) => merge_provider(existing, provider),
                        None => {
                            providers.insert(id, provider);
                        }
                    }
                }
            }
            Err(error) => eprintln!("[latent] models.json 解析失败:{error}"),
        }
    }
    register_config(&mut resolver, providers);
    resolver.set_show_builtin_models(show_builtin_models);
    resolver
}

/// models.json 候选路径:全局在前,项目在后(后读者覆盖先读者)。
fn models_config_paths(project_dir: Option<&Path>, home: Option<&Path>) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = home {
        paths.push(home.join(".latent/models.json"));
    }
    if let Some(project) = project_dir {
        paths.push(project.join(".latent/models.json"));
    }
    paths
}

// ---------------------------------------------------------------------------
// models.json 写回(/model 配置入口):读改写走同一数据模型,未知字段经
// serde(flatten) 保留
// ---------------------------------------------------------------------------

/// 读单个 models.json 为可写回的数据模型;文件不存在 = 空配置。
fn read_models_file(path: &Path) -> Result<ModelsFile, String> {
    parse_models_file(path)
}

/// 原子写回 models.json:先写同目录临时文件再 rename,失败不留半截文件。
fn save_models_file(path: &Path, file: &ModelsFile) -> Result<(), String> {
    let text = serde_json::to_string_pretty(file)
        .map_err(|e| format!("models.json 序列化失败:{e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// `/model` 交互式添加的一条记录:provider 已存在时 api/baseUrl/apiKey
/// 沿用配置原值(只追加 model);新 provider 三者必填 api。
#[derive(Debug, Clone)]
pub struct NewModelEntry {
    pub provider_id: String,
    pub api: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub model_id: String,
}

/// 把一条新模型 upsert 进 models.json 并原子写回:provider 按 id 查找,
/// 已存在则只追加 model(缺 api 等由 provider 级承担),不存在则要求 api
/// 字段并新建 provider;model 按 id 去重(重复 = 覆盖 name)。
pub fn upsert_models_json_entry(path: &Path, entry: &NewModelEntry) -> Result<(), String> {
    let mut file = read_models_file(path)?;
    let provider_id = entry.provider_id.trim();
    let model_id = entry.model_id.trim();
    if provider_id.is_empty() || model_id.is_empty() {
        return Err("provider 与 model id 不能为空".into());
    }
    let provider = match file.providers.get_mut(provider_id) {
        Some(existing) => {
            if existing.api.is_none() && entry.api.is_some() {
                existing.api = entry.api.clone();
            }
            existing
        }
        None => {
            let api = entry.api.as_deref().map(str::trim).filter(|s| !s.is_empty());
            let Some(api) = api else {
                return Err(format!("新 provider `{provider_id}` 需要指定 api 协议"));
            };
            let mut created = ProviderConfig {
                api: Some(api.to_string()),
                ..ProviderConfig::default()
            };
            if let Some(url) = entry.base_url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                created.base_url = Some(url.to_string());
            }
            if let Some(key) = entry.api_key_env.as_deref().map(str::trim).filter(|s| !s.is_empty())
            {
                created.api_key = Some(key.to_string());
            }
            file.providers.insert(provider_id.to_string(), created);
            file.providers.get_mut(provider_id).unwrap()
        }
    };
    match provider.models.iter_mut().find(|m| m.id == model_id) {
        Some(existing) => existing.name = Some(model_id.to_string()),
        None => provider.models.push(ModelConfig {
            id: model_id.to_string(),
            name: Some(model_id.to_string()),
            ..ModelConfig::default()
        }),
    }
    save_models_file(path, &file)
}

/// models.json 不存在时的初始模板:一个占位示例 provider,字段齐全、
/// 保存后即可解析(URL 是占位符,使用前必须改掉)。
pub fn write_models_template_if_absent(path: &Path) -> Result<bool, String> {
    if path.exists() {
        return Ok(false);
    }
    save_models_file(path, &default_models_template())?;
    Ok(true)
}

fn default_models_template() -> ModelsFile {
    let mut provider = ProviderConfig {
        base_url: Some("https://your-endpoint.example.com/v1".into()),
        api: Some("openai-completions".into()),
        api_key: Some("YOUR_API_ENV_VAR_NAME".into()),
        ..ProviderConfig::default()
    };
    provider.models.push(ModelConfig {
        id: "example-model".into(),
        name: Some("example-model".into()),
        ..ModelConfig::default()
    });
    let mut file = ModelsFile::default();
    file.providers.insert("example".into(), provider);
    file
}

/// `/model` 配置入口的写回目标:项目 `.latent/models.json` 存在则写它,
/// 否则全局 `~/.latent/models.json`;两者都缺 = 项目路径(由调用方创建)。
pub fn preferred_models_path(project_dir: Option<&Path>, home: Option<&Path>) -> std::path::PathBuf {
    if let Some(project) = project_dir {
        let path = project.join(".latent/models.json");
        if path.exists() {
            return path;
        }
        return path;
    }
    home.map(|home| home.join(".latent/models.json"))
        .unwrap_or_else(|| std::path::PathBuf::from(".latent/models.json"))
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
    /// TUI 渲染模式:`fullscreen`(alternate screen,输入区钉底、屏幕内
    /// 滚动)或 `regular`(终端原生 scrollback);缺省 fullscreen
    #[serde(default)]
    tui_mode: Option<String>,
    /// Ctrl+X 复制选区开关(有选区时 Ctrl+X = 复制);缺省开启
    #[serde(default)]
    ctrl_x_copy: Option<bool>,
    /// 选中后自动复制(选择/高亮/快捷键复制不受此开关影响);缺省关闭
    #[serde(default)]
    copy_on_select: Option<bool>,
}

/// 默认模型选择:项目 `.latent/settings.json` 优先于全局,首个非空 defaultProvider
/// /defaultModel 生效。返回 (provider, model),provider 可能缺失(此时
/// defaultModel 应为 `provider/model` 或已注册模型 id)。
pub fn load_default_model_selection(
    project_dir: Option<&Path>,
    home: Option<&Path>,
) -> (Option<String>, Option<String>) {
    let mut paths = Vec::new();
    if let Some(project) = project_dir {
        paths.push(project.join(".latent/settings.json"));
    }
    if let Some(home) = home {
        paths.push(home.join(".latent/settings.json"));
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

/// TUI 主题名:项目 `.latent/settings.json` 优先于全局,首个非空 `theme` 生效。
/// 只返回原始字符串;解析/降级由 latent-tui 的 `Theme::resolve` 负责。
pub fn load_theme_setting(project_dir: Option<&Path>, home: Option<&Path>) -> Option<String> {
    let mut paths = Vec::new();
    if let Some(project) = project_dir {
        paths.push(project.join(".latent/settings.json"));
    }
    if let Some(home) = home {
        paths.push(home.join(".latent/settings.json"));
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

/// TUI 渲染模式:项目 `.latent/settings.json` 优先于全局,首个非空 `tuiMode`
/// 生效。只返回原始字符串(`fullscreen` | `regular`);解析与缺省值
/// (fullscreen)由 interactive 装配负责。
pub fn load_tui_mode_setting(project_dir: Option<&Path>, home: Option<&Path>) -> Option<String> {
    load_setting_by(
        project_dir,
        home,
        |settings| settings.tui_mode.filter(|v| !v.trim().is_empty()),
    )
}

/// Ctrl+X 复制选区开关(`ctrlXCopy`)。
pub fn load_ctrl_x_copy_setting(project_dir: Option<&Path>, home: Option<&Path>) -> Option<bool> {
    load_setting_by(project_dir, home, |settings| settings.ctrl_x_copy)
}

/// 选中后自动复制开关(`copyOnSelect`)。
pub fn load_copy_on_select_setting(
    project_dir: Option<&Path>,
    home: Option<&Path>,
) -> Option<bool> {
    load_setting_by(project_dir, home, |settings| settings.copy_on_select)
}

fn load_setting_by<T>(
    project_dir: Option<&Path>,
    home: Option<&Path>,
    pick: impl Fn(SettingsDefaults) -> Option<T>,
) -> Option<T> {
    let mut paths = Vec::new();
    if let Some(project) = project_dir {
        paths.push(project.join(".latent/settings.json"));
    }
    if let Some(home) = home {
        paths.push(home.join(".latent/settings.json"));
    }
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(settings) = serde_json::from_str::<SettingsDefaults>(&text) else {
            continue;
        };
        if let Some(value) = pick(settings) {
            return Some(value);
        }
    }
    None
}

/// 把单个设置字段写入项目 `.latent/settings.json`(无项目目录时写全局;
/// 文件不存在则创建,已有字段与其他键保留)。`/setting` 的各项开关
/// 切换即写回,默认落项目级。
pub fn write_setting_field(
    project_dir: Option<&Path>,
    home: Option<&Path>,
    key: &str,
    value: serde_json::Value,
) -> Result<(), String> {
    let path = match project_dir {
        Some(dir) => dir.join(".latent/settings.json"),
        None => home
            .map(|home| home.join(".latent/settings.json"))
            .ok_or_else(|| "无法定位 settings.json(缺少项目目录与 HOME)".to_string())?,
    };
    let mut root = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or_else(|_| serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    };
    let Some(obj) = root.as_object_mut() else {
        return Err(format!("{} 顶层必须是 JSON 对象", path.display()));
    };
    obj.insert(key.to_string(), value);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建 {} 失败: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    std::fs::write(&path, text + "\n")
        .map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;
    Ok(())
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
                "latent_config_test_{}_{}",
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
            ".latent/models.json",
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
            ".latent/models.json",
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
            ".latent/models.json",
            r#"{ "providers": {
                "proxy": { "baseUrl": "http://global:1/v1", "api": "openai-completions",
                    "models": [{ "id": "m1", "contextWindow": 1000 }, { "id": "m2" }] }
            } }"#,
        );
        project.write(
            ".latent/models.json",
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
            ".latent/models.json",
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
            ".latent/models.json",
            r#"{ "providers": { "p1": { "baseUrl": "http://p1/v1", "api": "openai-completions",
                "apiKey": "LATENT_CONFIG_TEST_KEY", "models": [{ "id": "m" }] } } }"#,
        );
        std::env::set_var("LATENT_CONFIG_TEST_KEY", "env-value");
        let resolver = create_model_resolver_from_config(Some(&dir.0), None);
        let model = resolver.resolve("p1/m").unwrap();
        assert_eq!(model.api_key.as_deref(), Some("env-value"));
        std::env::remove_var("LATENT_CONFIG_TEST_KEY");
    }

    #[test]
    fn settings_default_model_selection() {
        let global = TempDir::new("sg");
        let project = TempDir::new("sp");
        global.write(".latent/settings.json", r#"{ "defaultProvider": "openai" }"#);
        let (provider, model) = load_default_model_selection(Some(&project.0), Some(&global.0));
        assert_eq!(provider.as_deref(), Some("openai"));
        assert_eq!(model, None);
        project.write(
            ".latent/settings.json",
            r#"{ "defaultProvider": "local", "defaultModel": "my-model" }"#,
        );
        let (provider, model) = load_default_model_selection(Some(&project.0), Some(&global.0));
        assert_eq!(provider.as_deref(), Some("local"));
        assert_eq!(model.as_deref(), Some("my-model"));
    }

    #[test]
    fn mouse_and_shortcut_settings_project_overrides_global() {
        let global = TempDir::new("cs_global");
        let project = TempDir::new("cs_project");
        assert_eq!(load_copy_on_select_setting(Some(&project.0), Some(&global.0)), None);
        assert_eq!(load_ctrl_x_copy_setting(Some(&project.0), Some(&global.0)), None);
        global.write(
            ".latent/settings.json",
            r#"{ "copyOnSelect": true, "ctrlXCopy": false }"#,
        );
        assert_eq!(
            load_copy_on_select_setting(Some(&project.0), Some(&global.0)),
            Some(true)
        );
        assert_eq!(
            load_ctrl_x_copy_setting(Some(&project.0), Some(&global.0)),
            Some(false)
        );
    }

    #[test]
    fn write_setting_field_merges_and_persists() {
        let home = TempDir::new("wf");
        // 写入新键:文件自动创建(无项目目录 → 全局 ~/.latent/settings.json,
        // /setting 的默认写回路径)
        write_setting_field(None, Some(&home.0), "tuiMode", serde_json::json!("regular")).unwrap();
        // 再写另一个键:已有键保留
        write_setting_field(None, Some(&home.0), "copyOnSelect", serde_json::json!(true)).unwrap();
        let text = std::fs::read_to_string(home.0.join(".latent/settings.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["tuiMode"], "regular");
        assert_eq!(value["copyOnSelect"], true);
        // 读回一致
        assert_eq!(load_tui_mode_setting(None, Some(&home.0)).as_deref(), Some("regular"));
        assert_eq!(load_copy_on_select_setting(None, Some(&home.0)), Some(true));
    }

    #[test]
    fn theme_setting_project_overrides_global() {
        let global = TempDir::new("tg");
        let project = TempDir::new("tp");
        // 无配置 → None
        assert_eq!(load_theme_setting(Some(&project.0), Some(&global.0)), None);
        global.write(".latent/settings.json", r#"{ "theme": "nord" }"#);
        assert_eq!(
            load_theme_setting(Some(&project.0), Some(&global.0)).as_deref(),
            Some("nord")
        );
        // 项目覆盖全局;空字符串视为未配置
        project.write(
            ".latent/settings.json",
            r#"{ "theme": "tokyo-night", "defaultModel": "m" }"#,
        );
        assert_eq!(
            load_theme_setting(Some(&project.0), Some(&global.0)).as_deref(),
            Some("tokyo-night")
        );
        project.write(".latent/settings.json", r#"{ "theme": "  " }"#);
        assert_eq!(
            load_theme_setting(Some(&project.0), Some(&global.0)).as_deref(),
            Some("nord")
        );
    }

    #[test]
    fn show_builtin_models_flag_controls_available_models() {
        let dir = TempDir::new("showflag");
        dir.write(
            ".latent/models.json",
            r#"{ "showBuiltinModels": false, "providers": {
                "local": { "baseUrl": "http://localhost:1/v1", "api": "openai-completions",
                    "models": [{ "id": "m1" }] }
            } }"#,
        );
        let resolver = create_model_resolver_from_config(Some(&dir.0), None);
        let specs: Vec<String> = resolver
            .available_models()
            .iter()
            .map(|m| format!("{}/{}", m.provider, m.id))
            .collect();
        assert_eq!(specs, vec!["local/m1".to_string()], "{specs:?}");
        // resolve 不受开关影响(内置默认模型仍可显式解析)
        assert!(resolver.resolve("anthropic/claude-sonnet-4-5").is_ok());

        // 缺省字段 = true;项目覆盖全局
        let global = TempDir::new("showflag_g");
        let project = TempDir::new("showflag_p");
        global.write(
            ".latent/models.json",
            r#"{ "showBuiltinModels": false, "providers": {} }"#,
        );
        project.write(
            ".latent/models.json",
            r#"{ "showBuiltinModels": true, "providers": {} }"#,
        );
        let resolver = create_model_resolver_from_config(Some(&project.0), Some(&global.0));
        assert!(
            !resolver.available_models().is_empty(),
            "项目 true 覆盖全局 false"
        );
    }

    #[test]
    fn save_models_file_round_trip_preserves_unknown_fields() {
        let dir = TempDir::new("save_rt");
        let path = dir.0.join(".latent/models.json");
        dir.write(
            ".latent/models.json",
            r#"{ "showBuiltinModels": false,
                "providers": { "p1": { "baseUrl": "http://p1/v1",
                    "api": "openai-completions", "customField": "keep-me",
                    "models": [{ "id": "m1", "modelField": 42 }] } } }"#,
        );
        let mut file = read_models_file(&path).unwrap();
        assert_eq!(file.show_builtin_models, Some(false));
        // 模拟表单添加:新 provider + 已有 provider 追加模型
        let provider = ProviderConfig {
            base_url: Some("http://p2/v1".into()),
            api: Some("anthropic-messages".into()),
            models: vec![ModelConfig {
                id: "m2".into(),
                name: Some("m2".into()),
                ..ModelConfig::default()
            }],
            ..ProviderConfig::default()
        };
        file.providers.insert("p2".into(), provider);
        file.providers.get_mut("p1").unwrap().models.push(ModelConfig {
            id: "m3".into(),
            ..ModelConfig::default()
        });
        save_models_file(&path, &file).unwrap();

        // 往返:新条目在,未知字段保留,原字段不丢
        let text = std::fs::read_to_string(&path).unwrap();
        let reloaded = read_models_file(&path).unwrap();
        assert_eq!(reloaded.show_builtin_models, Some(false));
        let p1 = reloaded.providers.get("p1").unwrap();
        assert_eq!(p1.base_url.as_deref(), Some("http://p1/v1"));
        assert_eq!(
            p1.extra.get("customField").and_then(|v| v.as_str()),
            Some("keep-me")
        );
        assert_eq!(p1.models.len(), 2, "{text}");
        assert_eq!(
            p1.models[0].extra.get("modelField").and_then(|v| v.as_u64()),
            Some(42)
        );
        assert_eq!(reloaded.providers.get("p2").unwrap().api.as_deref(), Some("anthropic-messages"));
    }

    #[test]
    fn upsert_models_json_entry_merges_and_validates() {
        let dir = TempDir::new("upsert");
        let path = dir.0.join(".latent/models.json");
        dir.write(
            ".latent/models.json",
            r#"{ "providers": { "p1": { "baseUrl": "http://p1/v1",
                "api": "openai-completions", "customField": "keep",
                "models": [{ "id": "m1" }] } } }"#,
        );
        // 新 provider 缺 api → 拒绝
        let err = upsert_models_json_entry(
            &path,
            &NewModelEntry {
                provider_id: "p2".into(),
                api: None,
                base_url: Some("http://p2/v1".into()),
                api_key_env: None,
                model_id: "m9".into(),
            },
        )
        .unwrap_err();
        assert!(err.contains("api"), "{err}");
        // 已有 provider:只追加 model,provider 字段不动
        upsert_models_json_entry(
            &path,
            &NewModelEntry {
                provider_id: "p1".into(),
                api: Some("anthropic-messages".into()),
                base_url: None,
                api_key_env: Some("P1_KEY_ENV".into()),
                model_id: "m2".into(),
            },
        )
        .unwrap();
        // 新 provider 全字段
        upsert_models_json_entry(
            &path,
            &NewModelEntry {
                provider_id: "p2".into(),
                api: Some("anthropic-messages".into()),
                base_url: Some("http://p2/v1".into()),
                api_key_env: Some("P2_KEY_ENV".into()),
                model_id: "m9".into(),
            },
        )
        .unwrap();
        let resolver = create_model_resolver_from_config(Some(&dir.0), None);
        let m2 = resolver.resolve("p1/m2").unwrap();
        assert_eq!(m2.api, "openai-completions", "已有 provider 沿用原 api");
        assert_eq!(m2.base_url, "http://p1/v1");
        let m9 = resolver.resolve("p2/m9").unwrap();
        assert_eq!(m9.api, "anthropic-messages");
        assert_eq!(m9.base_url, "http://p2/v1");
        assert_eq!(m9.api_key.as_deref(), Some("P2_KEY_ENV"));
        // 未知字段保留
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("customField"), "{text}");
        // 幂等:重复 model id 只覆盖 name 不重复追加
        upsert_models_json_entry(
            &path,
            &NewModelEntry {
                provider_id: "p1".into(),
                api: None,
                base_url: None,
                api_key_env: None,
                model_id: "m2".into(),
            },
        )
        .unwrap();
        let reloaded = read_models_file(&path).unwrap();
        assert_eq!(reloaded.providers.get("p1").unwrap().models.len(), 2);
    }

    #[test]
    fn models_template_parses_and_resolves() {
        let dir = TempDir::new("template");
        let path = dir.0.join(".latent/models.json");
        save_models_file(&path, &default_models_template()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        serde_json::from_str::<serde_json::Value>(&text).unwrap();
        let resolver = create_model_resolver_from_config(Some(&dir.0), None);
        let model = resolver.resolve("example/example-model").unwrap();
        assert_eq!(model.api, "openai-completions");
        assert!(resolver.available_models().len() > 1, "缺省显示内置默认表");
    }

    #[test]
    fn preferred_models_path_prefers_project_dir() {
        let global = TempDir::new("pref_g");
        let project = TempDir::new("pref_p");
        // 提供项目目录 → 恒写项目路径(存在与否一致,避免歧义)
        assert_eq!(
            preferred_models_path(Some(&project.0), Some(&global.0)),
            project.0.join(".latent/models.json")
        );
        project.write(".latent/models.json", r#"{ "providers": {} }"#);
        assert_eq!(
            preferred_models_path(Some(&project.0), Some(&global.0)),
            project.0.join(".latent/models.json")
        );
        // 无项目目录 → 全局
        assert_eq!(
            preferred_models_path(None, Some(&global.0)),
            global.0.join(".latent/models.json")
        );
    }
}
