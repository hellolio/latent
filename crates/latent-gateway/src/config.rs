//! gateway.json 配置体系(对齐 OpenClaw `docs/gateway/configuration.md`):
//! 位置 = 项目 `.latent/gateway.json` / 全局 `<数据目录>/gateway.json`,
//! **项目逐字段覆盖全局**(对象递归合并,标量/数组整体替换)。校验规则:
//! 未知键/坏类型/非法值 → [`ConfigError`] → daemon 进程退出码 78。
//!
//! 凭据三来源(明文/`$ENV`/`!shell`)解析统一走 `latent-channel::credential`;
//! **`!shell` 来源仅全局配置接受** —— 项目级出现 `!shell` → 拒绝启动
//! (恶意 repo 可借 gateway 启动执行任意命令)。危险配置打 stderr 警告
//! (不阻断):`dmPolicy: open`、群 allowlist 为空、token 明文等。

use std::path::Path;

use serde::{Deserialize, Serialize};

/// 配置错误(校验失败/凭据缺失;daemon 据此退出码 78)。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("gateway.json 解析失败: {0}")]
    Parse(String),
    #[error("gateway.json 校验失败: {0}")]
    Validation(String),
    #[error("凭据解析失败: {0}")]
    Credential(String),
}

/// 非法配置的进程退出码(对齐上游)。
pub const EXIT_INVALID_CONFIG: i32 = 78;

// ---------------------------------------------------------------------------
// schema(全部 deny_unknown_fields:未知键拒绝启动)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct GatewayConfig {
    pub agents: AgentsConfig,
    pub gateway: GatewaySection,
    pub channels: ChannelsSection,
    /// 结构预留:(channel, accountId, peer) → agentId;MVP 全路由 main
    pub bindings: Vec<serde_json::Value>,
    pub session: SessionConfig,
    pub messages: MessagesConfig,
    pub commands: CommandsConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct AgentsConfig {
    pub defaults: AgentDefaults,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct AgentDefaults {
    /// 工作区目录;空 = gateway 进程 cwd
    pub workspace: String,
    /// 默认 provider/model spec;空 = 走 settings.json defaultProvider/defaultModel
    pub model: String,
    /// off|minimal|low|medium|high|xhigh|max;None = 不覆盖
    pub thinking_level: Option<String>,
    /// never|instant|thinking|message(OpenClaw 同名键)
    pub typing_mode: String,
    /// typing keepalive 间隔秒数(默认 6 = 3000ms 上游 keepalive 的秒表达;
    /// 实际刷新间隔 = 值 × 500ms,默认即 3000ms)
    pub typing_interval_seconds: u64,
}

impl Default for AgentDefaults {
    fn default() -> Self {
        AgentDefaults {
            workspace: String::new(),
            model: String::new(),
            thinking_level: None,
            typing_mode: "message".into(),
            typing_interval_seconds: 6,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct GatewaySection {
    pub port: u16,
    /// 默认 loopback;放开必须显式配置
    pub bind: String,
    pub auth: GatewayAuth,
}

impl Default for GatewaySection {
    fn default() -> Self {
        GatewaySection {
            port: 18789,
            bind: "127.0.0.1".into(),
            auth: GatewayAuth::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct GatewayAuth {
    pub mode: String,
    /// token 来源串(明文 / $ENV / !shell);必填,缺失 → 拒绝启动
    pub token: String,
}

impl Default for GatewayAuth {
    fn default() -> Self {
        GatewayAuth {
            mode: "token".into(),
            token: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct ChannelsSection {
    pub qq: Option<QqChannelConfig>,
    pub wecom: Option<WecomChannelConfig>,
    pub telegram: Option<TelegramChannelConfig>,
    /// 内存渠道(测试/开发;mock feature 未编译时工厂拒绝启动该渠道)
    pub mock: Option<MockChannelConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct MockChannelConfig {
    pub enabled: bool,
    pub dm_policy: String,
    pub allow_from: Vec<String>,
}

impl Default for MockChannelConfig {
    fn default() -> Self {
        MockChannelConfig {
            enabled: true,
            dm_policy: "open".into(),
            allow_from: Vec::new(),
        }
    }
}

/// 渠道级 DM 放行策略(`dmPolicy` 默认 pairing,fail-closed)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DmPolicy {
    /// 陌生人走配对码
    #[default]
    Pairing,
    /// 仅 allowFrom 名单
    Allowlist,
    /// 公开 —— 仅当 allowFrom 含 "*" 才真公开
    Open,
    /// 拒绝所有 DM
    Disabled,
}

impl DmPolicy {
    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        match value.trim() {
            "pairing" => Ok(DmPolicy::Pairing),
            "allowlist" => Ok(DmPolicy::Allowlist),
            "open" => Ok(DmPolicy::Open),
            "disabled" => Ok(DmPolicy::Disabled),
            other => Err(ConfigError::Validation(format!(
                "dmPolicy 非法值 `{other}`(pairing|allowlist|open|disabled)"
            ))),
        }
    }
}

/// 渠道公共面(各渠道配置内联同名字段)。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct QqChannelConfig {
    pub enabled: bool,
    /// 反向 WS 服务端监听地址(默认且强烈建议 loopback)
    pub reverse_ws_host: String,
    pub reverse_ws_port: u16,
    /// 必填;缺失/为空 → 该渠道拒绝启动(不是警告)
    pub access_token: String,
    pub text_chunk_limit: usize,
    pub dm_policy: String,
    pub allow_from: Vec<String>,
}

impl Default for QqChannelConfig {
    fn default() -> Self {
        QqChannelConfig {
            enabled: true,
            reverse_ws_host: "127.0.0.1".into(),
            reverse_ws_port: 3001,
            access_token: String::new(),
            text_chunk_limit: 2000,
            dm_policy: "pairing".into(),
            allow_from: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct WecomChannelConfig {
    pub enabled: bool,
    pub bot_id: String,
    /// 必填($ENV / !shell 来源);缺失 → 拒绝启动
    pub secret: String,
    pub text_chunk_limit: usize,
    pub dm_policy: String,
    pub allow_from: Vec<String>,
}

impl Default for WecomChannelConfig {
    fn default() -> Self {
        WecomChannelConfig {
            enabled: true,
            bot_id: String::new(),
            secret: String::new(),
            text_chunk_limit: 2048,
            dm_policy: "pairing".into(),
            allow_from: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct TelegramChannelConfig {
    pub enabled: bool,
    /// 必填($ENV 来源);缺失 → 拒绝启动
    pub bot_token: String,
    pub text_chunk_limit: usize,
    pub dm_policy: String,
    pub allow_from: Vec<String>,
}

impl Default for TelegramChannelConfig {
    fn default() -> Self {
        TelegramChannelConfig {
            enabled: true,
            bot_token: String::new(),
            text_chunk_limit: 4000,
            dm_policy: "pairing".into(),
            allow_from: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct SessionConfig {
    pub dm_scope: String,
    pub group_scope: String,
    pub reset: SessionResetConfig,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            dm_scope: "per-channel-peer".into(),
            group_scope: "per-group".into(),
            reset: SessionResetConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct SessionResetConfig {
    /// none|daily|idle(daily{atHour}/idle{idleMinutes} 阶段 5)
    pub mode: String,
    pub at_hour: Option<u8>,
    pub idle_minutes: Option<u64>,
}

impl Default for SessionResetConfig {
    fn default() -> Self {
        SessionResetConfig {
            mode: "none".into(),
            at_hour: None,
            idle_minutes: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct MessagesConfig {
    pub queue: QueueConfig,
    /// 群策略单一定义点:groupPolicy/groupAllowFrom/requireMention/
    /// mentionPatterns/unmentionedInbound 唯一在此;channels 节不设同名键
    pub group_chat: GroupChatConfig,
}

/// 队列模式(§4.6):steer|followup|collect|interrupt。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueMode {
    #[default]
    Steer,
    Followup,
    Collect,
    Interrupt,
}

impl QueueMode {
    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        match value.trim() {
            "steer" => Ok(QueueMode::Steer),
            "followup" => Ok(QueueMode::Followup),
            "collect" => Ok(QueueMode::Collect),
            "interrupt" => Ok(QueueMode::Interrupt),
            other => Err(ConfigError::Validation(format!(
                "queue mode 非法值 `{other}`(steer|followup|collect|interrupt)"
            ))),
        }
    }
}

/// cap 溢出策略:summarize|old|new。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DropPolicy {
    #[default]
    Summarize,
    Old,
    New,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct QueueConfig {
    pub mode: String,
    pub debounce_ms: u64,
    pub cap: usize,
    pub drop: String,
    /// { "qq": 1000 } 裸数字 = ms
    pub debounce_ms_by_channel: std::collections::HashMap<String, u64>,
}

impl Default for QueueConfig {
    fn default() -> Self {
        // 默认 steer / 500ms / cap 20 / summarize(§4.6 上游核实值)
        QueueConfig {
            mode: "steer".into(),
            debounce_ms: 500,
            cap: 20,
            drop: "summarize".into(),
            debounce_ms_by_channel: std::collections::HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct GroupChatConfig {
    pub require_mention: bool,
    /// open|disabled|allowlist(默认 allowlist,fail-closed)
    pub group_policy: String,
    /// "qq:12345" 形式(channel:群号)
    pub group_allow_from: Vec<String>,
    pub mention_patterns: Vec<String>,
    /// user_request|room_event(未 @ 消息处置;MVP 仅 user_request 语义,
    /// room_event 同样丢弃但不再打警告)
    pub unmentioned_inbound: String,
}

impl Default for GroupChatConfig {
    fn default() -> Self {
        GroupChatConfig {
            require_mention: true,
            group_policy: "allowlist".into(),
            group_allow_from: Vec::new(),
            mention_patterns: Vec::new(),
            unmentioned_inbound: "user_request".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct CommandsConfig {
    /// owner 身份 = "<channel>:<userId>"
    pub owner_allow_from: Vec<String>,
}

// ---------------------------------------------------------------------------
// 加载与合并
// ---------------------------------------------------------------------------

/// 读取 gateway.json:项目 `.latent/gateway.json` 逐字段覆盖全局
/// `<数据目录>/gateway.json`(对象递归合并,标量/数组整体替换)。
/// 返回 (合并后 Value, 项目级配置是否存在)。
pub fn load_config_value(cwd: Option<&Path>, data_dir: Option<&Path>) -> (serde_json::Value, bool) {
    let mut paths = Vec::new();
    if let Some(dir) = data_dir {
        paths.push((dir.join("gateway.json"), false));
    }
    if let Some(cwd) = cwd {
        paths.push((cwd.join(".latent/gateway.json"), true));
    }
    let mut merged = serde_json::Value::Null;
    let mut has_project = false;
    for (path, is_project) in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => {
                if is_project {
                    has_project = true;
                }
                merged = merge_values(merged, value);
            }
            Err(error) => return (serde_json::json!({ "__invalid": error.to_string() }), has_project),
        }
    }
    (merged, has_project)
}

/// 解析 + 校验(入口:daemon 启动)。
pub fn load_config(cwd: Option<&Path>, data_dir: Option<&Path>) -> Result<GatewayConfig, ConfigError> {
    let (value, has_project) = load_config_value(cwd, data_dir);
    if value.get("__invalid").is_some() {
        return Err(ConfigError::Parse(
            value["__invalid"].as_str().unwrap_or_default().to_string(),
        ));
    }
    if value.is_null() {
        // 无任何配置文件 = 默认配置(但 auth.token 必填会让空配置拒绝启动)
        return validate(&serde_json::to_value(GatewayConfig::default()).unwrap(), has_project);
    }
    validate(&value, has_project)
}

/// 项目级 `!shell` 凭据来源门:仅全局配置接受(§4.2)。
fn reject_project_shell_sources(value: &serde_json::Value) -> Result<(), ConfigError> {
    // 扫描所有可能是凭据的键
    let mut violations: Vec<String> = Vec::new();
    let sources = [
        ("channels.qq.accessToken", value.pointer("/channels/qq/accessToken")),
        ("channels.wecom.secret", value.pointer("/channels/wecom/secret")),
        ("channels.telegram.botToken", value.pointer("/channels/telegram/botToken")),
        ("gateway.auth.token", value.pointer("/gateway/auth/token")),
    ];
    for (label, node) in sources {
        if let Some(text) = node.and_then(|v| v.as_str()) {
            if latent_channel::credential::is_shell_source(Some(text)) {
                violations.push(label.to_string());
            }
        }
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::Validation(format!(
            "项目级 .latent/gateway.json 不允许 `!shell` 凭据来源(涉及: {});恶意仓库可借 gateway 启动执行任意命令。请改用 $ENV 或把该配置移入全局数据目录",
            violations.join(", ")
        )))
    }
}

/// 类型化解析 + 值校验 + 危险配置警告。
pub fn validate(value: &serde_json::Value, has_project: bool) -> Result<GatewayConfig, ConfigError> {
    if has_project {
        reject_project_shell_sources(value)?;
    }
    let config: GatewayConfig = serde_json::from_value(value.clone())
        .map_err(|error| ConfigError::Parse(normalize_serde_error(&error.to_string())))?;

    // 枚举值校验
    DmPolicy::parse(&config.channels.qq.as_ref().map(|c| c.dm_policy.clone()).unwrap_or_else(|| "pairing".into()))?;
    DmPolicy::parse(&config.channels.wecom.as_ref().map(|c| c.dm_policy.clone()).unwrap_or_else(|| "pairing".into()))?;
    DmPolicy::parse(&config.channels.telegram.as_ref().map(|c| c.dm_policy.clone()).unwrap_or_else(|| "pairing".into()))?;
    latent_gateway_queue_mode(&config.messages.queue.mode)?;
    if !matches!(config.messages.queue.drop.as_str(), "summarize" | "old" | "new") {
        return Err(ConfigError::Validation(format!(
            "queue drop 非法值 `{}`(summarize|old|new)",
            config.messages.queue.drop
        )));
    }
    if !matches!(
        config.session.dm_scope.as_str(),
        "main" | "per-peer" | "per-channel-peer" | "per-account-channel-peer"
    ) {
        return Err(ConfigError::Validation(format!(
            "session.dmScope 非法值 `{}`",
            config.session.dm_scope
        )));
    }
    if !matches!(config.session.group_scope.as_str(), "per-group" | "main") {
        return Err(ConfigError::Validation(format!(
            "session.groupScope 非法值 `{}`",
            config.session.group_scope
        )));
    }
    if !matches!(config.session.reset.mode.as_str(), "none" | "daily" | "idle") {
        return Err(ConfigError::Validation(format!(
            "session.reset.mode 非法值 `{}`",
            config.session.reset.mode
        )));
    }
    if !matches!(
        config.agents.defaults.typing_mode.as_str(),
        "never" | "instant" | "thinking" | "message"
    ) {
        return Err(ConfigError::Validation(format!(
            "agents.defaults.typingMode 非法值 `{}`",
            config.agents.defaults.typing_mode
        )));
    }
    if let Some(level) = &config.agents.defaults.thinking_level {
        if !matches!(
            level.as_str(),
            "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
        ) {
            return Err(ConfigError::Validation(format!(
                "agents.defaults.thinkingLevel 非法值 `{level}`(off|minimal|low|medium|high|xhigh|max)"
            )));
        }
    }
    if !matches!(
        config.messages.group_chat.group_policy.as_str(),
        "open" | "disabled" | "allowlist"
    ) {
        return Err(ConfigError::Validation(format!(
            "messages.groupChat.groupPolicy 非法值 `{}`",
            config.messages.group_chat.group_policy
        )));
    }
    if !matches!(
        config.messages.group_chat.unmentioned_inbound.as_str(),
        "user_request" | "room_event"
    ) {
        return Err(ConfigError::Validation(format!(
            "messages.groupChat.unmentionedInbound 非法值 `{}`",
            config.messages.group_chat.unmentioned_inbound
        )));
    }

    warn_dangerous_config(&config);
    Ok(config)
}

fn latent_gateway_queue_mode(mode: &str) -> Result<(), ConfigError> {
    QueueMode::parse(mode).map(|_| ())
}

/// serde 错误压缩为单行(保留字段路径与"unknown field"信息)。
fn normalize_serde_error(error: &str) -> String {
    error.lines().next().unwrap_or(error).to_string()
}

/// 危险配置 stderr 警告(不阻断)。
fn warn_dangerous_config(config: &GatewayConfig) {
    for (name, dm_policy, allow_from) in [
        (
            "qq",
            config.channels.qq.as_ref().map(|c| c.dm_policy.as_str()),
            config.channels.qq.as_ref().map(|c| &c.allow_from),
        ),
        (
            "wecom",
            config.channels.wecom.as_ref().map(|c| c.dm_policy.as_str()),
            config.channels.wecom.as_ref().map(|c| &c.allow_from),
        ),
        (
            "telegram",
            config.channels.telegram.as_ref().map(|c| c.dm_policy.as_str()),
            config.channels.telegram.as_ref().map(|c| &c.allow_from),
        ),
        (
            "mock",
            config.channels.mock.as_ref().map(|c| c.dm_policy.as_str()),
            config.channels.mock.as_ref().map(|c| &c.allow_from),
        ),
    ] {
        if dm_policy == Some("open") {
            eprintln!(
                "[latent-gateway][warn] channels.{name}.dmPolicy=open:任何人可私聊机器人(建议 pairing/allowlist)"
            );
        }
        if dm_policy == Some("allowlist") && allow_from.map(|list| list.is_empty()).unwrap_or(false) {
            eprintln!(
                "[latent-gateway][warn] channels.{name}.dmPolicy=allowlist 但 allowFrom 为空:所有私聊都会被拒绝"
            );
        }
    }
    if config.messages.group_chat.group_policy == "allowlist"
        && config.messages.group_chat.group_allow_from.is_empty()
    {
        eprintln!(
            "[latent-gateway][warn] groupPolicy=allowlist 但 groupAllowFrom 为空:所有群消息都会被丢弃"
        );
    }
    let token = &config.gateway.auth.token;
    if !token.is_empty() && !token.starts_with('$') && !token.starts_with('!') && !token.starts_with("$$") {
        eprintln!(
            "[latent-gateway][warn] gateway.auth.token 为明文:建议 $ENV 或 !shell 来源,避免凭据落盘"
        );
    }
}

/// 逐字段深合并:对象递归,标量/数组右侧整体覆盖。
pub fn merge_values(base: serde_json::Value, overlay: serde_json::Value) -> serde_json::Value {
    match (base, overlay) {
        (serde_json::Value::Object(mut base), serde_json::Value::Object(overlay)) => {
            for (key, value) in overlay {
                let merged = match base.remove(&key) {
                    Some(existing) => merge_values(existing, value),
                    None => value,
                };
                base.insert(key, merged);
            }
            serde_json::Value::Object(base)
        }
        (_, overlay) => overlay,
    }
}

/// 渠道凭据物化(daemon 启动时调用):解析 `accessToken`/`secret`/`botToken`
/// 三来源,结果**同时写回类型化配置与 raw Value** —— 渠道插件的最终配置以
/// raw 覆盖为准,只改类型化不写 raw 会让插件拿到未解析的 `$ENV` 字面量
/// (回归:曾致 telegram getMe 404)。凭据解析失败 = 该渠道禁用,不阻断 daemon。
pub async fn materialize_channel_credentials(
    mut config: GatewayConfig,
    raw_channels: &mut serde_json::Value,
) -> Result<GatewayConfig, String> {
    /// 把解析结果同步进 raw 的 `channels.<channel>` 节点(节点缺失时跳过
    /// —— 渠道未在 raw 中配置;调用方传入的是整份 gateway.json Value)。
    fn write_back(raw_config: &mut serde_json::Value, channel: &str, key: &str, value: &str) {
        if let Some(node) = raw_config
            .get_mut("channels")
            .and_then(|channels| channels.get_mut(channel))
            .and_then(|node| node.as_object_mut())
        {
            node.insert(key.to_string(), serde_json::Value::String(value.to_string()));
        }
    }

    if let Some(qq) = &mut config.channels.qq {
        if qq.enabled {
            if qq.access_token.trim().is_empty() {
                eprintln!(
                    "[latent-gateway] 渠道 qq 启动失败: accessToken 必填(缺失 → 渠道拒绝启动)"
                );
                qq.enabled = false;
            } else {
                match resolve_credential_scoped("channels.qq.accessToken", &qq.access_token, true, None).await {
                    Ok(token) => {
                        write_back(raw_channels, "qq", "accessToken", &token);
                        qq.access_token = token;
                    }
                    Err(error) => {
                        eprintln!("[latent-gateway] 渠道 qq 启动失败: {error}");
                        qq.enabled = false;
                    }
                }
            }
        }
    }
    if let Some(wecom) = &mut config.channels.wecom {
        if wecom.enabled {
            match resolve_credential_scoped("channels.wecom.secret", &wecom.secret, true, None).await {
                Ok(secret) => {
                    write_back(raw_channels, "wecom", "secret", &secret);
                    wecom.secret = secret;
                }
                Err(error) => {
                    eprintln!("[latent-gateway] 渠道 wecom 启动失败: {error}");
                    wecom.enabled = false;
                }
            }
        }
    }
    if let Some(telegram) = &mut config.channels.telegram {
        if telegram.enabled {
            match resolve_credential_scoped(
                "channels.telegram.botToken",
                &telegram.bot_token,
                true,
                None,
            )
            .await
            {
                Ok(token) => {
                    write_back(raw_channels, "telegram", "botToken", &token);
                    telegram.bot_token = token;
                }
                Err(error) => {
                    eprintln!("[latent-gateway] 渠道 telegram 启动失败: {error}");
                    telegram.enabled = false;
                }
            }
        }
    }
    Ok(config)
}

/// 凭据解析(带层级门):`allow_shell=false` 时拒绝 `!shell` 来源
/// (项目级配置;全局配置 true)。统一走 latent-channel::credential。
pub async fn resolve_credential_scoped(
    label: &str,
    source: &str,
    allow_shell: bool,
    env_fallback: Option<&str>,
) -> Result<String, ConfigError> {
    if latent_channel::credential::is_shell_source(Some(source)) && !allow_shell {
        return Err(ConfigError::Credential(format!(
            "{label}: 项目级配置不允许 !shell 来源"
        )));
    }
    let resolved = latent_channel::credential::resolve_credential(label, Some(source), env_fallback)
        .await
        .map_err(ConfigError::Credential)?;
    resolved.ok_or_else(|| ConfigError::Credential(format!("{label}: 未配置且环境变量为空")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_minimal_config_with_defaults() {
        let config: GatewayConfig = serde_json::from_value(json!({})).unwrap();
        assert_eq!(config.gateway.port, 18789);
        assert_eq!(config.gateway.bind, "127.0.0.1");
        assert_eq!(config.messages.queue.mode, "steer");
        assert_eq!(config.messages.queue.cap, 20);
        assert_eq!(config.session.dm_scope, "per-channel-peer");
        assert!(config.commands.owner_allow_from.is_empty());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let result: Result<GatewayConfig, _> = serde_json::from_value(json!({
            "gateway": { "port": 1, "bind": "127.0.0.1", "auth": { "mode": "token", "token": "x" }, "bogus": true }
        }));
        assert!(result.is_err(), "未知键应拒绝");
    }

    #[test]
    fn project_shell_sources_are_rejected() {
        let value = json!({
            "channels": { "qq": { "accessToken": "!cat /etc/passwd" } }
        });
        let error = validate(&value, true).unwrap_err();
        assert!(error.to_string().contains("!shell"), "{error}");
        // 全局配置允许 !shell(解析期不做凭据求值)
        let value = json!({
            "channels": { "qq": { "accessToken": "!cat /etc/passwd" } }
        });
        assert!(validate(&value, false).is_ok());
    }

    #[test]
    fn queue_mode_validation() {
        let mut config: GatewayConfig = serde_json::from_value(json!({
            "messages": { "queue": { "mode": "bogus" } }
        })).unwrap();
        // GatewayConfig 的 mode 是 String,校验在 validate()
        config.gateway.auth.token = "t".into();
        let value = serde_json::to_value(&config).unwrap();
        let error = validate(&value, false).unwrap_err();
        assert!(error.to_string().contains("queue mode"), "{error}");
    }

    #[test]
    fn merge_is_field_wise() {
        let base = json!({ "a": { "x": 1, "y": 2 }, "b": [1, 2] });
        let overlay = json!({ "a": { "y": 3 }, "b": [9] });
        let merged = merge_values(base, overlay);
        assert_eq!(merged["a"]["x"], 1);
        assert_eq!(merged["a"]["y"], 3);
        assert_eq!(merged["b"], json!([9]));
    }

    #[tokio::test]
    async fn materialize_resolves_credentials_into_typed_and_raw() {
        std::env::set_var("LAT_GW_TG_TOKEN_TEST", "real-token-123");
        let mut raw = json!({
            "channels": { "telegram": { "enabled": true, "botToken": "$LAT_GW_TG_TOKEN_TEST" } }
        });
        let config: GatewayConfig = serde_json::from_value(json!({
            "channels": { "telegram": { "enabled": true, "botToken": "$LAT_GW_TG_TOKEN_TEST" } }
        }))
        .unwrap();
        let config = materialize_channel_credentials(config, &mut raw).await.unwrap();
        // 类型化配置与 raw 必须同步为解析后的真实凭据
        // (回归:raw overlay 只覆盖类型化,曾致插件拿到 "$ENV" 字面量 → 404)
        assert_eq!(
            config.channels.telegram.as_ref().unwrap().bot_token,
            "real-token-123"
        );
        assert_eq!(
            raw.pointer("/channels/telegram/botToken"),
            Some(&json!("real-token-123"))
        );
    }

    #[tokio::test]
    async fn materialize_disables_channel_on_unresolvable_credential() {
        std::env::remove_var("LAT_GW_TG_MISSING_TEST");
        let mut raw = json!({
            "channels": { "telegram": { "enabled": true, "botToken": "$LAT_GW_TG_MISSING_TEST" } }
        });
        let config: GatewayConfig = serde_json::from_value(json!({
            "channels": { "telegram": { "enabled": true, "botToken": "$LAT_GW_TG_MISSING_TEST" } }
        }))
        .unwrap();
        let config = materialize_channel_credentials(config, &mut raw).await.unwrap();
        assert!(!config.channels.telegram.as_ref().unwrap().enabled);
    }

    #[test]
    fn dangerous_config_warnings_do_not_block() {
        // dmPolicy open + 空 allowlist + 明文 token:validate 仍成功
        let value = json!({
            "gateway": { "auth": { "token": "plain-token" } },
            "channels": { "telegram": { "dmPolicy": "open" } },
            "messages": { "groupChat": { "groupPolicy": "allowlist", "groupAllowFrom": [] } }
        });
        assert!(validate(&value, false).is_ok());
    }
}
