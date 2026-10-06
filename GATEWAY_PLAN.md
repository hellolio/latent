# latent-gateway 开发计划（OpenClaw 架构级复刻）

> 状态：**待开发**。本文档是自包含的开发指南：目标读者是将独立实施本计划的 agent / 工程师，无需本计划的产生过程上下文。
> 写作日期：2026-10-07。上游基线：**OpenClaw main @ `6693bb96`（package version `2026.9.8`，核实于 2026-10-06）**。

---

## §0 背景与上游阅读指南（动手前必读）

**latent 是什么**：本仓库是 [earendil-works/pi](https://github.com/earendil-works/pi) 的 Rust 重写——终端 AI 编码 agent，流式驱动 LLM、并行执行工具、append-only 会话树、MCP 扩展、权限/沙箱。仓库规范见根目录 `AGENTS.md`（**硬门槛：`cargo test --workspace` 全绿 + `cargo clippy --workspace --all-targets` 零警告**）。

**OpenClaw 是什么**：<https://github.com/openclaw/openclaw>（MIT 协议），目前最流行的开源"个人 AI 助手网关"（🦞 龙虾吉祥物）：单个常驻 Gateway 进程拥有所有聊天渠道连接（WhatsApp/Telegram/Slack…）与所有 agent 会话，并暴露一套 WebSocket 控制面。**它的 agent 底层原本就是 pi**——与本仓库同源。官方文档：<https://docs.openclaw.ai>。

**总原则（本计划最高优先级指令）**：

> **行为语义、常量、协议格式、边界情况，一律先查 OpenClaw 对应源文件再动手。** 本文档 §6 给出了每个模块"实现前必读的 OpenClaw 源文件路径"和已核实的关键常量。OpenClaw 已经踩过并解决了大量实现细节的坑（去重时序、防抖窗口语义、保序投递、echo 关联、分段断点……），直接继承这些决策可以省去大量调试。**不要凭直觉自创语义；与 OpenClaw 不一致处必须在 §8 偏离记录中登记。**

注意事项：

- OpenClaw 是 TypeScript 项目，我们是 Rust 重写：**翻译语义，不翻译代码形态**。Rust 侧写法遵循 `AGENTS.md` 的工程规范（§9）。
- 上游仓库持续演进，本文引用的路径可能漂移（尤其 `src/agents/`、`src/gateway/` 正在重构）。路径 404 时**按文中给出的标识符关键词搜索**（如 `DEFAULT_CHUNK_LIMIT`、`buildAgentPeerSessionKey`、`createInboundDebouncer`）。
- 本文已核实的常量（写入 §2/§4/§6）以基线版本为准；若上游值已变，以上游新值为准并更新本文。

**一条消息的旅程（QQ 群示例，帮助建立整体直觉）**：

张三在 QQ 群 12345 发「@机器人 帮我看看 src/login.rs 为什么报 401」：

1. NapCat（外部程序，持有 QQ 登录态）把 OneBot 11 JSON 推进 gateway 的反向 WS 端点；
2. `latent-channel/qq` 把它翻译成统一 `InboundMessage`（`to_me: true`——At 段等于机器人 QQ 号），经去重/@判定；
3. 消息经 mpsc 通道进入 `latent-gateway`，`routing/session_key` 算出 `agent:main:qq:group:12345`；
4. `auto_reply` 管线：不是命令 → 防抖合并窗口 → 会话空闲则执行（忙则按队列模式 steer/followup/collect/interrupt 处置）→ `envelope` 包装为 `[qq group:12345 张三 14:32] 帮我看看…`；
5. 首次触发 `latent-runtime` 的 `build_session` 装配出该群专属的 `AgentSession`（工具/权限/沙箱/落盘——既有代码，不知道 QQ 的存在）；
6. agent 跑工具触发审批 → `ChatApprovalUi` 发审批消息回 owner，owner 回 `/approve <id> allow-once` → 命令拦截层路由回 oneshot → agent 继续；
7. 最终回复经 `chunk` 分段 → `ChannelSender.send()` → qq 模块调 `send_group_msg` → NapCat → 群里。

---

## §1 Crate 划分与依赖方向（严格单向）

```
L1  latent-channel      # 纯聊天域，零内部依赖：不 import 任何 latent-* crate
L2  latent-runtime      # 装配层：依赖 latent-ai/agent/session/tools/web/sandbox/core
L3  latent-gateway      # 引擎 + bin：依赖 latent-runtime + latent-channel
```

- 依赖规则：`latent-gateway → {latent-runtime, latent-channel}`；`latent-channel` 与 `latent-runtime` **互不依赖**；`latent-cli` **永不依赖** `latent-channel/latent-gateway`；渠道 crate 之间互不可见。
- 渠道适配器全部在 `latent-channel` 内部，作为 **feature 门控模块**（详见 §2）——对齐 OpenClaw 的 `extensions/<name>/` 同属主包的组织方式。
- **编译隔离验收（每阶段必跑）**：

```bash
cargo tree -p latent-channel -i latent-core   # 输出为空：channel 不认识 agent
cargo tree -p latent        -i latent-channel # 输出为空：CLI 不含聊天栈
cargo tree -p latent-gateway -i latent-tui    # 输出为空：gateway 不含 TUI
```

---

## §2 latent-channel 规格（纯聊天域）

### 2.1 文件树

```
crates/latent-channel/
  Cargo.toml
  src/
    lib.rs               # pub mod 声明；渠道模块挂 #[cfg(feature)]
    types.rs             # 统一消息模型（全部 pub 类型，字段见 2.3）
    plugin.rs            # ChannelPlugin trait + ChannelHandle/ChannelSender
    typing.rs            # typing 指示器生命周期
    mention_gating.rs    # @机器人判定（纯函数）
    debounce.rs          # 入站防抖合并
    chunk.rs             # 出站长文分段
    error.rs             # ChannelError（thiserror）
    telegram/            # #[cfg(feature = "telegram")]
    qq/                  # #[cfg(feature = "qq")]
    wecom/               # #[cfg(feature = "wecom")]
    mock/                # #[cfg(feature = "mock")] 测试假渠道
```

### 2.2 Cargo.toml（feature 门控——这是"单 crate 多渠道、按需编译"的机制）

```toml
[package]
name = "latent-channel"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true

[features]
default = []
mock     = []
telegram = ["dep:reqwest"]
qq       = ["dep:axum", "dep:tokio-tungstenite"]
wecom    = ["dep:tokio-tungstenite"]

[dependencies]
latent-channel 无任何 latent-* 依赖——这条是硬规则
async-trait.workspace = true
futures.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tokio.workspace = true
tokio-util.workspace = true
regex.workspace = true                      # mentionPatterns
reqwest = { workspace = true, optional = true }
axum = { workspace = true, optional = true }
tokio-tungstenite = { workspace = true, optional = true }
```

`lib.rs` 门控：

```rust
pub mod chunk;
pub mod debounce;
pub mod error;
pub mod mention_gating;
pub mod plugin;
pub mod typing;
pub mod types;

#[cfg(feature = "telegram")] pub mod telegram;
#[cfg(feature = "qq")]       pub mod qq;
#[cfg(feature = "wecom")]    pub mod wecom;
#[cfg(feature = "mock")]     pub mod mock;
```

> 已知坑：`cargo build --workspace` 会启用所有 feature（Cargo feature unification），三渠道都会编译。按版本出二进制走 `cargo build -p latent-gateway --no-default-features --features qq`。

### 2.3 types.rs（字段级定义）

```rust
pub enum ChatType { Private, Group }

/// 会话/聊天标识（跨渠道统一）。
pub struct ChatRef {
    pub platform: &'static str,     // "qq" | "wecom" | "telegram"
    pub chat_type: ChatType,
    pub conversation_id: String,    // 群号 / 对方 user_id（私聊）
}

pub struct Sender {
    pub user_id: String,
    pub display_name: String,       // 昵称或群名片
}

/// 消息段（收发共用，OneBot 12 / Koishi 风格）。
pub enum Segment {
    Text(String),
    At { user_id: String },         // "all" = @全体（平台支持时）
    Image { url: Option<String>, file_id: Option<String> },
    File { url: Option<String>, file_id: Option<String>, name: Option<String> },
    Reply { message_id: String },   // 引用回复
}

pub struct InboundMessage {
    pub platform: &'static str,
    pub chat: ChatRef,
    pub sender: Sender,
    pub message_id: String,         // 渠道侧去重键（OneBot message_id；TG 用 update_id 推导）
    pub segments: Vec<Segment>,
    pub text: String,               // 纯文本投影（Text 拼接、At→"@名字"、Reply→"[回复]"）
    pub to_me: bool,                // @机器人 / 回复机器人 / 私聊——渠道归一化时算好
    pub reply_to_me: bool,
    pub raw: serde_json::Value,     // 原始载荷透传（诊断/扩展用）
}

pub struct OutboundMessage {
    pub segments: Vec<Segment>,
    pub reply_to: Option<String>,   // 引用的 message_id（平台不支持则忽略）
}

pub enum ChannelEvent {
    Inbound(InboundMessage),
    Status(ChannelStatus),
}

pub enum ChannelStatus {
    Connected { account_id: String },
    Disconnected { reason: String },   // 可自动重连
    Failed { reason: String },         // 需宿主介入（如凭据失效）
}

/// 出站失败分类（对齐 OpenClaw ReplyMediaFailure 的语义面）。
pub enum ChannelError {
    ChatNotFound, NotInGroup, RateLimited { retry_after_ms: u64 },
    Unsupported, DeliveryFailed(String),
}
```

### 2.4 plugin.rs（渠道接口）

```rust
#[async_trait]
pub trait ChannelPlugin: Send + Sync {
    /// 渠道 id："qq" | "wecom" | "telegram"（== config.rs channels 节的键名）
    fn id(&self) -> &'static str;

    /// 启动渠道：自持连接/重连/登录态，入站事件推 tx。
    /// **panic 边界（硬要求）**：实现内部必须捕获自身任务 panic（tokio::spawn
    /// 的 JoinError 或 catch_unwind），降级为 Status::Failed 发给宿主，
    /// 绝不允许 panic 击穿 gateway 进程——对齐仓库"错误吞掉+诊断"原则。
    async fn start(&self, tx: mpsc::Sender<ChannelEvent>) -> Result<ChannelHandle, String>;

    /// 渠道配置注入（gateway 按渠道 id 分发原始 JSON，各渠道自己反序列化自己的
    /// Config struct——gateway 不认识任何具体渠道）。
    fn apply_config(&self, raw: &serde_json::Value) -> Result<(), String>;
}

#[derive(Clone)]
pub struct ChannelHandle { sender: ChannelSender }

impl ChannelHandle {
    pub async fn send(&self, chat: &ChatRef, message: OutboundMessage) -> Result<(), ChannelError>;
    /// typing 指示器；平台不支持时 no-op（对齐 OpenClaw：不支持渠道静默抑制）
    pub async fn typing(&self, chat: &ChatRef, on: bool);
    pub async fn status(&self) -> ChannelStatus;
}
```

内部结构建议：`ChannelSender` 持 `mpsc::Sender<ChannelCommand>`，渠道 `start` 任务里 select 消费命令（Send/Typing/Shutdown）与平台事件——出站串行化天然获得，保序由 gateway 侧 reply_dispatcher 保证（见 §4.5）。

### 2.5 宿主层机制（精确参数，全部已对照 OpenClaw 核实）

**typing.rs**（上游：`src/channels/typing.ts`、`typing-lifecycle.ts`）

- typingMode：`never | instant | thinking | message`（配置 `agents.defaults.typingMode`，默认 `message`——首个用户可见回复活动才发；DM 与被 @ 的群聊即时发）。
- keepalive 刷新间隔 **3000ms**；单次 typing 最长 **60000ms（TTL，超时告警并停止）**；连续失败 **2 次**即停。
- **没有"typing 完成才发送"的门**——typing 只是指示器，入队即发（上游文档明确澄清过，不要发明 gate）。

**mention_gating.rs**（上游：`src/channels/mention-gating.ts`、`src/auto-reply/reply/mentions.ts`、docs `channels/groups.md`）

- 输入：InboundMessage + 配置（requireMention、mentionPatterns、机器人自身账号 id 列表）。
- 判定规则：私聊恒 `to_me`；群聊需显式 At 段 == 机器人 id，**或** reply 引用的消息来自机器人，**或** 文本命中 mentionPatterns（大小写不敏感正则，优先级：agent 级 > `messages.groupChat.mentionPatterns` > 由机器人 identity.name 派生）。
- 纯函数：`resolve_mention(msg, cfg) -> MentionDecision { ToMe, RoomContext }`。

**debounce.rs**（上游：`src/auto-reply/inbound-debounce.ts`——语义逐条对照过）

- 按 `(conversation_id, user_id)` 缓冲合并连发消息；默认窗口 **0ms**（即默认不合并），配置优先级：per-channel 覆盖（`messages.queue.debounceMsByChannel`）> 全局 `messages.queue.debounceMs`。
- **窗口在首条到达时固定，后续消息不得顺延**；最大等待 = `debounceMs × 5`（`MAX_DEBOUNCE_WINDOW_MULTIPLIER`）。
- 跟踪键上限 **2048**（`DEFAULT_MAX_TRACKED_KEYS`），超限丢最旧。
- 命令消息（`/` 开头）与带 At 的消息**立即冲刷缓冲不等待**（对齐 `shouldDebounceTextInbound` 只防抖纯文本）。
- flush 产出合并文本（段间 `\n`）。

**chunk.rs**（上游：`src/auto-reply/chunk.ts`——常量逐个核实）

- `DEFAULT_CHUNK_LIMIT = 4000`；per-channel 覆盖（`channels.<id>.textChunkLimit`，账号级优先于渠道级）。QQ 建议配 2000（NTQQ 保守值）、企微 2048 字节、TG 4000。
- mode：`length`（默认，硬切但优先**括号感知断点**——先窗口内括号外的换行，再最后一个空白）/ `newline`（按空行段落 `/\n[\t ]*\n+/` 打包）。
- markdown 版：切割处**闭合 code fence 并在续块重开**（``` 配对不破坏）。

### 2.6 渠道实现要点（详细协议见 §6 对照表）

- **telegram/**（feature `telegram`）：Bot API **getUpdates 长轮询**（不引 SDK，reqwest 直连；OpenClaw 的 telegram 同样默认长轮询）。配置 `botToken`（`$ENV` 来源）、`textChunkLimit`（默认 4000）、`dmPolicy`、`textChunkLimit` 等。发送 `sendMessage`，`reply_to_message_id` 实现 Reply 段。typing 用 `sendChatAction`。
- **qq/**（feature `qq`）：**OneBot 11 协议 + 反向 WebSocket**（NapCat 是 WS **客户端**，gateway 是服务端——gateway 无需公网 IP）。axum 起 WS 端点，`Authorization: Bearer <accessToken>` 校验；事件按 `post_type` 分发，action 请求带 `echo` 字段 + oneshot 表关联响应；`meta_event.heartbeat` 判活。收：`message.private` / `message.group`（text/at/face/reply/image 段）；发：`send_private_msg` / `send_group_msg`。
- **wecom/**（feature `wecom`）：**企业微信智能机器人 WS 长连接**（官方文档 `developer.work.weixin.qq.com/document/path/101463`）。BotId + Secret 换 ticket → wss 主动外连（无需公网 IP/域名备案）。实现前**必须逐字读官方文档**核对握手/回调/回复协议——本计划未核实到字节级。
- **mock/**（feature `mock`）：内存版 ChannelPlugin——`start` 返回可编程 handle（测试注入 InboundMessage、断言 Outbound），gateway 集成测试的基础设施。

---

## §3 latent-runtime 规格（装配层下沉，行为不变）

### 3.1 迁移清单

新建 `crates/latent-runtime`，把以下内容从 latent-cli 迁出（latent-cli 侧 `pub use latent_runtime::...` re-export 保持现有路径兼容，E2E 与测试不破）：

| 迁移项 | 现位置 | 说明 |
|---|---|---|
| `assembly.rs` **全量**（含 `#[cfg(test)]`） | `crates/latent-cli/src/assembly.rs`（2393 行） | 纯业务核装配，import 只涉及 latent-* 与 std/tokio/futures/serde |
| `resolve_provider_and_model` | `main.rs:437` | provider/models.json 解析 |
| `resolve_session_store` | `main.rs:348` | `-c`/`-r` 会话存储解析 |
| `print_session_list` | `main.rs:387` | `-l` 列表 |
| `notify_legacy_data_dir` + `dirs_home` | `main.rs:199/486` | 数据目录提示（`dirs_home` 与 assembly.rs:119 重复，合一） |
| `session_event_to_json` + `agent_event_to_json` | `modes/mod.rs:18,66` | json/rpc 共用事件映射 |
| slash 解析层 | `modes/slash.rs` | `COMMANDS`/`SlashAction`/`SlashInput`/`parse`/`help_markdown`/`MODE_VARIANTS`/`SESSION_VARIANTS` + tests 迁往 `latent-runtime/src/slash.rs`；**`popup_entries()`（返回 `latent_tui::CommandEntry`）留在 latent-cli**（interactive 专用，见 modes/slash.rs 瘦身后 re-export） |
| landlock helper 分发 | `main.rs:66-68,216-217` | 抽 `maybe_dispatch_landlock_helper(args: &[String]) -> bool`（内部处理 `latent_sandbox::landlock::HELPER_FLAG` 分支），**两个 bin 的 main 开头都必须调用**，否则 Linux 沙箱静默失效 |

### 3.2 解除 `RpcApprovalUi` 耦合（唯一抽取障碍）

`assembly.rs` 全文件仅 2 处 `crate::` 引用，都是 `crate::modes::rpc::RpcApprovalUi`：

- **`assembly.rs:347`**（`BuiltSession.rpc_approval` 字段）与 **`assembly.rs:520`**（`BuildOptions.rpc_approval` 字段）。

改法（三处联动）：

1. 从 `BuiltSession` / `BuildOptions` **删除** `rpc_approval` 字段；`build_session` 的解构（:920 附近）与 `Ok(BuiltSession {..})`（:1348 附近）、`run_session` 的 `rpc_approval: None`（:1403）同步删除。
2. `modes/print_mode.rs` 的 `build_bare_session(..., rpc_approval: Option<Arc<crate::modes::rpc::RpcApprovalUi>>, ...)`（:47）删掉该参数。
3. `modes/rpc.rs` 的 `run_rpc_mode(built, reader, writer)`（:273）**增加参数** `approval: Arc<RpcApprovalUi>`（原实现从 `built.rpc_approval` 取，:278-283）。
4. `main.rs` RPC 分支（:296-317）：`build_bare_session(..., approval_ui = Some(rpc_approval.clone()), ...)`，然后 `run_rpc_mode(built, rpc_approval, stdin, writer)`。

> **顺带修复一个现存缺陷**：当前 main.rs RPC 分支传 `approval_ui = None`，而 `build_session` 对 None 的兜底是 `HeadlessApprovalUi(Deny)`（assembly.rs:971）——即 Confirm 模式下 RPC 会话的工具审批被静默自动拒绝，`RpcApprovalUi` 从未参与审批决策（只做应答路由；`tests/modes.rs:979` 的回路测试是直接调 trait 方法，没覆盖装配链路）。改动 4 同时修复此问题：approval_ui 与路由句柄指向同一实例。**修复后补一个装配链路的审批回路集成测试**（build_bare_session → ApprovalUi.request_approval 收到上行 → resolve → 决策生效）。

### 3.3 根 Cargo.toml 变更（阶段 1 一起做）

```toml
[workspace]
resolver = "2"
default-members = ["crates/latent-cli"]        # 裸 cargo build 只编 CLI；--workspace 仍全量
members = [
    # …现有 9 个…
    "crates/latent-runtime",
    "crates/latent-channel",
    "crates/latent-gateway",
]

[workspace.dependencies]
latent-runtime = { path = "crates/latent-runtime", version = "0.1.0" }
latent-channel = { path = "crates/latent-channel", version = "0.1.0" }
latent-gateway = { path = "crates/latent-gateway", version = "0.1.0" }

tokio = { version = "1", features = ["macros", "rt-multi-thread", "sync", "time", "io-util", "process", "net"] }  # 补 net
# 聊天网关所需（钉精确版本；axum 取 crates.io 最新稳定 0.8.x 精确 pin）
tokio-tungstenite = "=0.30.0"
axum = "=0.8.7"        # 实施时核对最新精确版本后钉死
```

---

## §4 latent-gateway 规格（引擎 + bin）

### 4.1 文件树

```
crates/latent-gateway/
  Cargo.toml             # [[bin]] latent-gateway；features 转发（见下）
  src/
    main.rs              # 默认跑 daemon；子命令：pairing list/approve、channels status [--probe]、status
    config.rs            # gateway.json schema + 校验（非法配置退出码 78）
    routing/
      session_key.rs     # session key 计算
    agents.rs            # ChatSessionRegistry：每 session key 一个 BuiltSession（惰性创建）
    auto_reply/
      mod.rs             # dispatch_inbound_message 管线
      queue.rs           # 四模式队列
      envelope.rs        # 入站消息包装
      commands.rs        # 聊天命令
      reply_dispatcher.rs# 出站保序
    approval.rs          # ChatApprovalUi
    pairing.rs           # DM 配对
    state.rs             # <数据目录>/gateway/state.json
    gateway/             # WS 控制面（阶段 4）
      server.rs auth.rs methods/ events.rs
```

```toml
[features]
default = ["qq", "wecom", "telegram"]
qq       = ["latent-channel/qq"]
wecom    = ["latent-channel/wecom"]
telegram = ["latent-channel/telegram"]
```

### 4.2 config.rs（gateway.json）

位置：项目 `.latent/gateway.json` / 全局 `<数据目录>/gateway.json`，**项目逐字段覆盖全局**（复用 settings.json 的合并语义；参考 `latent-runtime` 迁移后的 `read_settings_files` 模式）。下面示例中的 `//` 注释仅为说明，实际 serde JSON 不含注释：

```jsonc
{
  "agents": {
    "defaults": {
      "workspace": "",              // 工作区目录；空 = gateway 进程 cwd
      "model": "",                  // 默认 provider/model spec；空 = 走 settings.json defaultProvider/defaultModel
      "thinkingLevel": null,        // off|minimal|low|medium|high|xhigh|max
      "typingMode": "message",      // never|instant|thinking|message（OpenClaw 同名键）
      "typingIntervalSeconds": 6
    }
  },
  "gateway": {
    "port": 18789,
    "bind": "127.0.0.1",            // 默认 loopback；放开必须显式配置
    "auth": { "mode": "token", "token": "$LATENT_GATEWAY_TOKEN" }  // 未配置 → 拒绝启动（§5）
  },
  "channels": {
    "qq": {
      "enabled": true,
      "reverseWsHost": "127.0.0.1", // 默认且强烈建议 loopback
      "reverseWsPort": 3001,
      "accessToken": "$NAPCAT_TOKEN", // 必填；缺失/为空 → 该渠道拒绝启动
      "textChunkLimit": 2000
    },
    "wecom": { "enabled": true, "botId": "…", "secret": "$WECOM_SECRET" },
    "telegram": {
      "enabled": true,
      "botToken": "$TELEGRAM_BOT_TOKEN",
      "textChunkLimit": 4000,
      "dmPolicy": "pairing"          // pairing|allowlist|open|disabled
    },
    "defaults": { "groupPolicy": "allowlist", "groupAllowFrom": [] }
  },
  "bindings": [],                    // 结构预留：(channel, accountId, peer) → agentId；MVP 全路由 main
  "session": {
    "dmScope": "per-channel-peer",   // main|per-peer|per-channel-peer|per-account-channel-peer
    "groupScope": "per-group",       // per-group（默认）|main
    "reset": { "mode": "none" }      // none|daily{atHour}|idle{idleMinutes}（阶段 5）
  },
  "messages": {
    "queue": {
      "mode": "steer",               // steer|followup|collect|interrupt
      "debounceMs": 500,             // 批处理防抖（steer/followup/collect 生效）
      "cap": 20,
      "drop": "summarize",           // summarize|old|new
      "debounceMsByChannel": {}      // { "qq": 1000 } 裸数字=ms
    },
    "groupChat": {
      "requireMention": true,
      "groupPolicy": "allowlist",    // open|disabled|allowlist（默认 allowlist，fail-closed）
      "groupAllowFrom": ["qq:12345"],
      "mentionPatterns": [],
      "unmentionedInbound": "user_request"  // user_request|room_event
    }
  },
  "commands": {
    "ownerAllowFrom": ["qq:10000", "telegram:123456789"]  // owner 身份 = "<channel>:<userId>"
  }
}
```

校验规则（对齐 OpenClaw `docs/gateway/configuration.md`）：未知键/坏类型/非法值 → **拒绝启动，进程退出码 78**；凭据支持三来源：明文 / `$ENV_VAR` / `!shell 命令`（复用 latent-web `credential.rs` 的既有解析器）；启动时对危险配置打 stderr 警告（不阻断）：`dmPolicy: open`、群 allowlist 为空、token 走明文等。

### 4.3 routing/session_key.rs

上游：`src/routing/session-key.ts` 的 `buildAgentPeerSessionKey`。分隔符统一 `:`：

| 场景 | key |
|---|---|
| 群聊（groupScope=per-group） | `agent:{agentId}:{channel}:group:{conversation_id}` |
| 私聊 dmScope=`main` | `agent:{agentId}:{mainKey}`（mainKey 固定 `main`） |
| 私聊 dmScope=`per-peer` | `agent:{agentId}:direct:{peerId}` |
| 私聊 dmScope=`per-channel-peer`（**本项目默认**） | `agent:{agentId}:{channel}:direct:{peerId}` |
| 私聊 dmScope=`per-account-channel-peer` | `agent:{agentId}:{channel}:{accountId}:direct:{peerId}` |
| 线程/话题（二期） | 追加 `:thread:{threadId}` |

- `agentId` 默认 `main`（上游 `LEGACY_IMPLICIT_AGENT_ID = "main"`）；channel/peerId 为空时用 `"unknown"` 兜底（上游同款）。
- identityLinks（跨渠道身份合并）二期再做，结构预留。

### 4.4 agents.rs（会话注册表）

```rust
pub struct ChatSessionRegistry {
    sessions: Mutex<HashMap<String, ChatSession>>,   // key = session key
}
pub struct ChatSession {
    pub built: BuiltSession,             // 来自 latent-runtime::build_session
    pub run_lock: tokio::sync::Mutex<()>,// 同会话串行；跨会话并行
    pub queue: FollowupQueue,            // 见 4.6
}
```

- **惰性创建**：第一条消息到达该 session key 时才 `build_session`（provider/settings/MCP specs 全局解析一次共享；PermissionEngine/MCP 子进程/会话文件每会话独立——这是 `build_session` 的既有语义，不要试图共享）。
- 会话文件：`SessionStore::New { dir: <数据目录>/sessions }`；session key → 文件路径的映射持久化在 `state.json`，重启后 resume（`SessionStore::Resume`）。
- **资源模型（写进用户文档）**：每会话 = 1 个 MCP 子进程 × spec 数 + 1 个 PermissionEngine + 1 个 JSONL 文件。MVP 无会话回收（二期 LRU）。
- 退出处理：Ctrl-C → 所有会话 `subagent_registry.abort_all()` + 渠道优雅断开 + state.json 落盘。

### 4.5 auto_reply/mod.rs（dispatch 管线）

```
dispatch_inbound_message(msg: InboundMessage):
  1. 去重 claim（message_id，先于 ACK——上游语义 "ingress reserves before ACK"）
  2. 授权检查：私聊 dmPolicy（pairing/allowlist/open/disabled）；群聊 groupPolicy
     （allowlist 默认）+ requireMention（未 @ 且 unmentionedInbound=room_event →
     仅作上下文存储，不触发 run——MVP 直接丢弃并记日志）
  3. 命令拦截（commands.rs；未识别 /xxx → 本地警告不发给模型）
  4. 防抖合并窗口（debounce.rs）
  5. session key → 注册表取/建会话
  6. 队列处置（4.6）
  7. 执行 run：envelope 包装 → session.prompt() → 事件泵收集 → reply_dispatcher 投递
```

**事件泵**：每个 BuiltSession 挂一个 `SessionSubscriber`，把 `AgentSessionEvent` 推进该会话的 mpsc（**订阅者只推 channel，绝不在回调里做慢 IO**——core 的事件分发是串行 await 的）。泵的语义：

- 最终回复 = 累计 `MessageDelta::Text`，以 assistant `MessageEnd` 为权威定稿；`AgentSettled` = run 结束信号。
- 进度通知：`ToolExecutionStart` 节流（默认 30s 一条「仍在工作…」）；`AutoRetryStart` 可选通知。
- **审批请求**（`ApprovalRequested`）：转给 `approval.rs` 发消息（见 §5.2）。
- 后台 subagent：supervisor 唤醒的新 turn 事件自然流入泵——常驻 gateway **不要**调 `wait_background_subagents`（那是 print/json 退出前的兜底）。

**envelope.rs**（上游：`src/auto-reply/envelope.ts` 的 `formatInboundEnvelope`/`formatAgentEnvelope`）：

- DM + 自己的消息 → `(self): body`；DM → `{sender}: body`；群 → `{sender}: body`。
- 外层方括号头：`[{channel} {chat_type}:{conversation_id} {sender} {HH:mm}] body`；头部字段做 `sanitizeEnvelopeHeaderPart` 式清洗（替换方括号，防伪造层级）。

### 4.6 auto_reply/queue.rs（四模式队列）

上游：`src/auto-reply/reply/queue.ts` + `queue/types.ts` + docs `concepts/queue.md`（**默认值逐项核实过**）。

- `QueueSettings { mode, debounce_ms, cap, drop_policy }`；优先级：会话内 `/queue` 覆盖 > `messages.queue.byChannel.<channel>` > 全局 > 默认（steer / 500ms / 20 / summarize）。
- 会话忙（run_lock 被占）时入站消息的处置：
  - **steer**（默认）→ `session.steer(text)`：注入当前 run（latent 主循环在 turn 间隙消费；工具执行中途注入语义见 §8 偏离 4）；
  - **followup** → `session.follow_up(text)`：排到当前 run 结束后的新 turn；
  - **collect** → 防抖合并为单条后 follow_up（不同聊天对象各自独立）；
  - **interrupt** → `session.abort()` 后重新 prompt。
- cap 溢出（默认 20）：`drop=summarize` 在 latent 的近似实现 = **丢弃最旧 + 发一条合成提示 follow_up（「已丢弃 N 条更早消息」）**；`old` 直接丢最旧；`new` 拒绝新消息并回执。
- 去重键（对齐上游 `agent-dedupe.ts`）：入站 `message-id`；副作用方法（chat.send）带幂等键，进程内 Map + 过期时间。

### 4.7 auto_reply/commands.rs（聊天命令）

解析复用 `latent-runtime::slash::parse` 的风格但**独立命令表**（聊天端 ≠ TUI 端），正则形态对齐上游（`commands-reset.ts` 的 `/^\/(new|reset)(?:\s|$)/i`、`commands-approve.ts` 的 `/^\/?approve(?:\s|$)/i`）：

| 命令 | 权限 | 语义 |
|---|---|---|
| `/new`、`/reset` | 触发者（作用于自己触发的会话） | 新建会话（`latent-runtime::switch_new_session`） |
| `/compact [提示]` | 触发者 | `session.compact()` |
| `/stop` | 触发者 | `session.abort()` |
| `/status` | 触发者 | 模型/模式/队列深度/会话文件/用量 |
| `/model <spec>`、`/thinking <level>` | 触发者 | `set_model` / `set_thinking_level` |
| `/mode <plan\|confirm\|full-access>` | **full-access 仅 owner**（§5.1）；plan/confirm 触发者可用 | `set_mode` |
| `/queue <mode> [参数]` | **仅 owner** | 调整本会话队列设置 |
| `/activation <mention\|always>` | **仅 owner** | 按群切换 @ 门（对应 groupChat.requireMention 覆盖） |
| `/approve <id> <decision>` | **仅 owner**（§5.2） | decision：`allow|once|allow-once` / `always|allow-always` / `deny|reject|block`（顺序可换，上游同款） |
| `/help` | 所有人 | 命令清单 |

owner 判定：`commands.ownerAllowFrom` 里含 `"<channel>:<user_id>"`。非 owner 发特权命令 → 拒绝并提示（不暴露命令存在与否的区分，统一回复「该命令仅 owner 可用」）。

### 4.8 auto_reply/reply_dispatcher.rs（出站保序）

上游：`src/auto-reply/reply/reply-dispatcher.ts`——语义逐条对照：

- 所有出站（工具进度/流式块/最终回复）串在同一条 **sendChain** 上，保持 tool → block → final 顺序；三类计数 `queued_counts { tool, block, final }`。
- **最终回复只发一次**（`settlePendingFinalDelivery` 语义）。
- `NO_REPLY` 类静默标记 → 跳过发送。
- 长文经 `chunk.rs` 分段后顺序发送（段间可配延时；首段不延迟——上游 human delay 只对 block 且首块不延迟）。
- 平台发送失败：按 `ChannelError` 分类回执/重试（对齐上游 `ReplyMediaFailure` 的 code 面：file-not-found / unsupported-format / delivery-failed / invalid-reference）。
- （阶段 5）流式 block 分段：上游 `block-streaming.ts`——`MIN 800 / MAX 1200 字符`、coalesce idle `1000ms`（钳制 0–5000）、断句偏好 `paragraph`、joiner `"\n\n"`。

### 4.9 approval.rs（ChatApprovalUi）

```rust
pub struct ChatApprovalUi { /* owner 聊天句柄 + pending: Mutex<HashMap<u64, oneshot::Sender<ApprovalDecision>>> */ }

#[async_trait]
impl latent_core::ApprovalUi for ChatApprovalUi {
    async fn request_approval(&self, request: ApprovalRequest) -> Option<ApprovalDecision> {
        // 1. 发消息到 owner：「⏳ 需要批准 [{tool_name}] {detail}\n/approve {id} allow-once|allow-always|deny」
        //    （id 用 request.tool_call_id 的短哈希或自增号，建立映射）
        // 2. 等 oneshot，超时 120s → None
        // 3. None = Deny（core 的 ApprovalHooks 语义，fail-closed）
    }
}
```

- 模板是 `modes/rpc.rs` 的 `RpcApprovalUi`（id → oneshot 路由 + close_all 全部落 Deny）——**照抄其结构**。
- 经 `BuildOptions.approval_ui` 注入（接缝既有，勿改 core）。
- `/approve` 命令 → 查 pending 表 → `resolve(id, decision)`；`allow-always` → `ApprovalDecision::ApproveForSession`。
- 渠道断开/重启 → `close_all()`：所有未决审批落 Deny。

### 4.10 pairing.rs（DM 配对）

上游：docs `channels/pairing.md`（语义逐项核实）。

- dmPolicy 默认 `pairing`：陌生人私聊 → 生成 **8 位码（大写，剔除 0O1I）**，回复「配对码 XXXX，1 小时内有效」；**每渠道账户 pending 上限 3**，超限拒绝。
- 批准：`latent-gateway pairing approve <channel> <CODE>`（CLI 子命令，落 state.json）；批准**只授 DM 访问**，永不授群访问（上游同款 fail-closed）。
- `allowlist`：`channels.<id>.allowFrom` 显式列表；`open`：仅当列表含 `"*"` 才真公开；`disabled`：拒绝所有 DM。

### 4.11 state.rs

`<数据目录>/gateway/state.json`（原子写：临时文件 + rename）：

```jsonc
{
  "sessions": { "agent:main:qq:group:12345": { "file": "/…/sessions/…jsonl", "createdAt": 1728000000000 } },
  "pairing": { "pending": { "telegram": [ { "code": "ABCD2345", "userId": "…", "expiresAt": 1728003600000 } ] },
               "approved": { "telegram:123456789": { "approvedAt": 1728000000000 } } }
}
```

去重表/幂等表**仅在内存**（进程内 Map + 过期；重启后重放风险与上游一致，接受）。

### 4.12 gateway/（WS 控制面，阶段 4）

上游协议（docs `gateway/protocol/*.md` + `packages/gateway-protocol/src/schema.ts`）——**帧格式逐字对齐**：

```
文本帧 JSON。首帧必须是 connect，否则服务端立即断连。
请求：  {"type":"req","id":1,"method":"chat.send","params":{…}}
响应：  {"type":"res","id":1,"ok":true,"payload":{…}}   /  {"type":"res","id":1,"ok":false,"error":"…"}
事件：  {"type":"event","event":"agent","payload":{…},"seq":42}     // seq 单调递增，客户端断档须刷新
```

MVP 方法表（对齐上游 `core-descriptors.ts` 的命名；实现放 `gateway/methods/`，一方法一文件）：

| method | 语义 |
|---|---|
| `connect` | 握手：params 带 `client{id,version}`、`auth.token`、`role:"operator"`、`scopes`；成功回 `hello-ok`（含 server 版本、策略上限、当前状态快照） |
| `health` / `status` | 存活 / 运行态（uptime、渠道状态、会话数） |
| `chat.send` | 投递一条消息——**与渠道入站走同一 dispatch 管线**（`sessionKey` 直指目标会话；带幂等键） |
| `chat.abort` | 中止指定会话活跃 run（带 runId 则精确取消） |
| `chat.history` | 拉指定会话最近消息 |
| `sessions.list` / `sessions.reset` | 列出活跃会话 / 重置（= /new） |
| `channels.status` | 渠道连接状态（`--probe` 深探活） |
| `config.get` | 读当前生效配置（redact 凭据） |

事件（上游 `GATEWAY_EVENTS` 的子集）：`agent`（run 进度/最终回复）、`chat`（入站回执）、`session.message` / `session.typing` / `session.approval`、`channels`（状态变化）、`health`、`shutdown`。

认证：`gateway.auth.token` 必填（§5.4）；后续客户端（CLI/TUI/桌面端）连 `ws://127.0.0.1:18789` 用同一协议——这就是未来桌面端零网关改动的接入口。

---

## §5 安全设计（硬性要求，已与需求方拍板）

### 5.1 权限天花板（防远程提权）

- 聊天端 `/mode full-access` **仅 owner 白名单可执行**；非 owner 一律拒绝。这是硬要求：群 allowlist 只挡"哪个群"，不挡群内哪个人——若不设天花板，任何群成员都能拿走机器的完全控制权。
- plan/confirm 切换允许触发者使用（作用于自己触发的会话）。

### 5.2 审批应答权

- `/approve` **仅 owner 可应答**；非 owner 发 `/approve` 无效并提示。
- 审批请求默认投递到 owner 可达的聊天（有 owner 的 DM 渠道 → 发 owner 私聊；否则在触发会话内等待，但仍只接受 owner 应答）。
- 超时 120s / 渠道断开 → `None` → **Deny**（fail-closed，与 core 的 ApprovalUi 语义一致）。

### 5.3 渠道端点

- QQ 反向 WS：`reverseWsHost` 默认 **127.0.0.1**（配置可改但文档强警告）；`accessToken` **必填**，缺失/为空 → 该渠道拒绝启动（不是警告）；校验用 `Authorization: Bearer` Header（**不用 query 参数**——query 会进访问日志）。
- 出站连接（telegram/wecom）凭据走 `$ENV`/`!shell` 三来源，避免明文落盘；项目级 `.latent/gateway.json` 文档提醒加入 `.gitignore`。

### 5.4 控制面

- `gateway.auth.token` 未配置 → **拒绝启动**（错误信息指明如何配置）；建议自动生成强随机值并打印一次的选项可做（阶段 5）。
- 默认绑定 `127.0.0.1:18789`。
- 本机信任边界声明（写入文档）：控制面端口对本机所有进程开放，本机恶意进程不在防御模型内（与 OpenClaw 相同的定位——"一个 Gateway 一个信任边界"）。

### 5.5 其他

- **panic 边界**：渠道任务、事件泵、命令处理任务全部要求 catch panic 降级诊断；单渠道故障不击穿 daemon（健康监控自动重启连接，参考上游 `channel-health-monitor.ts`）。
- **日志 redact**：诊断输出不打凭据与完整消息体（沿用仓库既有 redact 惯例）。
- **已知限制（写进用户文档）**：提示注入防线 = 权限引擎 + 审批 + §5.1 天花板（群消息本身就是 prompt，"忽略之前指令"类攻击由 Confirm 审批兜底，FullAccess 下无防线——所以 5.1 是关键）；无 per-user 限速（队列 cap 20 + 防抖兜底，恶意刷屏 = LLM 账单风险）；一个 gateway = 一个工作区（多群并发共享 cwd，会互相踩文件——二期 per-agent workspace）；MCP 子进程按会话数倍增。
- MVP 不做"从消息内 URL 拉取媒体"（避免引入模型可控 URL 的请求面；latent-web 的 ssrf.rs 只管工具请求）。

---

## §6 实现细节对照表（核心章节：每个模块动手前先读这些）

OpenClaw 仓库 <https://github.com/openclaw/openclaw>（MIT）。路径基于 main @ `6693bb96`；404 时按关键词搜。

| 模块 | 实现前必读（OpenClaw 路径） | 关键词 / 已核实语义 |
|---|---|---|
| 四模式队列 | `src/auto-reply/reply/queue.ts`、`queue/types.ts`、docs `concepts/queue.md` | `QueueSettings{mode,debounceMs,cap,dropPolicy}`；默认 steer/500ms/cap 20/drop summarize；优先级 会话覆盖>byChannel>全局>默认；`QueueDedupeMode "message-id"\|"none"`；`QueueInsertPosition "tail"\|"front"` |
| 入站防抖 | `src/auto-reply/inbound-debounce.ts`、`src/channels/inbound-debounce-policy.ts` | `createInboundDebouncer`（enqueue/shouldBuffer/flushKey/cancelKey/drain）；默认 0ms；**窗口首条固定 ×5 封顶**；`DEFAULT_MAX_TRACKED_KEYS = 2048`；只防抖文本类消息 |
| 长文分段 | `src/auto-reply/chunk.ts` | `DEFAULT_CHUNK_LIMIT = 4000`、`DEFAULT_CHUNK_MODE = "length"`；`resolveTextChunkLimit`（账号级>渠道级）；`scanParenAwareBreakpoints`；newline 模式空行正则；markdown fence 闭合重开 |
| 流式分段（阶段5） | `src/auto-reply/reply/block-streaming.ts` | `DEFAULT_BLOCK_STREAM_MIN 800 / MAX 1200 / COALESCE_IDLE_MS 1000`（钳 0–5000）；break `paragraph`；joiner `"\n\n"` |
| 入站包装 | `src/auto-reply/envelope.ts`、`src/auto-reply/sender-identity.ts` | `formatInboundEnvelope`（self/sender/群三形态）；`formatAgentEnvelope` 方括号头 + `sanitizeEnvelopeHeaderPart` |
| 分发管线 | `src/auto-reply/dispatch.ts`、`src/auto-reply/reply/dispatch-from-config.ts`（+ 同前缀的 gather/prepare/execute/finalize 各文件） | 阶段序：admission ticket **先于 ACK** → gather → prepare → route → execute → finalize；`DispatchSessionRefreshRequiredError` 重试一次 |
| 出站保序 | `src/auto-reply/reply/reply-dispatcher.ts` | sendChain 串行；`queuedCounts{tool,block,final}`；**final 只发一次**；NO_REPLY 跳过；human delay 仅 block 且首块不延迟 |
| session key | `src/routing/session-key.ts` | `buildAgentPeerSessionKey`；`LEGACY_IMPLICIT_AGENT_ID = "main"`；空 channel/peer → `"unknown"`；thread 后缀 `:thread:{id}` |
| typing | `src/channels/typing.ts`、`src/channels/typing-lifecycle.ts`、docs `concepts/typing-indicators.md` | keepalive **3000ms**、TTL **60000ms**、连续失败 **2** 停；typingMode 四档；**无 typing gate**；入队即发 |
| @判定 | `src/channels/mention-gating.ts`、`src/auto-reply/reply/mentions.ts` | `resolveInboundMentionDecision`；`buildMentionRegexes/matchesMentionPatterns`（大小写不敏感）；mentionPatterns 优先级 agent > messages.groupChat > identity.name |
| 群策略 | docs `channels/groups.md` | groupPolicy 默认 **allowlist**（fail-closed，config 缺块时直接 allowlist 并警告）；requireMention 默认 true；`unmentionedInbound` 默认 `user_request`；`/activation mention\|always` owner-only |
| DM 配对 | docs `channels/pairing.md` | 8 位码剔除 0O1I；1h 过期；每账户 pending ≤ **3**；批准只授 DM；`pairing|allowlist|open|disabled`；open 需列表含 `"*"` |
| 渠道接口 | `src/channels/plugins/types.plugin.ts`、`types.adapters.ts`、`types.core.ts`、`manifest-channel-plugin.types.ts`；参照实现 `extensions/telegram/`（`openclaw.plugin.json` + `src/channel.ts` 的 `createChatChannelPlugin` 组装式） | config/outbound/status/pairing/mentions/typing 等"适配器集合"形态——我们的 ChannelPlugin trait 是其收敛子集 |
| 控制面协议 | docs `gateway/protocol/handshake.md`、`auth.md`、`rpc-*.md`；`packages/gateway-protocol/src/schema.ts` | req/res/event 三帧；**首帧必须 connect**；`connect.challenge{nonce,ts}`；`hello-ok` 带策略上限（maxPayload/maxBufferedBytes/tickIntervalMs）；副作用方法幂等键 |
| 方法表 | `src/gateway/methods/core-descriptors.ts`、`src/gateway/server-methods-list.ts` | `CORE_GATEWAY_METHOD_SPECS`：`[name, family, scope, since]` + policy 位；我们只实现 §4.12 的子集，**命名照抄** |
| 事件表 | 同上 + docs `gateway/protocol/` | `agent/chat/session.*/channels/health/heartbeat/shutdown` 等命名照抄 |
| 去重 | `src/gateway/agent-turn/agent-dedupe.ts` | 键 `agent:${idempotencyKey}`、`agent:${runId}`；进程内 Map + `expiresAtMs`；入站 `MessageSidFull ?? MessageSid ?? …` 取键；`markInboundDedupeReplayUnsafe` |
| 命令 | `src/auto-reply/reply/commands-reset.ts`、`commands-compact.ts`、`commands-approve.ts`、`commands-session.ts`、`src/auto-reply/status.ts` | `/new\|/reset` 正则；`/approve` decision 别名表（allow/once/allow-once、always/allow-always、deny/reject/block，id 与 decision 顺序可换）；`/activation`、`/stop` |
| 审批（上游形态） | `src/gateway/exec-approval-manager.ts`、`src/auto-reply/reply/commands-approve.ts` | `/approve` 走 gateway RPC；scope 门禁——**我们的差异：审批应答仅 owner（§8 偏离 6）** |
| 健康监控 | `src/gateway/channel-health-monitor.ts`、`channel-health-policy.ts`、`channel-thaw-restart.ts` | 区分"传输活着"与"消息在流动"；冻结/重启策略 |
| 配置 | `src/config/types.openclaw.ts`、docs `gateway/configuration.md` | 严格校验 + **退出码 78**；顶层段命名照抄（agents/channels/gateway/bindings/session/messages/commands） |
| 会话存储 | docs `concepts/session.md` | 上游 sqlite（`~/.openclaw/agents/<id>/agent/*.sqlite`）；**我们用 latent-session JSONL（§8 偏离 1）**；重置策略 daily(atHour 4)/idle 的参数面可参考 |
| 多 agent 路由（预留） | docs `concepts/multi-agent.md` | bindings 九级最具体匹配（exact peer → … → channel → fallback owner）；MVP 只留结构 |
| daemon（阶段5） | docs `gateway/`（install/daemon 页） | launchd label `ai.openclaw.gateway` / systemd user unit + linger / 退出码 78 / safe mode 思想 |

**已知坑清单（上游已解决、直接继承）**：

1. 去重 claim 必须**先于 ACK**（否则客户端重试造成双跑）。
2. 防抖窗口在首条到达时固定（maxWait = debounceMs×5），后续消息**不得**顺延窗口。
3. OneBot 11：action 响应用 `echo` 字段关联（oneshot 表）；`meta_event.heartbeat` 判活；多 NapCat 实例按 `self_id` 路由；token 走 Header 不走 query。
4. Telegram：`getUpdates` 带 `timeout` 长轮询；`offset = last_update_id + 1`；**两个轮询进程会 409 Conflict**（重启竞速时注意）；botToken 从 `$ENV` 读。
5. 一个会话同时只能有一个 run（run_lock）；`PromptOutcome::Started` 与 `Enqueued` 必须区分上报。
6. 订阅者回调里做慢 IO 会阻塞整个会话的事件分发——只推 channel。
7. `unsafe_code = "forbid"` 全仓生效；serde 判别符改名会破坏兼容（本计划新增类型无历史包袱，但仍守 camelCase 约定）。
8. feature unification：workspace 全量构建会启用所有渠道 feature（见 §2.2）。

---

## §7 实施阶段与验收

**每阶段通用硬门槛**：`cargo build --workspace` + `cargo test --workspace` + `cargo clippy --workspace --all-targets` 全绿零警告；§1 的三条 `cargo tree` 隔离断言通过。

### 阶段 1：latent-runtime 抽取 + 构建体系（约 1 天，行为不变）

产出：`crates/latent-runtime`（§3 全部迁移项）、根 Cargo.toml 变更（§3.3）、latent-cli re-export 兼容层、RPC 审批装配链路修复（§3.2 末）及其回归测试。
验收：现有全部测试不改动语义即通过；`cargo run -p latent-cli -- --mock "你好"` 行为不变；`cargo tree -p latent-runtime -i ratatui` 为空。

### 阶段 2：latent-channel 宿主层 + latent-gateway 核心（约 2-3 天）

产出：latent-channel（types/plugin/typing/mention_gating/debounce/chunk/error + mock feature，单测覆盖每个机制的常量语义）；latent-gateway（config/session_key/state/agents/auto_reply/approval/pairing + daemon 骨架，main 默认分支）。
集成测试（mock 渠道 + ScriptedProvider，全离线）：端到端消息→回复；steer/followup/collect/interrupt 四模式；cap 20 溢出处置；防抖合并与 ×5 封顶；chunk 三模式与 fence；session key 四档 dmScope + 群 key + unknown 兜底；envelope 格式；/new /compact /stop /status /model；群 allowlist + requireMention 拒绝路径；pairing 全流程（发码→过期→批准→放行→pending 上限）；审批三应答 + 超时 Deny + close_all 全 Deny；message-id 去重；重启后会话恢复（state.json）。

### 阶段 3：三个渠道（约 3-4 天，相互独立，顺序 telegram → qq → wecom）

产出：`latent-channel/{telegram,qq,wecom}` 各自实现 + 假平台服务器单测（telegram：假 Bot API 轮询服务器；qq：假 NapCat WS 客户端 + echo 关联；wecom：官方文档逐字核对后定用例）。
每渠道验收：事件归一化单测、出站分段与限速、断线重连、typing、凭据 `$ENV` 解析。

### 阶段 4：控制面 WS + bin 子命令（约 1-2 天）

产出：gateway/server+auth+methods+events（§4.12）；`latent-gateway pairing list/approve`、`channels status [--probe]`、`status` 子命令。
验收：协议帧集成测试（connect 首帧强制、token 错误拒绝、chat.send 走同一管线、事件 seq 单调）。

### 阶段 5：打磨 + 文档（约 1-2 天）

产出：block_streaming（§4.8 参数）；/queue 会话命令；session reset 策略（daily/idle）；daemon 安装脚本（launchd/systemd，label `ai.latent.gateway`）；`AGENTS.md` 更新（目录索引加三 crate、依赖图、gateway.json 配置节、"参考 OpenClaw 上游"一节 + §8 偏离记录全文）；README 特性表。
安全用例（并入阶段 2/4 测试，验收时复查）：非 owner 切 full-access 被拒；非 owner /approve 无效；无 token 渠道/控制面拒绝启动；未授权 WS 连接被断；陌生人 DM 走 pairing；非白名单群消息丢弃；danger 配置启动警告出现。

---

## §8 与 OpenClaw 的偏离记录（实现时不得擅自扩大偏离面）

1. **会话存储**：latent-session JSONL（OpenClaw 为 per-agent sqlite）——"转录即真相"是本仓核心不变量，不追求与其存储兼容。
2. **单 agent MVP**：session key、bindings 配置结构按多 agent 预留，运行期全路由 `main`。
3. **队列 drop=summarize**：以"丢弃最旧 + 合成提示 follow_up"近似（上游为摘要注入）。
4. **steer 注入时序**：latent 主循环在 turn 间隙消费 steering；bash 等长工具执行期间 steer 排队（上游可在工具执行中途注入）。不强改主循环时序。
5. **QQ（OneBot 11/NapCat）与企微渠道**为 OpenClaw 所无，按其 ChannelPlugin 接口语义新写；NapCat 是外部部署组件（docker 配置文档随渠道交付）。
6. **安全模型更严**：full-access 切换与审批应答均 owner-only（上游审批走 operator scope 体系）；目的：聊天暴露面大于终端，防群内横向提权。
7. **控制面**为上游数百方法的小子集；token 单角色，无 connect.challenge/设备配对（MVP）。
8. **配置**为 serde JSON（无 JSON5/$include）；时间戳毫秒整数（仓库既有约定）。
9. **重启不恢复在途 run**（无上游 restart recovery/tombstone 机制），转录保留；去重/幂等表仅内存。

---

## §9 仓库工程规范与既定选型

### 工程规范（摘自 AGENTS.md，对新增代码同样硬性）

- `unsafe` 全仓禁止（workspace lint `unsafe_code = "forbid"`）。
- 新外部依赖一律 pin 精确版本并登记根 `[workspace.dependencies]`（本计划新增：`tokio-tungstenite =0.30.0`、`axum` 最新稳定精确版；tokio 补 `net` feature）。
- crate 对外只暴露工厂、trait、类型；实现类型不 `pub`。
- 错误处理：`thiserror` 类型化向上传播；工具/渠道错误绝不 panic 击穿宿主；fail-closed 优先。
- 异步统一 tokio；trait 异步方法用 `async-trait`；取消经 `CancellationToken`/select。
- 对外 JSON 字段 camelCase；时间戳毫秒整数。
- trait 方法不 panic；观察回调错误吞掉 + 诊断。
- 提交前：三件套全绿零警告。

### 本计划已拍板的选型结论（不再重议）

| 决策点 | 结论 |
|---|---|
| QQ 路线 | **NapCat + OneBot 11 反向 WebSocket**（gateway 为 WS 服务端；NapCat 外部部署，文档附 docker 示例） |
| 微信路线 | **企业微信智能机器人 WS 长连接**（官方；个人微信非官方方案有封号风险，明确不做；未来如做，新增渠道模块隔离） |
| Telegram | **Bot API getUpdates 长轮询**，reqwest 自写不引 SDK |
| 会话粒度 | 群聊每群一个会话（groupScope=per-group）；私聊每人一个（dmScope=per-channel-peer） |
| full-access 切换 | 仅 owner 白名单（§5.1） |
| 审批应答权 | 仅 owner（§5.2） |
| crate 组织 | 三 crate：latent-channel / latent-runtime / latent-gateway；渠道 feature 门控于 latent-channel |
| 控制面 | 首期做最小镜像（§4.12），是未来 CLI/桌面端的统一接入口 |
| 参考架构 | OpenClaw（MIT）：概念命名、目录结构、协议帧、默认值全对齐；语义不确定时**以上游源码为准**（§0/§6） |

### 与既有仓库的接缝速查

- 装配点：`build_session(BuildOptions) -> BuiltSession`（迁移后在 `latent-runtime`）；聊天会话经它创建，`approval_ui` 参数注入 `ChatApprovalUi`。
- 会话操作：`AgentSession::{prompt, steer, follow_up, abort, wait_idle, queue_depths, set_mode, compact, set_model, set_thinking_level}`；`switch_new_session` / `switch_resume_session`（/new 与恢复）。
- 会话存储：`SessionStore::{Memory, New{dir}, Resume{file}}`；`latent_session::list_session_files`。
- 事件：`AgentSessionEvent::{Agent(AgentEvent), AgentSettled, QueueUpdate, AutoRetryStart/End, ApprovalRequested, ApprovalResolved}`；最终回复 = Text delta 累计 + assistant `MessageEnd` 定稿。
- slash 解析参考：`latent-runtime::slash::parse`（迁移后）。
- 凭据三来源解析器：`latent-web::credential`（gateway.json 的 `$ENV`/`!shell` 复用其思路或抽到 latent-core；注意依赖方向——若 latent-gateway 直接依赖 latent-web 亦可，它在 L2）。
