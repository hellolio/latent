# 01 — 核心数据模型与类型系统

> **一句话**:pi 的类型系统分三层 —— `pi-ai/types.ts` 定义 LLM 消息/模型/流式事件(词汇表),`agent/types.ts` 定义 agent 运行时状态与事件(循环的接口),`coding-agent` 用 4 种自定义消息扩展 `AgentMessage` 联合;所有消息都带 `timestamp`,所有错误都编码为字段而非异常。

## 1. 消息类型(`packages/ai/src/types.ts`)

### 1.1 `Message` 联合(@ types.ts:577)

```
Message = SystemMessage | UserMessage | AssistantMessage | ToolResultMessage
```

**SystemMessage**(@ types.ts:515-531)—— pi 最独特的设计,prompt 与工具集都由它承载:

```ts
interface SystemMessage {
  role: "system";
  content: string | TextContent[];            // 首条=基础 prompt;后续=追加指令
  sections?: Record<string, string | null>;   // 命名节:字符串=替换,null=删除
  toolsAdded?: Tool[];                        // 从此刻起可用的工具(完整定义)
  toolsRemoved?: ToolReference[];             // 从此刻起移除的工具(仅名字)
  timestamp: number;
}
```

- 回放全部 system 消息 = 当前 prompt + 当前工具集。支持中途 system 消息的 provider 原样发送;不支持的 provider 把状态折叠回首条(`collapseSystemMessages` @ `packages/ai/src/utils/transcript.ts:108-112`)。
- `sections` 约定:每节自定界(带 XML 标签),名字用 `[a-z][a-z0-9_-]*`,避免整数型名字(JSON 对象会重排序)。
- 工具集变更走 system 消息意味着**转录本身记录了模型见过哪些工具**,`declareToolChanges`(见 03 文档)据此计算增量。

**UserMessage**(@ types.ts:533-537):`content: string | (TextContent | ImageContent)[]`。

**AssistantMessage**(@ types.ts:539-561):

```ts
interface AssistantMessage {
  role: "assistant";
  content: (TextContent | ThinkingContent | ToolCall)[];
  api: Api; provider: ProviderId; model: string;
  responseModel?: string; responseId?: string;   // provider 回报的具体模型/响应 id
  providerThinkingLevel?: string;                 // 实际使用的 provider 侧 effort
  diagnostics?: AssistantMessageDiagnostic[];     // 脱敏的失败/恢复诊断
  usage: Usage;
  stopReason: StopReason;
  deferred?: DeferredHandle;                      // 异步长请求句柄
  errorMessage?: string;                          // stopReason=error/aborted 时的错误文案
  rawStopReason?: string;                         // provider 原始 finish reason
  endTurn?: boolean;                              // 调试用,不影响控制流
  timestamp: number;
}
```

**ToolResultMessage**(@ types.ts:563-575):

```ts
interface ToolResultMessage {
  role: "toolResult";
  toolCallId: string;        // 对应 ToolCall.id
  toolName: string;
  content: (TextContent | ImageContent)[];
  details?: JsonValue;       // 结构化 UI/日志数据,不发给 LLM
  usage?: Usage;             // 工具自身消耗(如子 LLM 调用),不进主上下文核算
  isError: boolean;
  timestamp: number;
}
```

### 1.2 内容块

| 类型 | 行号 | 字段 | 说明 |
|---|---|---|---|
| `TextContent` | types.ts:388 | `text`, `textSignature?` | textSignature 供 OpenAI Responses 回放 |
| `ThinkingContent` | types.ts:394 | `thinking`, `thinkingSignature?`, `redacted?` | 签名是 provider 侧不透明回放数据;redacted 时 payload 在 signature 里 |
| `ImageContent` | types.ts:404 | `data`(base64), `mimeType` | |
| `ToolCall` | types.ts:410 | `id`, `name`, `arguments: JsonObject`, `thoughtSignature?`, `namespace?` | thoughtSignature 为 Google 专用;namespace 为 OpenAI Responses 命名空间工具 |

### 1.3 `StopReason` 与 `Usage`

- `StopReason`(@ types.ts:443)= `"pending" | "stop" | "length" | "toolUse" | "error" | "aborted" | "deferred"`。
  - agent 循环只对 `error`/`aborted` 硬退出;`length` 触发工具调用全拒防御(03 文档);`deferred` 是异步长任务挂起。
- `Usage`(@ types.ts:420-441):`input, output, cacheRead, cacheWrite, cacheWrite1h?, reasoning?(output 的子集), totalTokens, cost{input,output,cacheRead,cacheWrite,total}`。cost 单位是美元实际金额。

### 1.4 工具声明与约束采样

```ts
interface Tool<TParameters extends TSchema = TSchema> {   // @ types.ts:680
  name: string;
  description: string;
  parameters: TParameters;                                // TypeBox JSON Schema
  constrainedSampling?: false | ConstrainedSamplingConfig;
}
interface ToolReference { name: string }                  // @ types.ts:687
type ConstrainedSamplingConfig =                          // @ types.ts:670
  | { type: "json_schema"; strict: "prefer" | "require" }
  | { type: "grammar"; variants: Partial<Record<"openai_lark"|"openai_regex", string>> };
```

pi 全部内置工具声明 `constrainedSampling: { type: "json_schema", strict: "prefer" }`。

### 1.5 `AgentMessage` 扩展机制(`packages/agent/src/types.ts:361-370`)

```ts
type AgentMessage = Message | CustomAgentMessages[keyof CustomAgentMessages];
```

TS 声明合并扩展;实际注入方是 coding-agent 的 `packages/coding-agent/src/core/messages.ts`:

| 自定义消息 | 行号 | 用途 | 进 LLM 上下文? |
|---|---|---|---|
| `BashExecutionMessage` | messages.ts:29 | `!` 前缀的裸 shell 执行记录 | 经 convertToLlm 变 user 文本 |
| `CustomMessage` | messages.ts:46 | 扩展注入,`customType` 标识 | 变 user |
| `BranchSummaryMessage` | messages.ts:55 | 树导航时的分支摘要 | 带 XML 包裹的 user |
| `CompactionSummaryMessage` | messages.ts:62 | 压缩摘要 | 带 XML 包裹的 user |

coding-agent 的 `convertToLlm` @ messages.ts:148 负责把以上四类转换为标准 Message 或过滤。

## 2. Model 与 provider 词汇

```ts
interface BaseModel<TApi> {          // @ types.ts:1062
  id, name, api, provider, baseUrl,
  input: ("text"|"image")[],
  inputLimits?: { maxRequestBytes?, images?: { resize?, maxPerMessage?, maxPerRequest? } },
  cost: ModelCost,                   // 含分层定价 tiers(按 inputTokensAbove)
  headers?: Record<string,string>,
}
interface Model<TApi> extends BaseModel<TApi> {   // @ types.ts:1076
  reasoning: boolean;
  thinkingLevelMap?: Partial<Record<ModelThinkingLevel, string|null>>;  // pi 级别→provider 值
  promptCache?: Partial<Record<"short"|"long", number>>;                // 各档缓存寿命秒数
  contextWindow: number;
  maxTokens: number;
  samplingParams?: Record<string, unknown>;         // 默认采样参数
  compat?: OpenAICompletionsCompat | OpenAIResponsesCompat | AnthropicMessagesCompat | ...;
}
```

- `ThinkingLevel`(agent 侧)@ `packages/agent/src/types.ts:345` = `"off"|"minimal"|"low"|"medium"|"high"|"xhigh"|"max"`;ai 侧 @ types.ts:85 无 `off`。
- `DeferredHandle`(@ types.ts:493):`{provider, modelId, api, id, expiresAt?, pollAfterMs?, data?}` —— 异步长请求(如 15m/1h/24h 窗口)的可轮询句柄。
- 模型目录 `packages/ai/src/models.generated.ts`(309 行)由脚本生成,禁止手改;provider 工厂在 `packages/ai/src/providers/`(每个 provider 一对 `xxx.ts` + `xxx.models.ts`)。

## 3. agent 运行时类型(`packages/agent/src/types.ts`)

### 3.1 工具与结果

```ts
interface AgentTool<TParameters, TDetails> extends Tool<TParameters> {  // @ types.ts:443
  label: string;                                    // UI 标签
  prepareArguments?: (args: unknown) => Static<TParameters>;  // 校验前兼容垫片
  execute: (toolCallId, params, signal?, onUpdate?) => Promise<AgentToolResult<TDetails>>;  // 抛异常=失败
  replay?: "never" | "safe";                        // harness 持久化重放策略
  executionMode?: "sequential" | "parallel";        // 逐工具覆盖批执行模式
}
interface AgentToolResult<T> {                      // @ types.ts:420
  content: (TextContent | ImageContent)[];          // 给模型看的内容
  details: T;                                       // 给 UI 的结构化数据
  usage?: Usage;
  terminate?: boolean;                              // 全批均为 true 时提前结束 agent
}
type AgentToolUpdateCallback<T> = (partialResult: AgentToolResult<T>) => void;  // @ types.ts:440, settle 后调用被忽略
```

错误表达约定:**`execute` 抛异常,不在 content 里编码错误**(@ types.ts:451 注释)。

### 3.2 状态与上下文

```ts
interface AgentState {            // @ types.ts:378
  readonly systemPrompt: string;  // 从转录 system 消息重放得到,只读
  model: Model<any>;
  thinkingLevel: ThinkingLevel;
  tools: AgentTool<any>[];        // 赋值时拷贝顶层数组;与转录声明差异会公告给模型
  messages: AgentMessage[];
  readonly isStreaming: boolean;  // 直到 agent_end 的 awaited listener 结束才变 false
  readonly streamingMessage?: AgentMessage;
  readonly pendingToolCalls: ReadonlySet<string>;
  readonly errorMessage?: string;
}
interface AgentContext {          // @ types.ts:471
  messages: AgentMessage[];       // 模型可见转录
  tools?: AgentTool<any>[];       // 运行时可执行集
}
```

### 3.3 事件模型(10 种 `AgentEvent`,@ types.ts:485-500)

| 事件 | 载荷 | 时机 |
|---|---|---|
| `agent_start` | — | run 开始 |
| `agent_end` | `messages: AgentMessage[]`(本次 run 新增) | run 结束(最后事件,但 awaited listener 仍属结算) |
| `turn_start` | — | 每个 turn 开始 |
| `turn_end` | `message, toolResults` | assistant 消息 + 全部工具结果之后 |
| `message_start` | `message` | system/user/assistant/toolResult 消息进入转录 |
| `message_update` | `message, assistantMessageEvent` | **仅 assistant 流式**,携带原始 pi-ai 事件(含 delta) |
| `message_end` | `message` | 消息定稿 |
| `tool_execution_start` | `toolCallId, toolName, args` | 工具准备后 |
| `tool_execution_update` | `+ partialResult` | 工具的 onUpdate 回调 |
| `tool_execution_end` | `toolCallId, toolName, result, isError` | 工具结算(并行模式下按**完成序**;tool result 消息事件按**源序**补发) |

### 3.4 配置面(`AgentLoopConfig`,@ types.ts:189-338)

继承 `SimpleStreamOptions`(temperature/maxTokens/reasoning/cacheRetention/sessionId/transport/onPayload/onResponse/onProviderStreamEvent/deferred/thinkingBudgets…),另加:

| 成员 | 行号 | 契约 |
|---|---|---|
| `model`(必填) | 189 | — |
| `convertToLlm`(必填) | 218 | **不得抛/拒**;不可转换的消息直接过滤 |
| `transformContext?` | 240 | AgentMessage 级变换(剪枝/注入),不得抛 |
| `getApiKey?` | 250 | 按 provider 动态取 key(OAuth 过期 token);不得抛 |
| `finishTurn?` | 260 | turn 结束、`turn_end` 前;可返回 `{action:"continue"|"end"}` |
| `prepareRequest?` | 267 | **每次** provider 请求前(含第一次);可替换 context/model/thinkingLevel |
| `prepareNextTurn?` | 274 | `turn_end` 后、下一 turn 前;同上但可追加 messages |
| `getSteeringMessages?` | 289 | 每 turn 结束后轮询;**必须返回 [] 而不是抛** |
| `getFollowUpMessages?` | 302 | agent 将停止时轮询;同上 |
| `toolExecution?` | 313 | 默认 `"parallel"` |
| `beforeToolCall?` | 322 | 参数校验后;`{block:true}` 拦截执行 |
| `afterToolCall?` | 337 | 结算前;逐字段浅覆盖 result(无深合并) |

### 3.5 小型枚举

- `ToolExecutionMode` @ types.ts:47 = `"sequential" | "parallel"`
- `QueueMode` @ types.ts:55 = `"all" | "one-at-a-time"`(steering/followUp 队列的抽取模式)
- `StreamFn` @ types.ts:33-37:`(model, TranscriptContext, SimpleStreamOptions?) => AssistantMessageEventStream`;契约:不抛异常,失败编码进流。

## 4. 源码文件索引

| 文件(相对 pi 仓库根) | 行数 | 职责 | 关键符号 | 优先级 |
|---|---|---|---|---|
| `packages/ai/src/types.ts` | 1133 | LLM 层全部类型 | `KnownApi`:17, `KnownProvider`:39, `SystemMessage`:515, `AssistantMessage`:539, `ToolResultMessage`:563, `Message`:577, `Tool`:680, `Context`:697, `TranscriptContext`:711, `AssistantMessageEvent`:732, `Model`:1076, `StopReason`:443, `Usage`:420, `ToolCall`:410 | P0 |
| `packages/agent/src/types.ts` | 500 | agent 运行时类型 | `StreamFn`:33, `AgentLoopConfig`:189, `ThinkingLevel`:345, `AgentMessage`:370, `AgentState`:378, `AgentToolResult`:420, `AgentTool`:443, `AgentContext`:471, `AgentEvent`:485 | P0 |
| `packages/coding-agent/src/core/messages.ts` | 196 | 4 种自定义消息 + convertToLlm | `BashExecutionMessage`:29, `CustomMessage`:46, `BranchSummaryMessage`:55, `CompactionSummaryMessage`:62, `convertToLlm`:148 | P0 |
| `packages/ai/src/utils/event-stream.ts` | 110 | EventStream 实现 | `EventStream`:26, `AssistantMessageEventStream`:91 | P1 |
| `packages/ai/src/utils/transcript.ts` | 234 | 转录折叠/重放 | `normalizeContext`:30, `collapseSystemMessages`:108, `getCurrentTools`/`getCurrentSystemMessage` | P1 |
| `packages/agent/src/index.ts` | 152 | agent 包公开面 | 分组导出 | P2 |
| `packages/agent/src/stream-fn.ts` | 20 | 默认 streamFn 安装点 | `setDefaultStreamFn`:11 | P2 |
| `packages/ai/src/models.generated.ts` | 309 | 生成的模型目录 | 脚本生成,勿手改 | P2 |
| `packages/ai/src/utils/diagnostics.ts` | 47 | 诊断类型 | `AssistantMessageDiagnostic` | P2 |

阅读顺序:`ai/types.ts`(§1.1-1.5 消息部分)→ `agent/types.ts` → `messages.ts` → `event-stream.ts` → `transcript.ts`。
