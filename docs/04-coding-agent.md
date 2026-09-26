# 04 — coding-agent 业务层架构

> **一句话**:`main → cli → core → modes` 四层;`core/` 是所有模式共享的唯一业务层,巨类 `AgentSession`(4023 行)编排 Agent、会话树、模型运行时、扩展与缓存;系统提示词是**可增量更新的命名 sections 状态**,存在转录里而不是每请求重算的字符串。

## 1. 分层(`packages/coding-agent/src/`)

```
main.ts (985 行, main() @566)
  ├─ 解析参数/凭据/settings/project trust → 组装 runtime
  ├─ --mode rpc   → runRpcMode(runtime)        @ main.ts:936
  ├─ --mode print → runPrintMode(runtime, …)   @ main.ts:972 (非 TTY 自动进入)
  ├─ 交互          → new InteractiveMode(...)  @ main.ts:938
  └─ --mode json  → json 事件序列化模式
cli/    —— 纯 CLI 期逻辑(args/auth/session-picker/startup-ui/file-processor/initial-message/project-trust)
core/   —— 所有模式共享的唯一业务层(core/index.ts 注释 "Core modules shared between all run modes")
modes/  —— 四种 I/O 壳(interactive/print/json/rpc),只做渲染与输入
```

## 2. AgentSession(`src/core/agent-session.ts`,4023 行)

### 2.1 组成

配置 `AgentSessionConfig`(@220)持有:`Agent`(pi-agent-core)、`SessionManager`、`SettingsManager`、`ModelRuntime`、`ResourceLoader`、cacheWarmer、`initialActiveToolNames`(默认 `[read, bash, edit, write]`)、allowed/excludedToolNames、baseToolsOverride。

**消息队列三通道**(@344-350):`_steeringMessages` / `_followUpMessages`(string[],在 session 层自己维护一份,与 Agent 队列同步)、`_pendingNextTurnMessages` / `_pendingCustomMessages`。

### 2.2 与 pi-agent-core 的桥(三个安装器)

| 安装器 | 行号 | 把 agent 钩子映射到 |
|---|---|---|
| `_installAgentToolHooks` | @529 | `beforeToolCall`/`afterToolCall` → 扩展 `tool_call`/`tool_result` 事件 |
| `_installAgentRequestProjection` | @608 | 请求前投影上下文(扩展 `context` 事件等) |
| `_installAgentBoundaryHooks` | @675 | `turn_end` / `agent_before_settle` 边界让扩展追加 entry |

### 2.3 事件面

`AgentSessionEvent`(@164)在 10 种 `AgentEvent` 之上追加:`agent_settled`、`queue_update`、`compaction_start/end`、`auto_retry_start/end`、`entry_appended`、`bash_execution_update` 等 —— UI 只消费这一层。

### 2.4 关键方法

| 方法 | 行号 | 说明 |
|---|---|---|
| `prompt(text, options?)` | @1606 | slash 命令展开 / skill 展开 / 扩展 input 事件;流式中按 `streamingBehavior` 转 steer/followUp |
| `steer(text)` / `followUp(text)` | @1860 / @1872 | push 进 session 队列 + agent 队列 |
| `setActiveToolsByName(names)` | @1279 | 更改激活工具集(经 declareToolChanges 公告给模型) |
| `setModel()` | @2118 | 切模型(记 `model_change` entry) |
| `compact(customInstructions?)` | @2406 | 手动触发压缩 |
| `navigateTree(...)` | @3581 | 树导航(可带分支摘要) |

### 2.5 可靠性策略

- **自动重试**:`isRetryableAssistantError` 分类 + 指数退避,`auto_retry_start/end` 事件;
- **overflow 恢复**:context overflow → 自动 compaction → 重放被中断的 turn;`_overflowRecoveryAttempted`(@355)保证每次恢复只尝试一次,重置点在 @898/@949;
- **cache warmer**(`src/core/cache-warmer.ts`,453 行):会话空闲时用 `cacheRetention:"long"` 的轻请求保持上一条 prompt cache 条目温热,消耗记 `kind:"cache_warm"` 的 usage entry。

辅助:`src/core/agent-session-runtime.ts`(449 行,runtime 组装+诊断)、`agent-session-services.ts`(221 行,services 工厂)。

### 2.6 嵌入 SDK(`src/core/sdk.ts`,454 行)

`CreateAgentSessionOptions`(@41):cwd、agentDir、model、thinkingLevel、scopedModels、`noTools: "all"|"builtin"`、tools allowlist、excludeTools、customTools、resourceLoader…;入口 `createAgentSession()`(@175)。

## 3. 系统提示词(`src/core/system-prompt.ts`,216 行)—— sections 机制

### 3.1 构建输入(`BuildSystemPromptOptions`,@9-32)

`customPrompt`(整体替换默认前缀)、`forceSystemPrompt`(before_agent_start 处理器的整 prompt 覆盖)、`selectedTools`(默认 `[read, bash, edit, write]` @58)、`toolSnippets`/`toolGuidelines`(每个工具向 prompt 贡献一行片段 + 指南条目)、`promptGuidelines`、`appendSystemPrompt`、`sections`(自定义 XML 节)、`contextFiles`(AGENTS.md 等,渲染成 `<project_instructions path="...">`)、`skills`。

### 3.2 有序命名节(`buildSystemPromptSections`,@121-180)

产出 `SystemPromptSections = Record<string, string>`(@50):`preamble`(身份句,**无标签**)、`tools`、`rules`(@81-118:按 hasBash/hasGrep 等条件拼装 + "Be concise" + "Show file paths clearly")、`docs`(pi 自身文档导读)、`addendum`、`project_context`、`skills`、`cwd`,外加扩展自定义节;**除 preamble 外每节包 `<name>...</name>` 标签**(:175-178)。节名必须匹配 `/^[a-z][a-z0-9_-]*$/` 且不得叫 preamble(@52, :136-140)。

### 3.3 增量更新机制(重写的核心对照点)

- `buildSystemPromptState()`(@186-192):强制整 prompt → 存 `content`;结构化 → 存 `sections`;
- `diffSystemPromptSections()`(@204-216):对旧节 diff,产出 `SystemMessage.sections` patch(变更节 = 新文本,消失节 = `null`);
- 结合 01 文档的 SystemMessage 语义:**prompt 是 transcript 重放状态,不是每请求重算的字符串**;扩展改一节 → 追加一条带 patch 的 system 消息。

## 4. ToolDefinition:五合一设计(`src/core/extensions/types.ts:461`)

工具定义同时声明了 schema、执行、prompt 贡献、TUI 渲染与约束采样:

```ts
interface ToolDefinition<TParams, TDetails, TState> {
  name, label, description;
  promptSnippet?: string;          // 一行,进 system prompt 的 tools 节
  promptGuidelines?: string[];     // 条目,进 rules 节
  parameters: TypeBox TSchema;     // 参数 schema
  constrainedSampling?: ...;       // 内置工具统一 {type:"json_schema", strict:"prefer"}
  executionMode?: "sequential"|"parallel";
  prepareArguments?: ...;          // 校验前兼容垫片
  renderShell?: ...;
  execute(toolCallId, params, signal, onUpdate, ctx: ExtensionContext);
  renderCall?/renderResult?;       // TUI 渲染器
}
```

`tool-definition-wrapper.ts`(47 行)把 ToolDefinition 包装成 pi-agent-core 的 `AgentTool`(剥掉 UI 部分)。两套工厂:`createXxxToolDefinition`(带渲染)/ `createXxxTool`(纯执行),见 05 文档。

## 5. 配套子系统一览

| 模块 | 行数 | 职责 |
|---|---|---|
| `settings-manager.ts` | 1434 | 多层 settings(默认/全局/项目)合并与校验 |
| `model-resolver.ts` | 783 | 模型字符串解析、scoped models、fallback |
| `model-runtime.ts` | 886 | 模型/思考级别运行时状态与切换 |
| `model-registry.ts` / `model-config.ts` | 173 / 333 | 自定义 provider/model 注册与配置 |
| `resource-loader.ts` | 1167 | 扩展/skill/模板/主题等资源发现装载 |
| `package-manager.ts` | 2729 | pi 包(npm/git 分发的资源包)管理 |
| `skills.ts` | 509 | SKILL.md 发现与 frontmatter 解析 |
| `prompt-templates.ts` | 320 | 模板与参数替换(`$1/$@/$ARGUMENTS/${@:N:L}`) |
| `trust-manager.ts` / `project-trust.ts` | 245 / 96 | 项目信任决策 |
| `auth-storage.ts` | 506 | 凭据存储(auth.json) |
| `session-manager.ts` | 2010 | 会话树(见 06 文档) |
| `compaction/` | ~1.7k | 压缩(见 06 文档) |
| `cache-warmer.ts` | 453 | prompt cache 保温 |
| `bash-executor.ts` | 156 | `!` 裸 shell 执行(BashExecutionMessage 来源) |
| `export-html/` | ~700 | 会话导出 HTML |
| `config.ts`(src 根) | 579 | 资产路径 helper(三种安装形态兼容) |

## 6. 源码文件索引

| 文件 | 行数 | 职责 | 关键符号 | 优先级 |
|---|---|---|---|---|
| `packages/coding-agent/src/main.ts` | 985 | 入口与模式分派 | `main`:566, rpc 分派:936, print 分派:972, InteractiveMode:938 | P0 |
| `packages/coding-agent/src/core/agent-session.ts` | 4023 | **业务核心巨类** | `AgentSessionEvent`:164, `AgentSessionConfig`:220, `AgentSession`:328, 队列:344-355, `_installAgentToolHooks`:529, `_installAgentRequestProjection`:608, `_installAgentBoundaryHooks`:675, `setActiveToolsByName`:1279, `prompt`:1606, `steer`:1860, `followUp`:1872, `compact`:2406, `navigateTree`:3581 | P0 |
| `packages/coding-agent/src/core/sdk.ts` | 454 | 嵌入 SDK 公开面 | `CreateAgentSessionOptions`:41, `createAgentSession`:175 | P0 |
| `packages/coding-agent/src/core/system-prompt.ts` | 216 | 系统提示词 sections | `BuildSystemPromptOptions`:9, `buildSystemPromptSections`:121, `buildSystemPromptState`:186, `diffSystemPromptSections`:204 | P0 |
| `packages/coding-agent/src/core/messages.ts` | 196 | 自定义消息 + convertToLlm | `convertToLlm`:148 | P0 |
| `packages/coding-agent/src/core/extensions/types.ts` | 1972 | 扩展 API 与 ToolDefinition | `ExtensionContext`:319, `ToolDefinition`:461, `ExtensionEvent`:1185, `ExtensionAPI`:1365 | P0(本篇只看 ToolDefinition) |
| `packages/coding-agent/src/core/tools/tool-definition-wrapper.ts` | 47 | ToolDefinition → AgentTool | `wrapToolDefinition` | P0 |
| `packages/coding-agent/src/core/agent-session-runtime.ts` | 449 | runtime 组装 | — | P1 |
| `packages/coding-agent/src/core/settings-manager.ts` | 1434 | settings 体系 | — | P1 |
| `packages/coding-agent/src/core/model-resolver.ts` / `model-runtime.ts` | 783/886 | 模型解析与运行时 | — | P1 |
| `packages/coding-agent/src/core/cache-warmer.ts` | 453 | cache 保温 | — | P1 |
| `packages/coding-agent/src/core/resource-loader.ts` | 1167 | 资源装载 | — | P1 |
| `packages/coding-agent/src/core/skills.ts` / `prompt-templates.ts` | 509/320 | skill 与模板 | — | P1 |
| `packages/coding-agent/src/core/package-manager.ts` | 2729 | pi 包管理 | — | P2 |
| `packages/coding-agent/src/core/trust-manager.ts` / `project-trust.ts` | 245/96 | 信任决策 | — | P1 |
| `packages/coding-agent/src/core/auth-storage.ts` | 506 | 凭据存储 | — | P2 |
| `packages/coding-agent/src/core/index.ts` | 97 | core 导出面 | — | P2 |
| `packages/coding-agent/src/config.ts` | 579 | 资产路径 helper | — | P2 |

阅读顺序:`sdk.ts` → `system-prompt.ts` → `agent-session.ts`(按行号分段读)→ `agent-session-runtime.ts` → `settings-manager.ts`。

## 踩坑记录

- **2026-09-25 重试装饰器把"可重试"当成"成功"上报,AutoRetryEnd 信号颠倒**:`RetryingProvider` 最初把 `is_retryable_assistant_error` 的布尔值直接传给 `on_retry_finished(success, ..)` —— 成功时上报 false、耗尽重试仍可重试时上报 true。解法:success = 最终尝试没有 error 终态(`final_error.is_none()`),与 rpi-ai `retry_assistant_call` 的回调语义对齐。(关联文件:`crates/rpi-core/src/retry.rs`)
- **2026-09-25 setActiveToolsByName 重建系统提示词时静默清掉用户配置**:从已渲染的 sections 反推 `SystemPromptOptions` 只能拿回 cwd,`custom_prompt`(preamble)、`context_files`(project_context)、`append_system_prompt`(addendum)、`prompt_guidelines` 全部丢失,diff 会生成删除这些节的 patch 进转录。解法:`SessionRuntime` 保存原始 `SystemPromptOptions`,工具集变更时在其上只替换 tool_snippets/tool_guidelines 重建;`Forced` 整 prompt 状态不参与重建(只换工具集本身)。(关联文件:`crates/rpi-core/src/session.rs`)
- **2026-09-25 overflow 恢复测试"没跑到错误轮就结束了"**:多 turn 脚本的第一轮若是纯文本回复(无工具调用、无 steering),循环在内层即停止 —— 错误轮根本不会被请求,恢复逻辑测不到。多轮测试必须给中间 turn 挂工具调用或预置 steering 驱动循环继续。(关联文件:`crates/rpi-core/tests/session_integration.rs`)
- **2026-09-25 工具集变更公告依赖循环而非会话**:pi 的 `setActiveToolsByName` 靠 `declareToolChanges` 在下次注入时公告增量;Rust 版保持同一机制 —— 会话只更新 `context.tools`,转录内的 toolsAdded/toolsRemoved system 消息由循环的 `declare_tool_changes` 统一计算,core 不重复实现公告逻辑。(关联文件:`crates/rpi-agent/src/declare.rs`、`crates/rpi-core/src/session.rs`)
- **2026-09-25 有意保留的取舍**:①`RetryingProvider` 在重试场景必须缓冲事件(失败的尝试不能泄漏给 UI),当前对成功尝试也整体缓冲后回放 —— 装配重试装饰后所有请求退化为"终态可见"的伪流式;若要恢复逐 delta 流式,需把装饰器下探到 rpi-ai 的 SSE 帧级(失败即截断回放),M6 一并评估;②流式中 `prompt()` 按 pi 语义转 steer,返回值复用 `RunStop::EndTurn` 表达"已入队",调用方需以 `agent.is_streaming()` 区分,M5 引入独立 `PromptOutcome` 时修正;③Agent 的 `wait_idle` 以 10ms 轮询 streaming 标志(订阅者在 agent_end 事件内联完成,功能等价),watch/Notify 化待 run 生命周期 spawn 化时处理。
