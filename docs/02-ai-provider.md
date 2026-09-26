# 02 — pi-ai:多 provider 抽象层

> **一句话**:所有 provider 差异被压进两个机制 —— ①每个 API 协议一个适配器模块(统一导出 `stream`/`streamSimple`,输出统一的 `AssistantMessageEvent` 流);②数据驱动的 `compat` 配置字段(而非代码分支)。失败不抛异常、一律编码进流。

## 1. API 协议层(`packages/ai/src/api/`)

`KnownApi`(@ `packages/ai/src/types.ts:17-27`)是 10 种协议;`Api = KnownApi | (string & {})` 是**开放集合**,扩展可注册任意新 API 字符串。

| KnownApi | 适配器文件 | 行数 |
|---|---|---|
| `anthropic-messages` | `api/anthropic-messages.ts`(+`.lazy.ts` 延迟加载壳) | 1528 |
| `openai-completions` | `api/openai-completions.ts` | 1727 |
| `openai-responses` | `api/openai-responses.ts` + `openai-responses-shared.ts` | 398 + 796 |
| `openai-codex-responses` | `api/openai-codex-responses.ts` | 1697 |
| `azure-openai-responses` | `api/azure-openai-responses.ts` | 353 |
| `bedrock-converse-stream` | `api/bedrock-converse-stream.ts` | 1345 |
| `google-generative-ai` | `api/google-generative-ai.ts` + `google-shared.ts` | 471 + 515 |
| `google-vertex` | `api/google-vertex.ts` | 554 |
| `mistral-conversations` | `api/mistral-conversations.ts` | 948 |
| `pi-messages` | `api/pi-messages.ts` | 444 |

统一契约 `ProviderStreams`(@ types.ts:286-299):每个适配器模块导出 `stream()`(完整选项)与 `streamSimple()`(简化选项:加 `reasoning`/`toolChoice`/`deferred`/`thinkingBudgets`,@ types.ts:343-351);可选 `fetchDeferred`/`cancelDeferred`(异步长请求)。

### 1.1 输入:只有转录

- 公开入口接受 `Context { systemPrompt?, messages, tools? }`(@ types.ts:697-701);
- `normalizeContext()`(@ `packages/ai/src/utils/transcript.ts:30-34`)把 systemPrompt 与 tools 折叠成首条 system 消息,产出带 brand 的 `TranscriptContext`(@ types.ts:711-714)—— **provider 代码只见转录,不见散装字段**,类型系统保证 `Context` 无法误入 provider;
- 不支持中途 system 消息的 provider 用 `collapseSystemMessages`(transcript.ts:108-112)把所有 system 重放折叠为首条;`supportsMidConvoSystemMessages` 是 compat 开关。

### 1.2 输出:统一流协议

`AssistantMessageEvent`(协议与字段见 01 文档 §3.3 之前部分,定义 @ types.ts:732-748)。载体 `AssistantMessageEventStream` 是 `EventStream<AssistantMessageEvent, AssistantMessage>`(@ `packages/ai/src/utils/event-stream.ts:91-105`):

- `EventStream`(event-stream.ts:26-89)= 泛型 push/pull FIFO + `AsyncIterable` + 完成谓词(`isComplete(event)`) + `result()` 返回最终值;`end()` 唤醒所有等待者。
- 流协议规则(@ types.ts:719-731 注释):成功流 `start` 先行、`done` 终止;请求建立失败可直接 `error` 终止;`partial` 是**共享的"到目前为止"对象**而非事件时点快照;text/thinking 从 `*_start` 空串随 `*_delta` 增长,`*_end` 权威定稿;redacted thinking 可能在 start 时即完整、无 delta;toolcall 参数在 `toolcall_start` 时是 provider 相关的部分 JSON,`toolcall_delta` 携带后续增量。

## 2. Provider 与模型目录(`packages/ai/src/models.ts`,1250 行)

```ts
interface Provider<TApi> {          // @ models.ts:143
  id, name, baseUrl?, headers?,
  auth: ProviderAuth;               // 必有:即使纯 env 凭据也提供 apiKey 型 auth,用于报告"是否已配置"
  getModels(): Model[];             // 同步
  getAllModels?(); refreshModels?(); filterModels?();   // 动态目录 provider
  stream(); streamSimple();         // 委派给 API 适配器
  fetchDeferred?/cancelDeferred?; generateImages?; classify?;
}
interface Models {                  // @ models.ts:243 —— provider 集合运行时
  getProviders/getProvider/getModels/getModel/getAllModels/getModelsOfType;
  refresh();                        // 并发刷新动态目录
  checkAuth(); getAvailable(); getAuth();   // getAuth 解析 OAuth 刷新,失败抛 ModelsError{code:"oauth"|"auth"}
  stream/streamSimple/completeSimple;       // 便捷方法:解析 auth + normalizeContext + 委派 provider
}
```

- `KnownProvider`(@ types.ts:39-81)约 40 个 id(anthropic、openai、google、bedrock、github-copilot、xai、groq、cerebras、openrouter、vercel-ai-gateway、zai、mistral、minimax、moonshotai、huggingface、fireworks、together、baseten、qwen-token-plan 系列、xiaomi 系列、cloudflare-* 等)。
- provider 工厂在 `packages/ai/src/providers/`:每个 provider 一对文件(`xxx.ts` 工厂 + `xxx.models.ts` 模型表);`providers/all.ts`(190 行)聚合注册;`providers/faux.ts`(710 行)是测试用假 provider。
- 模型目录:`scripts/generate-models.ts` 从 `*.models.ts` 生成 `src/models.generated.ts`;另有 models-store 持久化与远端 catalog 刷新(`src/models-store.ts`, `src/model-catalog.ts`)。
- 兼容别名:`src/legacy-api-aliases.ts`(108 行)。

## 3. compat:数据驱动的 provider 差异

差异用模型目录里的 `compat` 字段表达,适配器读字段决定行为,不写 if(provider)。字段全集见 01 文档 §2 与源码:

| 接口 | 行号 | 代表字段 |
|---|---|---|
| `OpenAICompletionsCompat` | types.ts:754-831 | `supportsStore/DeveloperRole/ReasoningEffort/UsageInStreaming/FinishReason`、`maxTokensField`、`requiresToolResultName/AssistantAfterToolResult/ThinkingAsText`、**`thinkingFormat`(11 种变体)**、`thinkingTokenBudgetField`、`supportsMidConvoSystemMessages/ToolAdditions`、`supportsStrictMode`、`cacheControlFormat:"anthropic"`、`sessionAffinityFormat`、`zaiToolStream`、`vllmPriority` |
| `OpenAIResponsesCompat` | types.ts:834-855 | `supportsDeveloperRole`、`supportsAdditionalTools/ToolSearch`、`supportsExplicitPromptCacheMode`、`supportsMaxOutputTokens` 等 |
| `AnthropicMessagesCompat` | types.ts:858-920 | `supportsEagerToolInputStreaming`、`supportsCacheControlOnTools`、`supportsTemperature`、`forceAdaptiveThinking`、`allowEmptySignature`、`supportsStrictTools`、`supportsMidConvoEffort/SystemMessages/ToolChanges`、`allowedFallbackModels` |
| `BedrockCompat` | types.ts:923-926 | `supportsStrictMode` |
| `MistralConversationsCompat` | types.ts:929-932 | `supportsMidConvoSystemMessages` |

`thinkingFormat` 的 11 种:`openai | openrouter | deepseek | together | baseten | zai | qwen | chat-template | qwen-chat-template | string-thinking | ant-ling` —— 同一个"思考参数怎么放进请求体"的问题被完全数据化。

## 4. 重试、超时与 overflow 检测

### 4.1 provider 级重试(`packages/ai/src/utils/retry.ts`,242 行)

- `RetryPolicy = {enabled, maxRetries, baseDelayMs, maxAgentDelayMs=60000}`;harness 默认 `{enabled:true, maxRetries:3, baseDelayMs:1000}`(harness/config.ts:4-9)。
- `retryAssistantCall`(retry.ts:176-226):指数退避 `baseDelayMs * 2^(attempt-1)`,封顶 `maxAgentDelayMs`;**aborted 永不重试**;退避中 abort → 归一化为 aborted 消息。
- 可重试性:`isRetryableAssistantError`(retry.ts:237-242)按正则分类 —— 429/5xx/网络/WebSocket/流早断等 ~50 模式可重试;quota/billing/订阅限额不可重试。
- 回调:`onRetryScheduled/onRetryStart/onRetryFinished`。

### 4.2 overflow 检测(`packages/ai/src/utils/overflow.ts`,187 行)

- `isContextOverflow`(overflow.ts:136):识别 20+ provider 的"上下文超限"错误文案(含 z.ai 的**静默溢出**:usage.input > contextWindow;小米式 "length + 0 输出");
- `isRecoverableLength`(overflow.ts:178):可恢复的 length 截断;
- 上层(harness 的响应结算 / coding-agent 的 overflow 恢复)据此决定"压缩后重试"。

### 4.3 请求选项要点(`StreamOptions` @ types.ts:187-237)

- `onPayload`(可检查/替换请求体)、`onResponse`(HTTP 响应观察)、`onProviderStreamEvent`(归一化前的原始 provider 事件);
- `transport: "sse" | "websocket" | "websocket-cached" | "auto"`;`websocketConnectTimeoutMs`;
- `cacheRetention: "none"|"short"|"long"`(默认 short)+ `sessionId`(session 亲和/prompt cache);
- `samplingParams`:任意透传参数(llama.cpp/vLLM/SGLang 等),合并覆盖 `Model.samplingParams`;
- `maxRetryDelayMs` 封顶服务端要求的等待。

## 5. 认证(`packages/ai/src/auth/`,~2.4k 行)

- `auth/types.ts`(240 行):`ProviderAuth`、`Credential` 类型;
- `auth/credential-store.ts`(67 行):凭据持久化;
- `auth/resolve.ts`(188 行):把 auth 解析为可用 apiKey(含 OAuth 刷新);
- `auth/oauth/`:每个 provider 一个 OAuth 流 —— `anthropic.ts`(364)、`github-copilot.ts`(507)、`openai-codex.ts`(544)、`openrouter.ts`(311)、`xai.ts`(239)、`kimi-coding.ts`(296)、`meta.ts`(208)、`radius.ts`(403);通用件:`device-code.ts`、`pkce.ts`、`oauth-page.ts`;
- `env-api-keys.ts`(190 行):环境变量 → provider 凭据映射。

## 6. 其他工具函数(`packages/ai/src/utils/`)

| 文件 | 职责 |
|---|---|
| `validation.ts`(350) | TypeBox `Convert`+`Check`+JSON-Schema 强制转换;`validateToolArguments` |
| `json-parse.ts`(124) | 流式 toolcall 参数的**尽力 JSON 修复解析** |
| `estimate.ts`(117) | token 估算(chars/4 等) |
| `assistant-message-frame.ts`(490) | 流式帧编解码(harness 恢复用) |
| `provider-retry.ts`(125)、`abort.ts`/`abort-signals.ts` | 重试与中止辅助 |
| `error-body.ts`(149)、`sanitize-unicode.ts` | 错误体解析与净化 |

## 7. 源码文件索引

| 文件 | 行数 | 职责 | 关键符号 | 优先级 |
|---|---|---|---|---|
| `packages/ai/src/types.ts` | 1133 | 全部类型 | 见 01 文档 | P0 |
| `packages/ai/src/models.ts` | 1250 | Provider/Models 运行时 | `Provider`:143, `Models`:243 | P0 |
| `packages/ai/src/utils/transcript.ts` | 234 | 转录折叠/重放 | `normalizeContext`:30, `collapseSystemMessages`:108 | P0 |
| `packages/ai/src/utils/event-stream.ts` | 110 | 流载体 | `EventStream`:26 | P0 |
| `packages/ai/src/utils/retry.ts` | 242 | 重试策略 | `retryAssistantCall`:176, `isRetryableAssistantError`:237 | P0 |
| `packages/ai/src/utils/overflow.ts` | 187 | 溢出检测 | `isContextOverflow`:136, `isRecoverableLength`:178 | P1 |
| `packages/ai/src/utils/validation.ts` | 350 | 参数校验 | `validateToolArguments` | P1 |
| `packages/ai/src/api/openai-completions.ts` | 1727 | 最大适配器范本 | 兼容 thinkingFormat 11 变体 | P1(重写先做这个+anthropic) |
| `packages/ai/src/api/anthropic-messages.ts` | 1528 | Anthropic 适配器 | cache_control、thinking 签名 | P1 |
| `packages/ai/src/api/pi-messages.ts` | 444 | 最小适配器范本 | 适合作为"从零写一个适配器"的模板 | P1 |
| `packages/ai/src/auth/types.ts` / `resolve.ts` | 240/188 | 认证类型与解析 | `resolveAuth` | P2 |
| `packages/ai/src/auth/oauth/*.ts` | 98-544 | 各家 OAuth 流 | `pkce.ts`(34) 是公共底座 | P2 |
| `packages/ai/src/providers/all.ts` | 190 | provider 聚合注册 | — | P2 |
| `packages/ai/src/providers/faux.ts` | 710 | 测试假 provider | 测试范本 | P2 |
| `packages/ai/src/utils/json-parse.ts` | 124 | 流式参数修复解析 | — | P1 |
| `packages/ai/src/env-api-keys.ts` | 190 | 环境变量凭据 | — | P2 |
| `packages/ai/src/index.ts` | 48 | 公开面 | — | P2 |

阅读顺序:`types.ts` → `models.ts`(Provider/Models 接口)→ `transcript.ts` → 一个最小适配器(`pi-messages.ts`)→ 最大适配器(`openai-completions.ts`)→ `retry.ts`/`overflow.ts` → `auth/`。

## 踩坑记录

- **2026-09-25 AssistantMessage 挂 `serde(tag = "role")` 导致 JSONL roundtrip 失败**：现象：`Message::Assistant` 序列化出重复 `role` 键（struct 自带一份、enum 打标又一份），反序列化直接失败，rpi-session 持久化静默丢数据 → 原因：internally-tagged enum 的 newtype 变体内的 struct 不能再挂同名词别的 tag → 解法：`AssistantMessage` struct 只留 `rename_all = "camelCase"`，role 由外层 enum 补；并补 roundtrip 回归测试钉住（关联文件：`crates/rpi-ai/src/types.rs`、`crates/rpi-agent/src/message.rs`）。
- **2026-09-25 pi 的 Anthropic 流按 JSON `type` 字段分发，不按 SSE `event:` 字段**：现象：适配器按 SSE event 名分发导致全部块事件丢失，流以"无 stop reason"报错 → 原因：pi 的 `iterateAnthropicEvents` 只用 `event:` 字段识别 `error`，其余一律解析 JSON 后按 `data.type` 分发 → 解法：`handle_sse_event` 先解析 JSON 再取 `type`；写测试务必用与 pi 相同的 SSE 形态（`data:` 行不带 `event:` 名）（关联文件：`crates/rpi-ai/src/adapters/anthropic.rs`）。
- **2026-09-25 reqwest `header()` 是追加语义，用户自定义头会与默认头并存**：现象：调用方传 `authorization` 后请求出现两个同名头，网关拒绝 → 原因：`RequestBuilder::header` 逐个追加而非覆盖 → 解法：`build_header_map` 先装默认头再以用户头 `insert` 覆盖，一次 `.headers()` 发出（关联文件：`crates/rpi-ai/src/adapters/mod.rs`）。
- **2026-09-25 `Peekable::clone` 保留 peek 项，`\uXXXX` 修复校验错位**：现象：`repair_json` 把合法 `\u00e9` 修成 `\\u00e9` → 原因：peek 到 'u' 后 `chars.clone().take(4)` 仍以 'u' 开头，四位十六进制校验永远失败 → 解法：先 `chars.next()` 消费 'u' 再取四位（关联文件：`crates/rpi-ai/src/json_parse.rs`）。
- **2026-09-25 集成测试里的"缺 key"路径必须与宿主环境隔离**：现象：本机设了 `ANTHROPIC_API_KEY` 时 `anthropic_requires_api_key` 偶发失败 → 原因：适配器会走 env 回退真实发请求 → 解法：该用例改用不命中任何 env 映射的 provider 名，且此类用例不得 `await` 不会 accept 的服务器任务（关联文件：`crates/rpi-ai/tests/adapters.rs`、`crates/rpi-ai/src/env_keys.rs`）。
- **2026-09-25 M1 有意裁剪 → 2026-09-26 用户决策定案**：OAuth 凭据流、其余 8 个适配器、模型目录生成与 models-store **正式裁剪，不再补齐**（接入以 API key + 通用 OpenAI 风格为主）；`StreamOptions` 的 onPayload/onResponse/onProviderStreamEvent 观察回调与 deferred 句柄待实施（见 `docs/11-gap-closure-plan.md` T3）；openai 侧 tool result 图片转发为后随 user 消息、grammar/custom 工具、reasoning_details 回放按需（暂搁置）；thinkingFormat 实现 openai/openrouter/deepseek/zai/together/qwen 六种，其余五种（baseten/chat-template/qwen-chat-template/string-thinking/ant-ling）**维持降级为 openai 风格**（决策：有一个通用实现即可）（关联文件：`crates/rpi-ai/src/adapters/openai_completions.rs`、`docs/09-wiring-and-rust.md` B6）。
- **2026-09-26 async_stream 生成器在最后一个 yield 后的代码不会执行**：现象：RetryingProvider 的 `on_retry_finished` 收尾回调从未触发 → 原因：消费端收到终态事件即停止 poll，生成器挂起在终态 yield 处，yield 之后的收尾代码永远跑不到 → 解法：所有收尾回调（含深度/计数类）一律放在终态 yield **之前**触发；为"流无终态"的兜底路径保留调度器退出后的兜底上报（消费端必须再 poll 一次才能看到流结束，届时执行）。写流式装饰器时把"终态 yield 后还有代码"视为 bug 信号。（关联文件：`crates/rpi-ai/src/retry.rs`）
- **2026-09-26 空回复（无内容 delta 的 Done）经重试装饰丢失终态**：现象：MockProvider 空回复装配重试后被循环兜底改写成 `stopReason=error` → 原因：提交点定义为"首个内容 delta"，空回复全程不提交，Done 终态被持有后只在"非重试放行"分支 yield，Done-without-delta 分支漏发 → 解法：终态持有逻辑统一为"未提交 + Done = 放行缓冲帧 + yield 终态"；补空回复回归测试。同类错误：committed 路径的终态是内联 yield 的，判断"流无终态"必须用显式 `saw_terminal` 标志，不能用 `held_terminal.is_none()`（committed 时它恒为 None）。（关联文件：`crates/rpi-ai/src/retry.rs`、`crates/rpi-ai/tests/retry_streaming.rs`）
- **2026-09-26 T3 观察回调落地要点**：`StreamOptions` 新增 `on_payload`（发送前可检查/替换请求体，`&mut Value`）、`on_response`（HTTP 响应观察）、`on_provider_stream_event`（归一化前原始事件）；panic 一律 `catch_unwind` 吞掉不击穿流，但建议落一行诊断便于排障（todo 见 docs/11 交接清单）。`DeferredHandle` 纯类型已备，适配器侧异步长请求接入按需。（关联文件：`crates/rpi-ai/src/types.rs`、`crates/rpi-ai/src/adapters/mod.rs`、两个适配器、`crates/rpi-ai/tests/observations.rs`）
