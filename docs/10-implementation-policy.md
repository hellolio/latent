# 10 — 总体实现方针

> **一句话**:crate 级拆分、工厂函数 + trait 注入、框架先行 —— 每个 crate 对外只暴露**工厂、trait、类型**三样东西,所有行为经由六个接缝注入;先把 workspace 骨架搭到"接缝可验证",再按里程碑逐模块纵向填充。

本文档是**开发规则**,不重复设计细节;模块内部的完整设计见 00-09 各篇,涉及处均给出引用。

---

## 1. 架构总纲:7 crate、四层单向依赖、crate 级可拆卸

crate 布局与依赖图沿用 09 文档 B1,与 pi 完全同构:

```
L4  rpi-cli (bin)  ──── rpi-tui ┐
L3  rpi-core ───────────────────┤  依赖下方全部
L2  rpi-agent ─── rpi-session ─┤  session/tools 只依赖 agent 的类型/trait
L1  rpi-ai ─────────────────────┘  零内部依赖
```

| crate | 职责 | 依赖 | 可拆卸性 |
|---|---|---|---|
| `rpi-ai` | `Provider` trait、适配器、事件流、retry、overflow | 无内部依赖 | 基座,不可拆 |
| `rpi-agent` | 循环 + `Agent` + 消息/事件类型 + `LoopHooks`/`Tool` trait | rpi-ai | 基座,不可拆 |
| `rpi-session` | JSONL 会话树、投影、compaction | rpi-agent 的**类型** | **可选组件**:core 通过注入使用,删除后其余 crate 照常编译 |
| `rpi-tools` | 内置 8 工具 | rpi-agent 的 `Tool` trait | **可选组件**:注册表按需装配 |
| `rpi-core` | `AgentSession`、系统提示词 sections、扩展 registry、模型解析 | rpi-ai、rpi-agent(session/tools 经 trait 注入) | 业务核 |
| `rpi-tui` | 差分渲染终端 UI | 无内部依赖(pi 意义上的零依赖) | **可选组件**:只是 core 事件的一种消费者 |
| `rpi-cli` | main + 四种模式 + **装配** | 全部 | 可执行壳 |

**可拆卸的判据**(CI 可检查):移除 `rpi-session`、`rpi-tools`、`rpi-tui` 中任意一个 crate,`cargo build --workspace` 其余成员仍零警告通过。可选组件对 core 的贡献一律经由 trait 对象(工具注册表、会话持久化句柄、UI 实现)在**装配期**注入,运行期 core 只持有 trait object。

## 2. 模块契约:独立性三规则

每个 crate 的公开 API 只允许三类东西:

1. **工厂函数**(`create_*()`):构造该模块的具体实现并以上游 trait 类型返回。调用方**永远不 import 具体实现类型**,只持有 trait object。换实现 = 换工厂,调用方零改动。
2. **trait 接口**:该模块对下游的全部行为承诺。
3. **纯类型**(struct/enum,serde 可序列化):跨 crate 流动的数据。

配套约束:

- **依赖方向严格单向**,按上图四层;禁止反向依赖、禁止同层互依(`rpi-session` 与 `rpi-tools` 互不知道对方存在)。
- **类型向下依赖,行为向上注入,事件单向广播**(09 Part A 的三条通道,原样继承)。
- **trait 方法不得 panic**:失败走 `Result` 或编码进事件流(见 §4 契约细节);钩子返回空值(`vec![]`/`None`)是合法语义,不是错误。
- **无可变全局状态**:循环拿快照进、发事件出;`Agent` 是有状态薄壳;上层是组装者(09 A4 状态所有权表)。
- 内部实现类型(适配器、具体工具、扩展 runner)一律 `pub` 之外 —— 经由工厂出厂,不留实现细节。

## 3. 六个接缝清单(可拆卸的技术保证)

全部映射结论出自 09 A2/B2,此处固化为**不允许随意改动的稳定边界**:

| # | 接缝 | Rust 形态 | 换实现的场景 |
|---|---|---|---|
| 1 | 循环 ↔ provider | `trait Provider`(`rpi-ai`) | 换 API 适配器 / 接 Mock |
| 2 | 宿主 ↔ 循环 | `trait LoopHooks`(默认空实现) | 业务核/模式覆盖钩子 |
| 3 | 循环 ↔ 工具 | `trait Tool` + `Arc<dyn Tool>` 注册表 | 增删工具、换后端(SSH/沙箱,05 文档 Operations) |
| 4 | core ↔ 扩展 | `trait ExtensionActions` + `trait ExtensionUi` | 加载方式换 WASM 时不动 core(09 B4) |
| 5 | core ↔ mode | mode 订阅事件流 + 各 mode 实现 `ExtensionUi` | 加 print/json/rpc 模式零改动 |
| 6 | 事件汇 | `Vec<Arc<dyn Subscriber>>` 串行 await,保序 | UI/持久化/扩展都只是订阅者 |

**修改纪律**:接缝 trait 的签名变更视为破坏性变更,必须在方针文档本表登记理由并更新全部实现;新增行为优先加带默认实现的方法,不改既有签名。

**变更登记**:

- **2026-09-26 接缝 #2 `ToolBlock` 增加 `args: Option<serde_json::Value>` 字段**。理由:使 tool_call 干预点支持改参(对齐 pi 原地改 `event.input`),B 方案扩展(07 §8)的拦截/审计类扩展需要;缺省 `None` = 不改参,既有实现不受影响。影响:`rpi-agent/src/hooks.rs` 类型定义与全部构造点、`agent_loop` 应用逻辑、07 §8 的 MCP 改参链式语义。
- **2026-09-26 接缝 #2 `ToolBlock` 增加 `block: bool` 字段(实现落地时补充)**。理由:`Option<ToolBlock>` 返回形态下,"仅改参不拦截"无法表达;显式 block 标志同时与 pi `ToolCallEventResult{block, reason, terminate}`(@types.ts:1233)的 wire 形态对齐,MCP 扩展可直接反序列化同构 JSON。语义:`block=true` 拦截(reason/terminate 生效),`block=false` 且 `args=Some` 改参继续执行。影响:同上条;循环层改参后重新过 schema 校验。
- **2026-09-26 接缝 #2 `LoopHooks` 删除 `steering_messages()`/`follow_up_messages()`(11 计划 T5)**。理由:注入从轮询制改 mpsc 推送制(03 §10.5)——循环在流式期间 `select!` 接收 steering,turn 边界按 QueueMode 整流;轮询点、补轮询、防双注入补丁全部消失。影响:`LoopHooks` 实现方(ExtensionHooks、PassthroughHooks、测试 hooks)需删除两方法;宿主经 `rpi_agent::create_injection_endpoints()` 取得 `InjectionSender`(push)/`InjectionReceiver`(随 run 进循环),`run_agent_loop` 签名增 receiver 参数并归还;`Agent::steer/follow_up` 语义不变。
- **2026-09-26 接缝 #6 `AgentEvent` 负载变更(11 计划 T2)**。理由:delta 为主 + 快照读口(03 §10.6 待定点②定案)。`MessageStart` 增加 `partial: Option<SharedPartial>`(`Arc<RwLock<AssistantMessage>>` 读口,`#[serde(skip)]`,跨进程由 delta 重建);`MessageDelta` 负载从纯文本 String 改为 `MessageDeltaPayload`(Text/Thinking/ToolCallArgs 三变体,serde tagged)。影响:全部事件消费者(rpi-tui/interactive、print、json、rpc、event_bus)。wire 兼容:event_bus 对 Text 增量保持旧 `{"delta": ...}` 形态,Thinking/ToolCallArgs 各自展开。
- **2026-09-26 接缝 #1 `StreamOptions` 增加观察回调(11 计划 T3)**。理由:02 文档 §4.3 的 onPayload/onResponse/onProviderStreamEvent 补齐;`StreamOptions` 另增 `deferred: bool` 与 `DeferredHandle` 纯类型(适配器接入按需)。影响:适配器在发送前/响应后/原始事件解析后各调用一次;回调 panic 捕获吞掉(policy §2 宿主侧兜底)。`RetryCallbacks`/`RetryHooks` 语义不变;重试装饰器本体下移 rpi-ai(`create_retrying_provider`),rpi-core 保留薄装配厂(02 文档层界,11 §3.3)。
- **2026-09-26 mode 面 `AgentSession::prompt` 返回类型变更为 `Result<PromptOutcome, CoreError>`(11 计划 T8)**。理由:流式中入队不再复用 `RunStop::EndTurn` 谎报(04 踩坑②);`PromptOutcome::Started(RunStop)`/`Enqueued` 使调用方免于以 `is_streaming()` 二次判断。影响:四个调用点(print/json/rpc/interactive)同步更新;rpc 对 Enqueued 如实上报 `"stopReason":"enqueued"`(旧实现上报 end_turn 属线协议行为修正);`Agent::prompt` 保持 inline await,签名不变。并发 TOCTOU:is_streaming 检查后 run 被抢先时,session 层把 `AlreadyRunning` 折叠为 steer + Enqueued。
- **2026-09-26 `Agent` streaming 标志改 `tokio::sync::watch<bool>`(11 计划 T8)**。理由:`wait_idle` 去 10ms 轮询(04 踩坑③),`borrow_and_update + changed` 无忙等、无丢失唤醒。影响:`is_streaming()` 读 watch 值,"检查-置位"由 `Mutex<watch::Sender<bool>>` 串行化(替代 compare_exchange);类型未跨 crate 暴露,`AgentStateSnapshot.is_streaming` 语义不变。
- **2026-09-26 shell 工具配置通道(11 计划 T9/T10)**。理由:PI_* 会话环境注入(T9)与 spawnHook/commandPrefix(T10)需要从装配层进 bash/powershell 工具。形态:rpi-tools 新增类型 `SessionEnvFn`/`ShellSpawnOptions`、trait `ShellSpawnHook`,工厂 `create_bash_tool_with*`/`create_powershell_tool_with`/`create_tools_at_with_shell` 出厂(遵守 §2 三规则);cli `BuildOptions` 增加可选 `spawn_hook`,commandPrefix 来自 settings。未改 `AgentSessionConfig` —— rpi-core 不依赖 rpi-tools(§1 分层),工具配置在 cli 装配点直接进工具工厂参数,core 依旧只认 `Arc<dyn Tool>`。
- **2026-09-28 权限系统与计划模式(13 文档)**。理由:三种会话模式(Plan/Confirm/FullAccess)需要审批流、OS 沙箱与工具收紧,全部挂在现有接缝上。接缝变更:(a) `AgentSessionEvent` 新增 `ApprovalRequested{request}`/`ApprovalResolved{tool_call_id,decision}` 两变体(负载为 rpi-core 新类型 `ApprovalRequest`;扩展总线不感知这两变体);(b) `SessionSink` 新增默认空实现方法 `append_mode_change(&str)`(对齐 `append_tool_set_change` 模式);(c) `AgentSessionConfig` 新增可选字段 `permission: Option<Arc<PermissionEngine>>` 与 `mode_tool_ceiling: Option<Vec<String>>`(未装配 = 行为退化为现状);(d) `LoopHooks::before_tool_call` 调用点(loop_.rs prepare_call)与 cancel token `select!`,审批等待可被 abort 打断(签名不变);(e) `rpi-agent::ToolCallCtx` 不变,`ToolBlock.terminate` 复用既有"批全部 terminate 提前结束"语义承载审批 Abort。新增 L2 crate `rpi-sandbox`(零内部依赖,可选组件;rpi-core 不依赖它,`SandboxPolicy` 在 core 另有定义,cli 装配层映射);`CURRENT_SESSION_VERSION` 5→6(旧文件无 ModeChange = 默认 Plan,读取侧兼容)。

## 4. 关键契约细节(实现时不许偏离)

- **失败编码进流**:`Provider::stream` 返回的事件流以终态事件(error/done)收尾,**不抛异常**(00 设计原则 5;02 文档流协议)。
- **`LoopHooks::convert_to_llm` 是唯一必填方法**,其余全部带默认空实现(09 B3 草图)。
- **partial 消息**:流式期间 buffer 在循环局部,`done` 后一次性 push 进转录;`message_update` 事件携带快照(09 B2)。
- **并行工具双语义**:`tool_execution_end` 事件按**完成序**,tool result 消息按**源序**(09 B2)。
- **length 防御**:`stopReason === length` 时拒绝该消息全部工具调用(03 文档不变量 I 系列)。
- **会话 append-only**:JSONL 树只追加,compaction 原 entry 保留(06 文档)。

## 5. 框架先行的开发流程

### Phase 0 — 骨架(已完成,本次)

- workspace 建齐 7 crate,`cargo build --workspace` 零警告;
- 六接缝 trait 全部定型(按 09 B3 草图),stub 实现到位;
- `MockProvider`(canned 流式事件)+ `rpi-cli --mock` 走通 cli→core→agent→ai 全链路 —— **证明接缝真实可用,而非空壳**;
- `cargo test --workspace` 覆盖:trait 默认实现、mock 流、装配工厂。

### Phase 1-N — 按里程碑纵向填充(09 B6,原样引用)

| 阶段 | 模块 | 内容 | 验收 |
|---|---|---|---|
| M1 | rpi-ai | 类型 + 1 个真实适配器 + 事件流 | `cargo run -- "hello"` 流式打印回复 |
| M2 | rpi-agent | 循环(03 文档伪代码逐行)+ 串行工具 + read/bash | 模型能读文件并执行命令 |
| M3 | rpi-session | JSONL 树 + 投影 + compaction | 重启续聊、自动压缩 |
| M4 | rpi-core | AgentSession + sections + 8 工具 + steering/followUp + 重试 | print 模式完整可用 |
| M5 | rpi-tui + 扩展 | ratatui(或自研差分)+ 编译期 registry | interactive 模式 + 示例扩展 |
| M6 | rpc | stdio JSONL + 其余 provider 适配器 | 编辑器可接 |

### 每个模块的验收标准(不变)

1. `cargo test -p <crate>` 独立通过;
2. 不破坏其余任何 crate 的编译与测试;
3. 新增对外类型/trait 遵守 §2 三规则,接缝变更走 §3 修改纪律;
4. 对应 00-09 设计文档中的源码索引逐项对照完毕(README 阅读顺序)。

## 6. 差异点默认决策(09 B5,直接采纳,避免反复)

| 差异点 | 决策 |
|---|---|
| 失败 assistant 消息是否进上下文 | **保留进上下文**(跟随 coding-agent,UI 标红) |
| 流式事件负载 | delta 事件 + `message_update` 携带快照 |
| partial 消息归属 | buffer + 定稿 push(见 §4) |
| `details` 类型 | 边界统一 `serde_json::Value`,内部各工具强类型 |
| 扩展加载 | 起步编译期注册(`trait Extension`),稳定边界是事件干预语义 + `ExtensionUi`;**2026-09-26 改判:动态加载定为 B 方案——扩展为独立进程,通讯复用 MCP(rmcp),埋点经公共分发函数上 `rpi/event` 通道(设计见 07 §8)**;进程内脚本引擎(A)不采用(需 +30–60MB 嵌入引擎或自研异步桥,且失去进程隔离),WASM 仍为远期(仅当出现分发不可信扩展的需求) |
| 扩展错误语义 | **2026-09-26 新增**:编译期扩展 init 失败 = 跳过 + 收集诊断(对齐 pi loader 的 continue+warning,逐扩展独立工具缓冲,失败不留已注册工具);运行期 handler 失败/超时/断连 = 诊断 + 跳过该次分发,fail-open/closed 按扩展注册时声明,绝不击穿宿主(pi runner 语义) |
| 串行订阅结算 | `agent_end` 后 join 全部 subscriber 才算 idle,`Agent::wait_idle` 等该 join |
| 控制面(system message) | 照搬 pi:`SystemMessage.sections/toolsAdded/toolsRemoved` 进转录,与 pi 会话格式兼容优先 |

后续如需推翻某条默认决策,在本表改判并注明理由与影响面,不另立文档。
