# rpi 核心改造任务清单

> **用途：直接交给 Coding Agent 执行。**
>
> 重要要求：**必须先阅读 rpi 实际源码，再修改代码，不要根据 README / 文档猜实现。**
>
> 当前只处理下面 4 个问题。其他问题暂时全部忽略，不要顺手扩展范围：
>
> 1. **Model / Provider / URL 配置：CLI 当前无法配置 URL，需要做成类似 Pi 的 JSON 配置体系**
> 2. **Session 当前只存在内存，需要真正持久化到 `.rpi/...` 文件**
> 3. **统一 Session / Agent Transcript**
> 4. **统一 Overflow / Compaction**
>
> 暂时不要实现 Permission System、MCP 动态更新、TUI 跨平台、Unicode、OAuth 等其他改造。
> （2026-09-28 更新：**Subagent 已立项**，设计见 `docs/14-plugin-in-process.md` §4，不再列入"不要做"。）

---

# 一、P0：Model / Provider / URL 配置体系

## 1. 当前实际问题

现在 CLI 主要支持：

```bash
rpi --provider <provider> --model <model> "prompt"
```

但是**没有 CLI 参数可以配置 `base_url`**。

当前底层 `Model` 实际上已经有：

```rust
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub input: Vec<String>,
    pub cost: ModelCost,
    pub headers: Option<HashMap<String, String>>,
    pub reasoning: bool,
    pub thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    pub context_window: u64,
    pub max_tokens: u32,
    pub sampling_params: Option<serde_json::Map<String, serde_json::Value>>,
    pub compat: Option<serde_json::Value>,
}
```

也就是说底层能力已经存在，但：

```text
CLI
  ↓
ModelResolver
```

这一层没有把自定义 model/provider/url 配置暴露出来。

---

# 2. 配置方式必须参考 Pi

不要自己设计一套完全不同的格式。

Pi 当前将：

```text
~/.pi/agent/models.json
```

作为 Model / Provider 配置文件。

Pi 的核心结构类似：

```json
{
  "providers": {
    "ollama": {
      "baseUrl": "http://localhost:11434/v1",
      "api": "openai-completions",
      "apiKey": "OLLAMA_API_KEY",
      "models": [
        {
          "id": "qwen3",
          "name": "Qwen 3",
          "reasoning": false,
          "input": ["text"],
          "contextWindow": 128000,
          "maxTokens": 32000
        }
      ]
    }
  }
}
```

Pi 也支持 provider 级 `baseUrl/api/compat` 和 model-level override。配置模型时应尽量采用同样的字段和层级，而不是重新发明格式。

---

# 3. rpi 配置文件

建议：

```text
~/.rpi/
├── settings.json
├── models.json
└── sessions/
```

职责：

### `models.json`

负责：

```text
provider
baseUrl
api
apiKey
headers
models
compat
```

### `settings.json`

负责：

```text
defaultProvider
defaultModel
defaultThinkingLevel
compaction
其他运行设置
```

不要在本次任务中扩展更多配置系统。

---

# 4. `models.json` 推荐格式

例如：

```json
{
  "providers": {
    "openai": {
      "baseUrl": "https://api.openai.com/v1",
      "api": "openai-completions",
      "apiKey": "OPENAI_API_KEY",
      "models": [
        {
          "id": "gpt-4.1-mini",
          "name": "GPT-4.1 Mini",
          "reasoning": false,
          "input": ["text"],
          "contextWindow": 128000,
          "maxTokens": 16384
        }
      ]
    },

    "my-proxy": {
      "baseUrl": "http://localhost:8080/v1",
      "api": "openai-completions",
      "apiKey": "MY_API_KEY",
      "models": [
        {
          "id": "my-model",
          "name": "My Model",
          "reasoning": false,
          "input": ["text"],
          "contextWindow": 128000,
          "maxTokens": 16384
        }
      ]
    }
  }
}
```

本地模型：

```json
{
  "providers": {
    "ollama": {
      "baseUrl": "http://localhost:11434/v1",
      "api": "openai-completions",
      "apiKey": "ollama",
      "models": [
        {
          "id": "qwen3",
          "name": "Qwen 3"
        }
      ]
    }
  }
}
```

---

# 5. API key 解析

不要强制所有 provider 必须存在：

```text
PROVIDER_API_KEY
```

因为自定义 provider 可能：

- 不需要 key
- 使用固定字符串
- 使用环境变量
- 使用代理自己的 header

至少支持 Pi 类似的：

```json
"apiKey": "OPENAI_API_KEY"
```

表示：

```text
读取名为 OPENAI_API_KEY 的环境变量
```

同时为以后支持 literal / credential source 留出空间。

不要把真实 API key 写入：

- 测试
- 默认配置
- 日志
- Git

---

# 6. Headers

支持：

```json
{
  "headers": {
    "X-Custom-Header": "MY_HEADER_VALUE"
  }
}
```

如果沿用 Pi 的语义，可以让 value 支持环境变量名。

---

# 7. Provider 与 Model 的继承关系

采用：

```text
Provider defaults
        ↓
Model overrides
```

例如：

```json
{
  "providers": {
    "foo": {
      "baseUrl": "http://localhost:8000/v1",
      "api": "openai-completions",
      "models": [
        {
          "id": "model-a"
        },
        {
          "id": "model-b",
          "api": "openai-responses"
        }
      ]
    }
  }
}
```

那么：

```text
model-a
    baseUrl = provider.baseUrl
    api      = provider.api

model-b
    baseUrl = provider.baseUrl
    api      = model.api
```

不要复制配置。

---

# 8. 与现有 ModelResolver 的关系

现在已有：

```rust
ModelResolver::register_model()
ModelResolver::resolve()
```

应该扩展现有 Resolver，而不是重新创建第二套 Model 系统：

```text
models.json
    ↓
load providers
    ↓
merge built-in providers
    ↓
register custom models
    ↓
resolve provider/model
    ↓
Model
```

---

# 9. Built-in Provider 与 Custom Provider

必须支持两种情况。

## A. 新增 provider

```json
{
  "providers": {
    "ollama": {
      "baseUrl": "http://localhost:11434/v1",
      "api": "openai-completions",
      "models": [...]
    }
  }
}
```

## B. 覆盖已有 provider 的 URL

例如：

```json
{
  "providers": {
    "openai": {
      "baseUrl": "https://my-proxy.example.com/v1"
    }
  }
}
```

这种情况下不要强制用户重新定义全部 OpenAI models。

应该允许：

```text
built-in provider
        +
user override
        ↓
merged provider
```

---

# 10. CLI 与 settings

最终至少应该支持：

```bash
rpi --provider openai --model gpt-4.1-mini
```

自动读取 `.rpi/models.json`。

最好也支持：

```bash
rpi --model openai/gpt-4.1-mini
```

如果没有指定 provider/model，则读取 `.rpi/settings.json`：

```json
{
  "defaultProvider": "openai",
  "defaultModel": "gpt-4.1-mini"
}
```

---

# 11. 关键问题：自定义 provider 不能被 env key 检查提前拦截

当前 CLI 类似：

```rust
if rpi_ai::env_keys::get_env_api_key(provider_id).is_none() {
    return Err(...);
}
```

这会导致：

```text
ollama
my-proxy
local
my-company
```

这类自定义 provider 在进入 ModelResolver 之前就失败。

必须改成：

```text
先读取 models.json
        ↓
解析具体 Model
        ↓
根据 Model / Provider 配置解析 credential
        ↓
创建 Provider
```

而不是：

```text
provider 名字
    ↓
硬编码 env key
    ↓
没有就直接失败
```

这是本次 Model 配置改造的关键点。

---

# 12. 暂时不要实现

这一阶段不要扩展：

- OAuth
- 完整 credential store
- 新 API adapter
- 大量新 provider
- 完整 Pi CLI
- provider catalog

目标只有：

> **用户能够通过 JSON 配置 provider、model、URL、API、key、headers，然后直接让 rpi 使用。**

---

# 二、P0：Session 真正持久化到 `.rpi`

## 1. 当前问题

当前 CLI assembly 中存在类似：

```rust
rpi_session::create_session(None::<String>)
```

导致 CLI 实际运行时 Session 主要存在：

```text
memory only
```

虽然 `rpi-session` 已经具备文件持久化相关能力，但 CLI 没有真正接通。

---

# 2. 目标

运行：

```bash
rpi
```

之后必须产生：

```text
~/.rpi/
└── sessions/
    └── 项目路径_时间_<session-id>.jsonl
```

优先采用类似 Pi 的 JSONL session storage。

Pi 的 session 本身也是以 JSONL 文件保存历史，session storage 与 cwd/project 有明确关联。

---

# 3. 为什么使用 JSONL

当前 Session 本身就是 append-only entry/event 模型，非常适合：

```text
一条 Entry
↓
一行 JSON
```

例如：

```text
{"type":"session","id":"..."}
{"type":"message","..."}
{"type":"message","..."}
{"type":"toolCall","..."}
{"type":"toolResult","..."}
{"type":"usage","..."}
```

优点：

- append 简单
- 不需要每次重写整个文件
- 崩溃恢复简单
- 与现有 Session Entry 模型天然匹配

---

# 5. Session 创建

interactive mode 启动：

```text
rpi
 ↓
定位当前项目 .rpi
 ↓
创建 .rpi/sessions/
 ↓
创建 session id
 ↓
创建 <session-id>.jsonl
 ↓
创建 file-backed Session
 ↓
Agent
```

不能再让 CLI 默认使用纯内存 Session。

---

# 6. 哪些内容必须持久化

至少检查并持久化当前已有 Entry 类型：

```text
SessionInfo
Message
ThinkingLevelChange
ModelChange
Usage
Compaction
BranchSummary
Custom
CustomMessage
ContextEdit
Label
```

以及 Agent 实际产生的：

```text
assistant message
tool call
tool result
user message
```

具体以源码已有 Entry 类型为准。

原则：

> 关闭进程后，Session 文件必须足够恢复完整历史。

---

# 7. Streaming 不要每 token 写文件

不要：

```text
每个 token
→ append JSONL
```

建议：

```text
assistant streaming
      ↓
内存累积
      ↓
assistant message 完成
      ↓
append 完整 message
```

Tool call / tool result 同理，在逻辑事件完成后写入。

---

# 8. Flush

关键 Session entry 应及时写入文件。

至少不能：

```text
整个 Agent run 结束
↓
一次性保存
```

否则进程崩溃会丢失整个 session。

建议：

```text
entry 产生
↓
append
↓
flush
```

具体是否每条都 `fsync` 不需要过度设计，先保证正常 flush 和 crash recovery 的基本能力。

---

# 9. Session 加载

必须实现：

```text
session file
    ↓
parse JSONL
    ↓
Session entries
    ↓
projection
    ↓
Agent Context
```

第一阶段不一定要实现完整：

```bash
rpi --continue
rpi --resume
rpi --fork
```

但是底层 API 不应该阻止以后增加这些功能。

---

# 三、P0：Session / Agent Transcript 统一

## 1. 当前问题

目前存在两套历史：

```text
Agent 内部 Context / Messages
```

和：

```text
SessionManager / Session Entries
```

容易产生：

```text
Agent 有
Session 没有

Session 有
Agent 没有
```

特别容易发生在：

- tool call
- tool result
- compaction
- overflow
- context edit
- branch

---

# 2. 最终原则

必须明确：

```text
Session = Source of Truth
```

Agent context：

```text
Session
   ↓
Projection
   ↓
Agent Context
```

Agent context 可以是内存 cache，但不能成为第二个无法重建的事实来源。

---

# 3. 目标架构

```text
                  Session
               append-only log
                     │
                     ▼
                 Projection
                     │
                     ▼
               Agent Context
                     │
                     ▼
                Agent Loop
```

正常流程：

```text
User Message
    ↓
append Session
    ↓
update Context
    ↓
Agent
```

Assistant：

```text
assistant result
    ↓
append Session
    ↓
update Context
```

Tool：

```text
tool call
    ↓
append Session
    ↓
execute
    ↓
tool result
    ↓
append Session
```

---

# 4. 不要求每次重新 projection

不要做成：

```text
每个 token
→ 重新读取 Session
→ projection
```

正确方向：

```text
Session = source of truth
Context = in-memory projection/cache
```

允许 Context 增量更新。

但必须保证：

```text
Session → Projection → Context
```

永远可以重新构建。

---

# 5. 必须统一的状态

至少检查：

- User Message
- Assistant Message
- Thinking / Reasoning
- Tool Call
- Tool Result
- Usage
- Model Change
- Thinking Level Change
- Context Edit
- Branch

不要为了统一而把纯 runtime 状态全部写进 transcript。

区分：

```text
persistent session state
```

和：

```text
runtime state
```

---

# 6. 验收测试

## Test 1：完整运行恢复

```text
创建 session
→ user message
→ assistant message
→ tool call
→ tool result
→ assistant message
```

然后：

```text
结束进程
→ 重新加载 session
→ projection
```

结果必须与运行结束时 Agent Context 一致。

---

## Test 2：只从 Session 恢复

测试不能直接依赖：

```text
Agent 内存 Vec<Message>
```

而应该：

```text
Session file
→ load
→ projection
→ context
```

证明 Session 真正具备恢复能力。

---

## Test 3：Tool interaction 恢复

Session 中：

```text
assistant tool call
tool result
```

重新 projection 后必须保留正确的 tool interaction。

---

# 四、P0：Overflow / Compaction 统一

## 1. 当前问题

当前存在两个不同机制：

### Agent overflow recovery

Agent context 太大时直接修改自己的 context，例如删除旧消息。

### Session compaction

Session 又通过：

```text
Compaction
```

entry 表达压缩后的历史。

于是可能出现：

```text
Agent:
旧消息已经被删

Session:
旧消息仍然存在
```

或者：

```text
Agent:
已经压缩

Session:
没有对应 Compaction entry
```

必须消除这种状态分裂。

---

# 2. 最终目标

统一成：

```text
Context overflow
       ↓
Compaction
       ↓
Session.append(Compaction)
       ↓
Projection
       ↓
New Context
       ↓
Agent continues
```

不要：

```text
Overflow
   ↓
直接 truncate Agent Vec
```

---

# 3. 不要删除 Session 原始历史

Session 与 LLM context 必须区分：

```text
Session = full history
Context = current working set
```

Compaction 改变的是：

```text
current working set
```

不是删除：

```text
full history
```

参考 Pi 的行为：完整历史仍保留在 JSONL session 中，compaction 只改变当前 context 的组成。

---

# 4. 复用现有 Compaction

当前已经存在：

```text
Compaction entry
```

优先复用，而不是创建：

```text
AgentCompaction
SessionCompaction
OverflowCompaction
```

三套机制。

应该形成一个统一入口：

```text
Compaction
```

例如：

```rust
async fn compact_context(...) -> Result<CompactionResult>
```

具体 API 以当前代码结构为准。

---

# 5. Manual / Automatic Compaction

最终：

```text
manual compact
automatic overflow compact
```

都应该调用同一个底层 compaction implementation。

不要：

```text
manual compact → A
overflow → B
```

---

# 6. Compaction 必须持久化

运行：

```text
context overflow
      ↓
compaction
      ↓
Session.append(Compaction)
      ↓
Agent 继续
```

如果之后进程退出：

```text
重新加载 Session
      ↓
projection
```

必须得到和之前一致的压缩后 context。

---

# 7. Compaction 的数据模型

如果现有 `Compaction` entry 已经包含：

```text
summary
firstKeptEntryId
```

或类似字段：

优先复用现有模型。

不要为了“架构漂亮”重新设计一套不兼容的数据结构。

真正需要保证的是：

```text
Compaction entry
```

能够让 projection 知道：

```text
哪些历史已经由 summary 表示
哪些最近消息继续保留
```

---

# 五、最终目标架构

```text
                    ┌──────────────────┐
                    │  .rpi/models.json│
                    │ provider/model/url│
                    └────────┬─────────┘
                             │
                             ▼
                      Model / Provider
                             │
                             ▼
                         Agent Loop
                             │
                 ┌───────────┴───────────┐
                 │                       │
                 ▼                       ▼
              Tool                   Session
                                         │
                                  .rpi/sessions/
                                         │
                                  append-only JSONL
                                         │
                                         ▼
                                    Projection
                                         │
                                         ▼
                                  Agent Context
                                         │
                              ┌──────────┴──────────┐
                              │                     │
                           normal                overflow
                              │                     │
                              │                     ▼
                              │                 Compaction
                              │                     │
                              │                     ▼
                              └────────────── Session
```

核心关系：

```text
models.json
    ↓
Model configuration

Session JSONL
    ↓
Source of Truth

Projection
    ↓
Agent Context

Compaction
    ↓
Context projection change
    ↓
Persisted Session Entry
```

---

# 六、严格实施顺序

## Phase 1：Model 配置

- [ ] `.rpi/models.json`
- [ ] Pi 风格 `providers`
- [ ] provider `baseUrl`
- [ ] provider `api`
- [ ] provider `apiKey`
- [ ] provider `headers`
- [ ] provider `compat`
- [ ] provider `models`
- [ ] model-level override
- [ ] built-in provider + user override
- [ ] custom provider
- [ ] `.rpi/settings.json`
- [ ] `defaultProvider`
- [ ] `defaultModel`
- [ ] CLI 正确解析配置
- [ ] 自定义 URL 真正生效
- [ ] custom provider 不再被硬编码 env-key 检查提前拒绝

## Phase 2：Session 文件持久化

- [ ] `.rpi/sessions/`
- [ ] session id
- [ ] JSONL persistence
- [ ] append entry
- [ ] flush
- [ ] load
- [ ] projection
- [ ] CLI 实际使用 file-backed session
- [ ] 进程重启后可以恢复

## Phase 3：Session / Agent Transcript

- [ ] Session 成为 source of truth
- [ ] Agent Context 可以从 Session 重建
- [ ] user message 一致
- [ ] assistant message 一致
- [ ] tool call 一致
- [ ] tool result 一致
- [ ] model change 一致
- [ ] usage 一致
- [ ] context edit 一致
- [ ] branch projection 一致

## Phase 4：Compaction

- [ ] overflow 使用统一 compaction
- [ ] manual / automatic compaction 共用实现
- [ ] Compaction 写入 Session
- [ ] 原始历史不被删除
- [ ] projection 可以恢复 compaction 状态
- [ ] 重启后 context 与之前一致

---

# 七、明确不要做

这次任务**不要**顺便实现：

- PermissionPolicy
- MCP `tools/list_changed`
- RPC Bash 重构
- TUI 跨平台
- Unicode width
- ~~Subagent~~（已立项，按 `docs/14-plugin-in-process.md` §4 设计实施）
- OAuth
- 完整 model catalog
- 大量新 provider
- Pi 全部 CLI 参数
- 大规模 crate 重构

如果发现这些问题：

> **记录即可，不要扩大本次修改范围。**

---

# 八、修改原则

## 1. 先读实际代码

重点搜索：

```text
create_session
SessionManager
Session
ModelResolver
resolve_provider_and_model
settings.json
base_url
Compaction
overflow
ContextEdit
```

先确认真实调用链，再修改。

---

## 2. 尽量复用已有抽象

优先复用：

```text
Model
ModelResolver
Session
SessionManager
Compaction
Projection
```

只补齐缺失连接。

---

## 3. Model 配置优先兼容 Pi

字段和层级尽量采用：

```text
providers
baseUrl
api
apiKey
headers
models
compat
```

不要创造另一套：

```text
providerUrl
modelUrl
endpoint
modelConfig
```

---

## 4. Secret 不进入 Git

真实：

```text
API key
token
password
```

不能写入：

```text
.rpi/models.json
```

示例使用：

```text
OPENAI_API_KEY
MY_API_KEY
```

---

## 5. 小步修改

每个 Phase 完成后运行：

```bash
cargo check --workspace
cargo test --workspace
cargo clippy --workspace
```

并增加对应 regression tests。

---

# 九、最终验收

## Case 1：自定义 OpenAI-compatible URL

创建：

```text
.rpi/models.json
```

```json
{
  "providers": {
    "local": {
      "baseUrl": "http://localhost:8080/v1",
      "api": "openai-completions",
      "apiKey": "LOCAL_API_KEY",
      "models": [
        {
          "id": "my-model"
        }
      ]
    }
  }
}
```

然后：

```bash
export LOCAL_API_KEY=xxx
rpi --provider local --model my-model "hello"
```

必须真正请求：

```text
http://localhost:8080/v1
```

而不是默认 provider URL。

---

## Case 2：Session 文件

运行：

```bash
rpi
```

之后必须存在：

```text
.rpi/
└── sessions/
    └── <session-id>.jsonl
```

---

## Case 3：重新加载

```text
进程 A
  ↓
产生完整对话
  ↓
退出

进程 B
  ↓
读取 session
  ↓
projection
```

必须恢复完整 context。

---

## Case 4：Overflow

```text
context overflow
      ↓
compaction
      ↓
Session JSONL 写入 Compaction
      ↓
Agent 继续
      ↓
进程退出
      ↓
重新启动
      ↓
读取 JSONL
      ↓
projection
```

必须得到正确的压缩后 context。

---

# 十、最重要的四句话

```text
1. Model / Provider / URL 配置要像 Pi 一样通过 JSON 配置，而不是只能写 CLI。
2. Session 必须真正持久化到 .rpi/sessions/*.jsonl，而不是只存在内存。
3. Session 是 source of truth，Agent Context 是 projection/cache。
4. Overflow 和 Compaction 必须使用同一套机制，并且 Compaction 必须持久化。
```
