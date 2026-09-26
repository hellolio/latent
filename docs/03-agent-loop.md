# 03 — 核心 agent 循环(packages/agent)

> **一句话**:整个系统的心跳是一个 898 行的 `agent-loop.ts` —— 双层 while 循环:内层"有工具调用或待注入消息就继续 turn",外层"follow-up / 显式 continue 唤醒";`error`/`aborted` 是唯一硬退出;工具执行分 prepare→execute→finalize→result 四阶段;`Agent` 类是有状态包装,负责队列、订阅与事件归约。
>
> **Rust 重写必读**:§10 是对 pi 循环的评估(6 条必须保留的不变量、8 条缺点)与结构性改善方案(显式状态机、工具结果完整性类型化、护栏内建、推送式注入、钩子链),与 09 的机制映射互补;§10.7 给出了与 09 B6 里程碑的合并计划。

## 1. 包结构

`packages/agent` 分两层:低层循环(`agent-loop.ts` + `agent.ts` + `types.ts`,内存态、事件驱动)与 harness(`src/harness/`,~25k 行的持久化/可恢复运行时,**实验性**,coding-agent 尚未使用 —— 见 08 文档附注)。本篇只覆盖低层。

## 2. 入口函数(`packages/agent/src/agent-loop.ts`)

| 函数 | 行号 | 作用 |
|---|---|---|
| `agentLoop(prompts, context, config, signal, streamFn)` | agent-loop.ts:37 | 新 prompt 启动;把 prompts 加入 context;返回 `EventStream<AgentEvent, AgentMessage[]>`(完成标记 = `agent_end` 事件,其 `messages` 为最终结果) |
| `agentLoopContinue(...)` | agent-loop.ts:70 | 重试/续跑:不加新消息;要求最后一条经 `convertToLlm` 转换后是 user 或 toolResult(agent-loop.ts:76-82 校验 role,assistant 抛错) |
| `runAgentLoop` / `runAgentLoopContinue` | agent-loop.ts:101 / 127 | async 版本,直接返回新增消息数组 `newMessages` |

`runAgentLoop` 的前置(agent-loop.ts:109-121):先对 prompts 做 `declareToolChanges`,发 `agent_start` → `turn_start` → 每条初始消息的 `message_start`/`message_end`,然后进 `runLoop`。

## 3. 主循环完整伪代码(`runLoop`,agent-loop.ts:162-320)

```
runLoop(initialContext, newMessages, initialConfig, signal, emit, streamFn):
  currentContext = initialContext
  config = initialConfig
  lastCompletedTurn = undefined            # PrepareNextTurnContext
  explicitContinuation = false
  pendingMessages = await config.getSteeringMessages?.() ?? []   # 循环开始即轮询一次(用户可能已在等待期输入)

  OUTER: loop:                             # 外层:agent 本应停止时,被 follow-up / 显式 continue 唤醒
    hasMoreToolCalls = true

    INNER: while (hasMoreToolCalls || pendingMessages.length > 0):
      preparedMessages = []
      if lastCompletedTurn != nil:                         # 非第一轮
        snapshot = await config.prepareNextTurn?.(lastCompletedTurn)     # :185
        if snapshot:
          currentContext = snapshot.context ?? currentContext
          preparedMessages  = snapshot.messages ?? []
          config.model     = snapshot.model ?? config.model
          config.reasoning = thinkingLevel 归一(snapshot.thinkingLevel, config.reasoning)   # :192-198,"off"→undefined
        if pendingMessages.isEmpty():                      # prepareNextTurn 可能耗时长(如 compaction),
          pendingMessages = await getSteeringMessages()    #   补轮询一次;仅当第一次轮询为空时才补,
                                                           #   避免 one-at-a-time 模式一轮注入两条 (:203-205)
        emit(turn_start)                                   # :206

      # ① 注入待处理消息(prepared + pending),注入前先 declareToolChanges 声明工具集增量
      for msg in declareToolChanges(currentContext, [...preparedMessages, ...pendingMessages]):   # :210
        emit(message_start, msg); emit(message_end, msg)
        currentContext.messages.push(msg); newMessages.push(msg)
      pendingMessages = []

      # ② 每次请求前(含第一次)的 prepareRequest 钩子:可替换 context/model/thinkingLevel (:218-238)

      # ③ 流式请求 LLM
      message = await streamAssistantResponse(currentContext, config, signal, emit, streamFn)   # :241
      newMessages.push(message)

      # ④ 硬退出:error / aborted —— 不执行工具、不轮询任何队列 (:244-255)
      if message.stopReason in {"error", "aborted"}:
        lastCompletedTurn = {message, toolResults: [], context, newMessages}
        await config.finishTurn?.(...)        # 仍会调用,但返回的决策被忽略
        emit(turn_end); emit(agent_end, newMessages); RETURN

      # ⑤ 工具调用 (:257-277)
      toolCalls = message.content.filter(type == "toolCall")
      toolResults = []; hasMoreToolCalls = false
      if toolCalls.length > 0:
        if message.stopReason == "length":
          # 输出被 token 上限截断:流式参数经"尽力 JSON 修复"定稿,可能静默不完整 →
          # 全部拒执行,逐一返回错误 tool result 让模型重发 (:263-269, 475-500)
          batch = failToolCallsFromTruncatedMessage(toolCalls, emit)
        else:
          batch = executeToolCalls(currentContext, message, config, signal, emit)
        toolResults = batch.messages
        hasMoreToolCalls = !batch.terminate   # terminate = 批内全部结果 terminate==true
        for r in toolResults: currentContext.messages.push(r); newMessages.push(r)

      # ⑥ turn 收尾 (:279-297)
      lastCompletedTurn = {message, toolResults, context, newMessages}
      decision = await config.finishTurn?.(lastCompletedTurn, signal)    # turn_end 之前调用
      emit(turn_end, message, toolResults)
      if decision?.action == "end": emit(agent_end); RETURN              # 直接结束,不碰队列
      explicitContinuation = (decision?.action == "continue")
      pendingMessages = await getSteeringMessages()                      # steering 注入点:每个 turn 结束后
      if hasMoreToolCalls || pendingMessages.length > 0:
        explicitContinuation = false       # 有自然请求时,"continue" 配额被自然消耗,不再额外发一次

    # —— 内层退出:agent 本应停止 ——
    followUps = await config.getFollowUpMessages?.() ?? []             # 只在将停止时轮询 (:301)
    if followUps.length > 0:
      explicitContinuation = false; pendingMessages = followUps; continue OUTER
    if explicitContinuation:                                           # 无自然请求时,
      explicitContinuation = false; continue OUTER                     # 用"仅上下文"的一轮兑现 continue
    break                                                              # 真正结束

  emit(agent_end, newMessages)                                          # :319
```

关键状态变量:`currentContext`(可被钩子整体替换)、`config`(同样可替换,模型/thinking 可逐轮切换)、`hasMoreToolCalls`、`pendingMessages`(steering 与 follow-up 共用的注入通道)、`explicitContinuation`。

**消息注入点固定三处**:①循环开始(:175);②每个 turn 结束后(:294,steering);③agent 将停止时(:301,follow-up)。②③之后的 `prepareNextTurn` 补轮询(:203-205)只在前次为空时进行 —— 这是 one-at-a-time 防双注入的细节。

## 4. 流式处理(`streamAssistantResponse`,agent-loop.ts:380-466)

```
messages   = config.transformContext ? await transformContext(context.messages, signal) : context.messages
llmMessages= await config.convertToLlm(messages)          # AgentMessage[] → Message[] (:394)
llmContext = normalizeContext({ messages: llmMessages })  # 折叠 systemPrompt/tools 进首条 system (:396)
apiKey     = (await config.getApiKey?.(model.provider)) || config.apiKey   # 过期 token 支持 (:399-400)
response   = streamFn(config.model, llmContext, {...config, apiKey, signal})   # :402

partialMessage = null; addedPartial = false
for event in response:
  "start":    partial = event.partial; context.messages.push(partial); addedPartial = true
              emit(message_start, {...partial})
  "*_start|*_delta|*_end" (text/thinking/toolcall):
              partial = event.partial
              context.messages[len-1] = partial        # 原地替换"活"消息 (:431)
              emit(message_update, {assistantMessageEvent: event, message: {...partial}})
  "done"|"error":
              final = await response.result()          # 流的最终 AssistantMessage (:442)
              context.messages[len-1] = final (或 push)
              if !addedPartial: emit(message_start)
              emit(message_end, final); return final
# for-await 意外正常退出的兜底:也取 result() 并补发 start/end (:457-465)
```

要点:**partial 消息直接占据 `context.messages` 末位被原地替换**,`done/error` 才换最终消息;`message_update` 携带原始 `AssistantMessageEvent`(含 delta)供 UI 增量渲染。

## 5. 工具执行

### 5.1 模式选择(`executeToolCalls`,agent-loop.ts:505-520)

`config.toolExecution === "sequential"` **或** 批内任一工具 `executionMode === "sequential"` → 串行;否则并行。

### 5.2 串行(`executeToolCallsSequential`,agent-loop.ts:527-581)

逐个:`tool_execution_start` → prepare → execute → finalize → `tool_execution_end` → 生成 ToolResultMessage → `message_start`/`message_end`(:895-898);每步后 `signal.aborted` 则 break(剩余调用无结果消息)。

### 5.3 并行(`executeToolCallsParallel`,agent-loop.ts:583-657)

- 顺序执行 `tool_execution_start` + prepare;immediate 结果就地落定;prepared 调用包装成 thunk,abort 时 thunk 直接产出 "Operation aborted" 错误结果(:616-624);
- `Promise.all` 并发执行全部 thunk → `tool_execution_end` **按完成顺序**发出;随后 tool-result 消息事件**按 assistant 源顺序**补发(:643-651)—— 此语义在 types.ts:39-46 注释中明确。

### 5.4 单工具四阶段

1. **prepare**(`prepareToolCall`,agent-loop.ts:703-771):按名找工具,找不到 → immediate 错误结果;可选 `tool.prepareArguments` 兼容垫片(:689-701);`validateToolArguments`(pi-ai validation)校验,失败 → immediate 错误;`beforeToolCall` 钩子 —— `{block:true}` → 错误结果(可带 terminate),abort → immediate 中止结果;全程多处检查 signal。
2. **execute**(`executePreparedToolCall`,agent-loop.ts:773-814):`tool.execute(toolCallId, args, signal, onUpdate)`;`onUpdate(partialResult)` 产生 `tool_execution_update`,以 promise 列表收集、settle 前 `Promise.all` 等待(保证事件顺序);工具抛异常 → 捕获转错误 AgentToolResult。
3. **finalize**(`finalizeExecutedToolCall`,agent-loop.ts:816-861):`afterToolCall` 钩子可**逐字段浅覆盖** content/details/usage/terminate/isError(:840-848,无深合并);钩子自身抛错 → 整体转错误结果。
4. **result**(`createToolResultMessage`,agent-loop.ts:880-893):`{role:"toolResult", toolCallId, toolName, content: result.content ?? [], details, usage, isError, timestamp}`。

### 5.5 提前终止(`shouldTerminateToolBatch`,agent-loop.ts:685-687)

批非空且**每个** finalized 结果 `terminate === true` 时 `batch.terminate = true`,内层循环因此退出 —— 这是"任务完成信号"(block 场景的 terminate 也能参与,:739-749)。

### 5.6 length 截断防御(`failToolCallsFromTruncatedMessage`,agent-loop.ts:475-500)

`stopReason === "length"` 时:对该消息全部 tool call 发 `tool_execution_start` + 错误结果("…may be truncated. Re-issue the tool call with complete arguments."),**terminate: false**(继续循环,让模型重发)。

## 6. 工具集声明(`declareToolChanges`,agent-loop.ts:332-362)

`context.tools` 是"运行时可执行集";转录 system 消息声明"模型可见集"。每次注入消息前,用 pi-ai 的 `getCurrentTools`(重放全部 system 消息的 toolsAdded/toolsRemoved)与 `(context.tools ?? []).map(toToolDeclaration)` 求差集,得出 `toolsAdded`/`toolsRemoved`:

- pending 消息中已有 system 消息 → 视其工具字段为"意图",替换为计算出的增量(:352-356);
- 否则新建 system 消息,插到第一个非 system 的 pending 消息之前(:357-361);
- 无变化则原样返回。
- 重放后模型可见工具集恒等于 `context.tools`。

## 7. Abort 语义

- `signal` 贯穿循环;工具 prepare/execute 前多处检查 `signal.aborted`,串行批 abort 后 break,并行 thunk abort 直接产出错误结果。
- `stopReason === "aborted"` 与 `"error"` 一样是**硬退出**(:244-255):不执行工具、不轮询 steering/follow-up。
- 钩子(beforeToolCall 等)自己负责响应 signal(types.ts:322、337 注释)。

## 8. `Agent` 类(`packages/agent/src/agent.ts`,613 行)= 循环的有状态包装

### 8.1 公开 API

| 成员 | 行号 | 说明 |
|---|---|---|
| `subscribe(listener)` | agent.ts:266 | listener 按**订阅顺序串行 await**,是 run 结算的一部分;`agent_end` 后所有 listener 结束才算 idle |
| `get state()` | agent.ts:276 | `AgentState`(见 01 文档) |
| `steer(msg)` / `followUp(msg)` | agent.ts:299 / 304 | 两条队列;另有 `clearSteeringQueue/clearFollowUpQueue/clearAllQueues`、`hasQueuedMessages`、`peekQueuedMessages`(steering 优先,:330-333) |
| `steeringMode` / `followUpMode` | — | `"all" | "one-at-a-time"`;**Agent 默认两者都是 one-at-a-time**(:247-248;harness 默认 "all") |
| `get signal()` / `abort()` | agent.ts:341 | 触发当前 run 的 AbortController |
| `waitForIdle()` | agent.ts:350 | |
| `reset()` | agent.ts:355 | 保留重放后的首条 system 消息作 baseline;activeRun 存在时抛错 |
| `prompt(input)` | agent.ts:371-381 | activeRun 存在时抛错("用 steer/followUp 或等待");string → `[{role:"user", content:[{type:"text",...images}], timestamp}]`(:413-430) |
| `continue()` | agent.ts:384-411 | 最后一条是 assistant 时先耗 steering 队列(设 `skipInitialSteeringPoll` 防循环开头再轮询),再耗 follow-up;都为空则抛错;否则纯续跑 |

### 8.2 队列(`PendingMessageQueue`,agent.ts:143-174)

`mode: "all" | "one-at-a-time"`;`drain()` 按 mode 取全部或最旧一条。

### 8.3 run 生命周期(agent.ts:507-556)

`activeRun = {promise, resolve, abortController}`;`isStreaming = true`;executor 结束后 `finishRun()` 复位并 resolve。**异常兜底** `handleRunFailure`(agent.ts:532-548):若循环本身抛异常(契约要求钩子不得抛,但兜底仍需要),合成一条 `stopReason: "aborted"|"error"`、`usage: EMPTY_USAGE`、空 text content 的 assistant 消息,依次发 `message_start → message_end → turn_end → agent_end`(`agent_end.messages` 只含这一条)。

### 8.4 事件 reducer(`processEvents`,agent.ts:565-612)

`message_start/update` → 置 `state.streamingMessage`;`message_end` → 清空并 **push 进 `state.messages`**(注意:Agent 的公开 messages 是事件驱动的;loop 内部操作的是 run 开始时的快照 slice);`tool_execution_start/end` → 维护 `pendingToolCalls: Set<string>`;`turn_end` → assistant 带 `errorMessage` 时记入 `state.errorMessage`;`agent_end` → 清 streamingMessage。然后携带当前 run 的 signal 逐个 await listener。

### 8.5 重试语义(重要)

低层**无内建重试**。错误以 `stopReason:"error"` 的 AssistantMessage 进入转录,循环硬退出;重试是调用方的职责 —— `agentLoopContinue` / `Agent.continue()`,且要求最后一条转换后是 user/toolResult(provider 级重试在 pi-ai 的 `retryAssistantCall`,由 harness 与 coding-agent 层使用,见 02/04 文档)。

## 9. 源码文件索引

| 文件 | 行数 | 职责 | 关键符号 | 优先级 |
|---|---|---|---|---|
| `packages/agent/src/agent-loop.ts` | 898 | **主循环 + 工具执行(全部)** | `agentLoop`:37, `agentLoopContinue`:70, `runAgentLoop`:101, `runLoop`:162, `declareToolChanges`:332, `streamAssistantResponse`:380, `failToolCallsFromTruncatedMessage`:475, `executeToolCalls`:505, `executeToolCallsSequential`:527, `executeToolCallsParallel`:583, `shouldTerminateToolBatch`:685, `prepareToolCall`:703, `executePreparedToolCall`:773, `finalizeExecutedToolCall`:816, `createToolResultMessage`:880 | P0 |
| `packages/agent/src/agent.ts` | 613 | Agent 类(状态/队列/订阅/run 生命周期) | `AgentOptions`:114, `PendingMessageQueue`:143, `subscribe`:266, `steer`:299, `followUp`:304, `abort`:341, `reset`:355, `prompt`:371, `continue`:384, `handleRunFailure`:532, `processEvents`:565 | P0 |
| `packages/agent/src/types.ts` | 500 | 运行时类型 | 见 01 文档索引 | P0 |
| `packages/agent/src/stream-fn.ts` | 20 | 默认 streamFn 模块级安装点 | `setDefaultStreamFn`:11, `getDefaultStreamFn`:16 | P1 |
| `packages/agent/src/index.ts` | 152 | 公开导出面 | 分组导出(循环/harness/proxy/search) | P1 |
| `packages/agent/src/proxy.ts` | 406 | streamFn 的代理实现(POST /api/stream + SSE,客户端重建 partial) | `streamProxy`:120 | P2 |
| `packages/agent/src/search/index.ts` | 27 | 会话检索宿主注入点接口 | `SessionSearchService` | P2 |
| `packages/agent/src/node.ts` | 2 | NodeExecutionEnv 导出(harness) | — | P2 |
| `packages/ai/src/utils/event-stream.ts` | 110 | EventStream(循环的返回流载体) | `EventStream`:26 | P0 |

阅读顺序:`agent-loop.ts` 全文 → `agent.ts` 全文 → `types.ts` 对照 → `event-stream.ts`。harness 子目录(~25k 行)见 08 文档附注,主线路径无需阅读。

## 10. 评估与 Rust 重写:优点、缺点与结构性改善

> **一句话**:pi 循环设计在 TS 生态属上乘——转录中心、无隐藏行为、契约明确;缺点一半是"单人项目没来得及做的护栏"(预算/重试/checkpoint),一半是"动态语言给不了的类型保证"(穷尽事件、状态机、完整性不变量)。Rust 重写不应逐行翻译:§10.3 的三个结构性改善(显式状态机、工具结果完整性类型化、护栏内建)才是重写的核心理由。机制层的语言映射(LoopHooks trait、partial buffer、JoinSet 保序等)已由 09 文档 B2/B3 定案,本节不重复,只覆盖 09 未涉及的评估与结构决策。

### 10.1 必须保留的不变量(Rust 版的验收标准)

| # | 不变量 | pi 出处 | Rust 版验证方式 |
|---|---|---|---|
| I1 | 单一转录真相:UI/compaction/投影从同一份 `context.messages` 出 | :415-447 partial 占位 | 类型上只有一个 `Vec<AgentMessage>` 所有者 |
| I2 | 无隐藏行为:低层无内建重试/自动 savepoint,错误进转录 | §8.5 | 错误模型显式(§10.4),重试在 rpi-ai 层 |
| I3 | `error`/`aborted` 唯一硬退出,不碰任何队列 | :244-255 | `Phase::Done(Aborted/Error)` 不经过注入通道 |
| I4 | 并行保序双语义:`tool_execution_end` 按完成序、tool-result 消息按源序 | :643-651 | mpsc 收完成序 + 有序 Vec 发源序(09 B2 已定) |
| I5 | length 截断消息的全部 tool call 拒执行,让模型重发 | :475-500 | 保留,并加防振荡计数(§10.4) |
| I6 | 重放 system 消息的工具声明后,模型可见集恒等于 `context.tools` | §6 | property test |

### 10.2 pi 的缺点(按严重程度)

1. **工具结果不完整(最接近 bug 的一类)**:串行批 abort 后 `break`,剩余 tool call 无 result 消息(§5.2,:527-581)。转录出现"N 个 toolCall、M 个 toolResult"缺口——Anthropic 严格配对校验会直接拒绝下一个请求。恢复路径(`agentLoopContinue`)能否走通取决于 provider 宽容度,靠运气而非设计。
2. **主循环是 160 行隐式状态机**:5 个可变变量 + 双层 while 编码状态。`explicitContinuation` 配额消耗(:283-286)与 one-at-a-time 防双注入(:203-205 **仅前次为空才补轮询**)都是聪明但脆弱的补丁,每个补丁靠注释解释自己;steering 与 follow-up 共用 `pendingMessages` 一条通道,来源类型丢失;只能集成测试。
3. **注入是轮询制非推送制**:`getSteeringMessages()` 只在三个固定点轮询(:175/:294/:301);长工具执行期间的用户输入要等当前 turn 完全结束才被看到;轮询制正是缺点 2 各补丁的成因。
4. **双份真相**:Agent 公开 `messages` 是事件归约(agent.ts:565),循环操作 run 开始时的快照 slice。reducer 漏处理新事件类型即静默分叉——TS union 不强制穷尽匹配。
5. **无内建护栏**:无 max-turns、无 token 预算、无墙钟超时;终止条件全部外置(abort 或宿主在 `finishTurn` 里数轮数)。length 截断重发(:263-269,terminate:false)无防振荡计数,上下文卡在边界时理论上可无限循环。
6. **契约靠约定不靠类型**:"钩子不得抛异常"是注释契约,`handleRunFailure`(agent.ts:532-548)只能事后合成假 assistant 消息兜底,转录已不干净;12 个可选回调挤一个 config(types.ts:189),多扩展共享钩子的顺序/组合语义在低层空白;`afterToolCall` 浅覆盖(:840-848)语义模糊(覆盖 vs 替换、嵌套 details)。
7. **错误模型太薄**:`stopReason` 仅 error/aborted 二值 + `errorMessage` 字符串。429 与断连、上下文超限与内部错误在循环眼里同物,循环层无法差异化决策(退避/换 provider/裁剪重试),上推给调用方而调用方信息又不足。
8. **transcript 混入控制面**:工具集变更编码为转录内 system 消息(§6)。模型侧统一,但投影/compaction/fork 必须小心避开;重放求差集每次注入 O(n)。**这是取舍点非缺陷**——若要会话文件与 pi 逐字节兼容则保留原方案。

### 10.3 结构性改善一:循环改为显式状态机(对应缺点 2/4)

```rust
enum Wake { Steering(AgentMessage), FollowUp(AgentMessage), ExplicitContinue }

enum Phase {
    AwaitingRequest { wake: Option<Wake> },   // 即将发请求
    Streaming,                                 // LLM 流式中
    ExecutingTools { batch: ToolBatch },
    Settling { turn: CompletedTurn },          // finishTurn 决策点
    Done(Outcome),                             // End | Aborted | Error(e)
}

async fn step(state: &mut LoopState, phase: Phase) -> Phase;  // 纯转移函数
```

- 双层 while 拆成 enum + transition,循环体退化为调度器;`step` 可脱离 tokio 单测。
- 防双注入、continue 配额补丁消失:三种 `Wake` 是不同类型,注入路径只有一条,配额语义变成 `ExplicitContinue` 是否入队的显式判断。
- 双份真相的解法:状态只有 `LoopState` 一份,Agent 公开视图从事件导出,事件由状态转移**必然**产生;Rust 穷尽匹配保证新事件类型编译期逼出 reducer 分支。

### 10.4 结构性改善二:完整性 + 护栏内建(对应缺点 1/5/7)

工具结果完整性用类型保证:

```rust
enum ToolOutcome { Completed(AgentToolResult), Cancelled, Blocked(String) }
// execute_batch 返回 Vec<ToolOutcome>,长度恒等于 toolCalls 数
// → "每个 toolCall 恰好一个 toolResult" 成为类型不变量 + property test
```

护栏内建(新增可区分的终止原因,不伪装成 error):

```rust
struct TurnLimits {
    max_turns: Option<u32>,
    max_tool_calls: Option<u32>,
    max_total_tokens: Option<u64>,
    deadline: Option<Instant>,
    max_truncation_retries: u32,   // length 截断重发防振荡
}

enum StopReason { EndTurn, ToolUse, Length, Aborted,
    Error(LoopError), BudgetExhausted(BudgetKind) }

enum LoopError {
    Provider { status: u16, retryable: bool },  // 429/5xx → 退避重试(rpi-ai 层,对应 pi 的 retryAssistantCall)
    ContextOverflow,                            // → 触发 compaction 重试
    Cancelled,
    Hook(HookError),
    Transport(..),
}
```

循环层首次有能力做差异化决策;同时把"断点续跑"做进低层:每个 completed turn 本来就是天然 checkpoint(§3 `lastCompletedTurn`),加 `CheckpointStore` trait,崩溃恢复即不依赖 harness 的 25k 行。

### 10.5 结构性改善三:注入通道改推送 + 钩子可组合(对应缺点 3/6)

推送式注入(steering/follow-up 用 `tokio::sync::mpsc`,循环内 `select!`):

```rust
select! {
    evt = stream.next() => ..,
    msg = steering_rx.recv() => pending.push(msg),
    _ = cancel.cancelled() => ..,
}
```

轮询点、补轮询、防双注入全部消失。真正的 mid-turn 注入仍做不到(请求已发出,provider 限制非语言限制),但可在工具执行期间收到 steering 时**取消当前工具**提前结束 turn——pi 做不到,因为它轮询不到。

钩子从单 config 对象改为可组合中间件链(多扩展共享时的顺序/短路成为显式契约):

```rust
trait Hook: Send + Sync {
    async fn before_tool_call(&self, ctx: BeforeToolCtx) -> HookFlow; // Proceed | Block(..)
}
// Vec<Arc<dyn Hook>> 按优先级链式 fold;首个 Block 短路;afterToolCall 的"浅覆盖"改为显式 Patch 合并规则
```

注意:09 B3 的单 `LoopHooks` trait 对应单宿主场景,多扩展需求出现时(M4/M5)演进为本节链式方案,二者兼容(单宿主 = 长度为 1 的链)。

### 10.6 Rust 侧要付的代价

- **partial 占位行为变更**:TUI 依赖读转录末位渲染;已定改为 buffer + 定稿 push(09 B5.2)。事件负载二选一需明确:delta-only + 另给 `Arc<RwLock<>>` 快照读口(推荐),或每 delta 带整个 partial 快照。
- **Agent 共享可变 state**:不用 `Arc<RwLock<AgentState>>`(锁顺序问题),改为 Agent 拥有状态、循环经事件单向写、查询返回 Clone 快照——牺牲读性能换掉双份真相。
- **扩展作者门槛**:TS 用户一行 JS 挂钩子,Rust 编译期注册筛掉非程序员作者——09 B4 的 WASM 路线因此是终局而非可选项,编译期只是 M4 前的脚手架。

### 10.7 与 09 B6 里程碑的合并建议

| 阶段 | 增量(在 09 B6 基础上) |
|---|---|
| M2 | 循环即按 §10.3 状态机结构写(此时改结构成本最低);`Vec<ToolOutcome>`(§10.4)同步定返回类型;`TurnLimits` + `StopReason`/`LoopError` 枚举 M2 就定义(哪怕只实现 deadline)——它们决定 `stopReason` 类型,后改要动所有 match |
| M2+ | I1-I6 不变量写成 property test(尤其 I6 工具声明重放、工具结果配对) |
| M4/M5 | 推送式注入(§10.5)+ 钩子链,届时以真实扩展需求验证接口 |

**取舍待定点**(2026-09-26 决策):① 控制面出转录**维持现状**(与 pi 一致;已决定不要求与 pi 会话文件双向兼容);② 事件负载定为 **delta 为主 + 快照读口**(文档推荐方案,待实施,见 `docs/11-gap-closure-plan.md` T2);③ `CheckpointStore` **推迟**,本批不做。

## 踩坑记录

- **2026-09-25 ScriptedProvider 脚本被倒序消费,多轮测试"看似随机"卡死**:现象是集成测试中 spawn 的 run 任务一次 poll 跑完整个 run、主任务永远看不到 is_streaming。原因:`ScriptedProvider` 用 `Vec::pop()` 取脚本 turn(取的是队尾),第一个被消费的 turn 恰是无延迟项,流从不挂起;而单 turn 测试只含延迟项所以通过。解法:脚本容器改 `VecDeque` + `pop_front()`(FIFO)。(关联文件:`crates/rpi-ai/src/mock.rs`)
- **2026-09-25 串行批 abort 后转录出现 toolCall/toolResult 缺口**:pi 的串行批在 abort 后 `break`,剩余 tool call 没有结果消息(03 文档 §10.2.1 指出的最接近 bug 的缺陷)。Rust 版以 `Vec<ToolOutcome>` 类型不变量修复:abort 后剩余调用一律以 `Cancelled` 结算并生成 "Operation aborted" 错误结果;工具批执行完成后再检查 cancel → run 以 `RunStop::Aborted` 硬退出。专项测试 `serial_batch_cancel_midway_still_pairs_all_results`。(关联文件:`crates/rpi-agent/src/loop_.rs`)
- **2026-09-25 预算检查位置导致已 drain 的 steering 消息静默丢失**:预算检查最初放在 `prepare_next_turn`/steering 补轮询与 `TurnStart` 之后 —— 命中时队列已被 drain、TurnStart 已发出却没有配对的 TurnEnd。解法:预算检查移到 while 循环体最前(drain 之前)。(关联文件:`crates/rpi-agent/src/loop_.rs`)
- **2026-09-25 thunk panic 会击穿 JoinSet 的配对保证**:`JoinSet::join_next` 对 panic 只返回 `JoinError`,拿不到对应 toolCall,无法发配对的 `tool_execution_end`。解法:thunk 内部用 `AssertUnwindSafe(..).catch_unwind()` 自捕获,panic 转错误 `ToolOutcome` 并正常发 end 事件。(关联文件:`crates/rpi-agent/src/loop_.rs`)
- **2026-09-25 有意保留的取舍(对照 03 §10.3/§10.5)**:①循环控制流仍按 §3 伪代码的双层 while 实现,`Phase` 枚举目前仅作观察/断言用途,§10.3 的完整 step 函数状态机推迟到推送式注入(M4/M5)一并落地 —— 届时注入通道改 mpsc,`pending` 单通道与 explicitContinuation 补丁才会被真正移除;②`message_update` 只在终态携带完整快照,流式中文本增量走 `MessageDelta`,thinking/toolCall 的逐块增量转发待 TUI(05)需要时扩展 rpi-ai 事件负载;③`validate_arguments` 是 JSON Schema 子集(type/required/嵌套 properties,`integer` 接受任意 number),待引入 jsonschema crate 后替换。
- **2026-09-26 T5 落地：循环重写为 Phase 状态机 + mpsc 推送注入**：`pending`/`explicit_continuation` 补丁符号已消失；`Wake` 三类型（Steering/FollowUp/ExplicitContinue）+ `step_*` 转移函数。要点：①空工具批必须 `terminate=true`（pi 的 hasMoreToolCalls=false），否则无工具调用的 turn 会被误判自然续跑、多烧一轮脚本；②one-at-a-time 语义改为循环侧"整流进本地缓冲、每轮注入一条"，通道余量原地保留——硬退出时未消费消息不丢；③`LoopHooks` 删除两个轮询方法（接缝 #2 登记），`run_agent_loop` 接收 `InjectionReceiver` 并在返回时归还（Agent 在 run 期间把 receiver 从槽位移出）。（关联文件：`crates/rpi-agent/src/loop_.rs`、`agent.rs`、`hooks.rs`）
- **2026-09-26 T5 流式期间已消费的 steering 在硬退出时会静默丢失**：现象：select! 把 steering 从通道搬进 `deferred_steering` 后流以 error/aborted 收尾，消息既不在通道也不在转录，深度计数永久泄漏（`has_queued_messages` 恒真）→ 原因：I3 只保证"不再继续消费"，没保证"已消费不丢" → 解法：`LoopOutput` 携带 `requeued_steering`/`requeued_follow_up`，Agent 经 `InjectionSender::requeue_*(不增计数)` 放回；深度计数统一在**注入时点**递减。专项测试 `steering_consumed_mid_stream_is_requeued_on_hard_exit`。（关联文件：`crates/rpi-agent/src/loop_.rs`、`agent.rs`）
- **2026-09-26 T2 落地：delta 为主 + SharedPartial 读口**：`MessageStart` 携带 `Arc<RwLock<AssistantMessage>>`（serde skip），循环在每个 delta 上原地更新，UI 随帧经 `Agent::partial_message()` 克隆读取；`MessageDeltaPayload` 三变体（Text/Thinking/ToolCallArgs）。写侧 RwLock 用 `unwrap_or_else(|e| e.into_inner())` 防"订阅者持锁 panic 中毒后每个 delta 都 panic"。toolcall 参数增量尽力解析仅供 UI 预览，终态以 provider 定稿为准。（关联文件：`crates/rpi-agent/src/event.rs`、`loop_.rs`、`agent.rs`）
- **2026-09-25 的取舍③（validate_arguments 子集）就此作废**：T7 已引入 `jsonschema =0.58.0` 完整校验，enum/minimum/数组元素等此前漏过的非法参数在执行前拦截；schema 非法（compile 失败）fail-closed 返回 Err。要点：①`Value::Null` / `Bool(true)` schema 视为"未声明，不校验"（既有测试钉住）；②每次工具调用现场 compile schema（工具调用低频，不在热路径），真实内置 schema 的兼容性断言放 rpi-tools 侧（rpi-agent 不能反向依赖 rpi-tools）。（关联文件：`crates/rpi-agent/src/loop_.rs`、`crates/rpi-tools/src/lib.rs`）
- **2026-09-26 T8 落地：streaming 标志 watch 化，wait_idle 去轮询**：`AtomicBool` 改 `Mutex<watch::Sender<bool>>` + receiver——"检查-置位"在锁内串行化补上 watch 没有的 CAS 语义，`wait_idle` 用 `borrow_and_update() + changed().await`（克隆一个 receiver，clone 后 run 结束不丢唤醒；`changed()` Err 兜底 break 防自旋）。`Agent::prompt` 维持 inline await 整个 run（pi 同构），"run spawn 化"按 11 §5 决策记录不实施。（关联文件：`crates/rpi-agent/src/agent.rs`）
