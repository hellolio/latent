# 11 — 缺口补齐实施计划（2026-09-26 决策记录）

> 本文是对全仓盘点后确定的"未实现/推迟项"的集中处置方案。每项均标注了用户决策、
> 对应设计文档、现状代码锚点与验收标准。实施时按本仓库 `rpi-dev-workflow` 流程执行：
> 先读基础文档（00/01/09/10）与对应功能文档，再编码，测试全绿后派独立 reviewer 审查，
> 最后在对应功能文档追加踩坑记录。

## 0. 背景与总原则

- 仓库 M1–M6 里程碑主体已完成（7 crate、约 230 测试、可编译），本计划只补**用户拍板要做的缺口**。
- 以下各项**维持现状，不做**（已记录为正式决策，后续不得再当作"遗漏"提出）：
  - OAuth 登录、其余 8 个 provider 适配器、模型目录 models-store（02 文档 M6 范围）——理由：接入绝大多数走 API key，通用 OpenAI 风格适配器已覆盖主流。
  - OpenAI 侧冷门细节：tool result 图片转发、grammar/custom 工具、reasoning_details 回放——按需再做。
  - read 工具读图、fork（跨文件复制历史）、TUI 双屏/图片/完整键位表、RPC 剩余约 18 个命令、实验性 CBOR 栈。
  - 时间戳用毫秒整数、压缩单请求——保持，不追求与 pi 会话文件逐字节兼容。
- 实施顺序遵循依赖方向：**rpi-ai → rpi-agent → rpi-core → rpi-tools → rpi-cli/TUI → 文档**，与 09 B1 的分层一致，避免反向依赖或为图省事把逻辑上提到错误层。

## 1. 任务全景表

| # | 任务 | crate | 源文档 | 优先级 |
|---|---|---|---|---|
| T1 | 真流式重试（SSE 帧级缓冲） | rpi-core + rpi-ai | 04 §踩坑① | 高 |
| T2 | thinking/toolCall 流式增量 | rpi-ai + rpi-agent | 03 §踩坑② | 高 |
| T3 | 观察回调 onPayload/onResponse/onProviderStreamEvent | rpi-ai | 02 §M4 | 高 |
| T4 | 用量展示（token/缓存/费用） | rpi-cli | 用户决策 | 高 |
| T5 | mpsc 推送式注入 + Phase 状态机 | rpi-agent | 03 §10.3/§10.5 | 高 |
| T6 | I1–I6 property test | rpi-agent | 03 §10.1/§10.7 | 高 |
| T7 | jsonschema 参数校验 | rpi-agent | 03 §踩坑③ | 中 |
| T8 | PromptOutcome + wait_idle 去轮询 | rpi-core + rpi-agent | 04 §踩坑②③ | 中 |
| T9 | PI_* 环境变量注入 | rpi-tools + rpi-core | 05 §踩坑③ | 中 |
| T10 | spawnHook / commandPrefix | rpi-tools + rpi-core | 05 §踩坑③ | 中 |
| T11 | bash 进程组杀灭（孙进程清理） | rpi-tools | 05 §踩坑② | 中 |
| T12 | 文档更新与接缝登记 | docs/ | 07 §8.7、10 §3 | 收尾必做 |

建议批次：**第一批 T1/T2/T4/T5/T6**（核心体验与硬性验收），**第二批 T3/T7/T8/T9/T10/T11**，T12 随每批收尾。

## 2. 各任务详细说明

### T1 真流式重试（#12）

**现状**：`crates/rpi-core/src/retry.rs:26` 的 `create_retrying_provider` 对**成功尝试也整体缓冲后回放**，装配重试装饰后所有请求退化为"终态可见"的伪流式。

**目标**：恢复逐 delta 流式。按 04 文档方案把缓冲下探到 rpi-ai 的 SSE 帧级：

- 重试装饰器逐帧转发当前尝试的事件；失败发生时若**尚无内容 delta 发出**，静默截断、重试下一轮；
- 一旦内容已开始流给下游，失败直接按现有"失败编码进流"语义终止，不做中途重试（policy §4：失败编码进流，不许偏离）；
- 指数退避、可重试判定等现有逻辑保留（可下移到 rpi-ai 层，见注意事项 3）。

**验收**：用 ScriptedProvider/MockProvider 注入"先失败后成功"序列，断言：成功路径逐 delta 到达（不是终态一次性）、失败重试路径无重复 delta、流中断语义与 02 文档一致。

### T2 thinking/toolCall 流式增量（#9）

**现状**：`message_update` 只在终态携带完整快照；流式中文本有增量（MessageDelta），thinking 与 toolCall 参数是等完整后一次性出（03 §踩坑②）。

**目标**：采用 03 文档待定点②的推荐方案——**delta 为主 + 提供随时可读的当前快照读口**：

- rpi-ai 侧核对 `AssistantMessageEvent` 的 thinking/toolcall delta 字段是否已齐全（`crates/rpi-ai/src/types.rs:482` 起的 StreamOptions/事件区），缺则补；
- rpi-agent 侧扩展 `AgentEvent::MessageDelta` 负载携带 thinking 增量与 toolcall 参数增量，循环内透传不加工；
- 快照读口用 `Arc<RwLock<…>>`（或等价物）暴露"到目前为止"的 partial 状态，供 TUI 随帧取用，**不做每个 delta 带整份快照**（热路径分配，见注意事项 5）。

**验收**：TUI 能看到思考过程与工具调用参数逐块增长；事件序满足"start → *_delta → done"；thinking/toolcall 增量不改变终态消息内容（终态权威定稿语义不变）。

### T3 观察回调（#4a）

**现状**：`StreamOptions`（`crates/rpi-ai/src/types.rs:482`）无观察回调。

**目标**：按 02 文档 M4 范围补齐：`on_payload`（可检查/替换请求体）、`on_response`（HTTP 响应观察）、`onProviderStreamEvent`（归一化前的原始 provider 事件）+ deferred 句柄。默认无操作，不装配时零开销、路径行为不变。

**验收**：不装配时现有测试全绿不动；装配后能观察到原始请求体/响应/原始事件；回调 panic 不得击穿流（trait 方法不 panic 约束，policy §约束）。

### T4 用量展示（#4b）

**现状**：`Usage`（`crates/rpi-ai/src/types.rs:74`）字段齐全（input/output/cache_read/cache_write/cache_write_1h/reasoning/total_tokens/cost），但 interactive 模式完全不展示。

**目标**：

- interactive 模式每回合结束显示一行用量：输入/输出/缓存读/缓存写（有 reasoning 则一并）token + 费用；
- 维护会话累计值并在状态区可见；
- print/json 模式核对 usage 已随事件输出，缺则补；
- 渲染走现有差分渲染路径，不做每帧重算（注意事项 5）。

**验收**：TUI 出现单回合与累计用量行；数值与 provider 返回的 usage 一致（用 Mock 注入已知 usage 断言）。

### T5 mpsc 推送式注入 + Phase 状态机（#7）

**现状**：`crates/rpi-agent/src/loop_.rs:201` 起用 `pending: Vec<AgentMessage>` 单通道 + `explicit_continuation`（:201/:396）轮询 `hooks.steering_messages()` 补丁式实现注入；`Phase`（:89）仅作观察/断言。

**目标**：按 03 §10.3/§10.5 重构：

- 注入通道改为 mpsc（接收端在循环内 await，发送端由宿主/钩子 push），移除 pending 单通道与 explicitContinuation 补丁；
- `Phase` 升级为 §10.3 的 step 函数状态机，成为真实控制流而非标注；
- 保持 03 §10.1 的 I1–I6 不变量与既有事件语义（error/aborted 唯一硬退出，I3）。

**风险**：这是循环热路径的结构性改动，先补齐 T6 的 property test 再动手（红→绿保护），改完跑全部 loop_integration。

**验收**：steering 在流式期间注入仍生效；abort 语义不变；I3/I4 property test 通过；`pending`/`explicit_continuation` 符号消失。

### T6 I1–I6 property test（#11）

**现状**：03 §10.7 M2+ 要求 I1–I6 写成 property test，全仓未引入 proptest/quickcheck。

**目标**：rpi-agent 加 `proptest` dev-dependency，重点覆盖：

- **I6**（最优先）：重放 system 消息的工具声明后，模型可见集恒等于 `context.tools`；
- 工具结果配对：正常/abort/length 截断三条路径下 toolCall 与 toolResult 一一配对；
- I4：并行时事件完成序 vs 消息源序双保序；
- I5：length 截断消息的全部 tool call 被拒执行（含防振荡计数）。

**验收**：`cargo test -p rpi-agent` 含 property test 全绿；每条不变量至少一个独立用例并注明对应 I 编号。

### T7 jsonschema 参数校验（#8）

**现状**：`crates/rpi-agent/src/loop_.rs:891` 的 `validate_arguments` 只支持子集（type/required/嵌套 properties，`integer` 放过任意 number）；enum、范围、数组元素类型等不校验，非法参数要到工具执行时才报错。

**目标**：引入 `jsonschema` crate（pin 精确版本）替换子集实现；保持错误语义——校验失败返回 Err，由现有路径转错误 AgentToolResult，**不 panic、不改 Tool trait 签名**。

**验收**：enum/最小值/数组元素等此前漏过的非法参数在执行前被拦截；合法参数（含现有测试全集）不受影响；schema 本身非法时的行为有明确测试。

### T8 PromptOutcome + wait_idle 去轮询（#13）

**现状**：流式中 `prompt()` 返回值复用 `RunStop::EndTurn` 表达"已入队"，调用方需以 `agent.is_streaming()` 区分（04 §踩坑②）；`wait_idle`（`crates/rpi-agent/src/agent.rs:317`）以 10ms 轮询 streaming 标志（04 §踩坑③）。

**目标**：

- 引入独立 `PromptOutcome` 类型（如 `Started` / `Enqueued`），调用方不再二次判断；
- run 生命周期 spawn 化，`wait_idle` 改 Notify/watch 等待，去掉轮询。

**接缝影响**：`prompt()` 返回类型变更是破坏性接缝变更，**必须在 `10-implementation-policy.md` §3 接缝表登记**。

### T9 PI_* 环境变量注入（#14）

**现状**：bash 工具拿不到会话上下文（05 §踩坑③）。

**目标**：按 pi 的 PI_* 清单，在 bash 执行时注入会话环境变量（会话目录等）。注入走工具配置（见 T10 的装配通道），不得用可变全局状态（policy §约束）。注意不要覆盖用户显式设置的同名变量。

### T10 spawnHook / commandPrefix（#15）

**现状**：无任何命令前缀/钩子机制（05 §踩坑③）。

**目标**：

- rpi-tools 的 bash 工具配置增加 `commandPrefix`（统一前缀）与 `spawnHook`（执行前回调，可检查/改写命令）；
- rpi-core 装配侧把扩展管线（经 LoopHooks）与 settings 里的配置接到该通道；
- 边界：空前缀为无操作；钩子返回错误时的行为（拒绝执行并回错误结果）需与扩展错误语义（跳过+诊断，绝不击穿宿主，07 §8.5）一致。

### T11 bash 进程组杀灭（#16）

**现状**：`crates/rpi-tools/src/bash.rs:179` 用 `kill_on_drop(true)` 直接杀子进程，孙进程可能残留（bash.rs:7 头注释已注明）。

**目标**：Unix 下用 `nix`（pin 精确版本）`setpgid` 建进程组，超时/中断时 `kill(-pgid)` 清整棵树；**非 Unix（含 powershell 路径）保持现状**，`cfg(unix)` 隔离。注意 10 文档可拆卸判据：移除 rpi-tools 不破坏其他 crate 零警告编译，nix 依赖只进 rpi-tools。

**验收**：测试里 spawn 会再 fork 子进程的脚本，超时后孙进程也被回收；正常结束路径不受影响。

### T12 文档更新与接缝登记（#23，随批收尾）

- `docs/07-extensions.md` §8.7 标题"实施步骤（未开始，实施时按序）"已过时——扩展 MCP 化（event_bus/mcp_host/mcp_tool、E2E）已落地，改为"已落地（2026-09-26），剩余子项见踩坑记录"。
- `docs/02-ai-provider.md` M1 裁剪条目改为"经用户决策正式裁剪"：OAuth/8 适配器/模型目录不做，注明决策日期。
- `docs/03-agent-loop.md` 待定点：①控制面出转录——**维持现状**（与 pi 一致；用户已放弃逐字节兼容）；②事件负载——**delta + 快照读口**（随 T2 落地）；③CheckpointStore——**推迟**。
- `docs/10-implementation-policy.md` §3 接缝表登记本计划全部接缝变更：StreamOptions 观察回调、AgentEvent::MessageDelta 负载扩展、注入通道 mpsc、prompt() 返回 PromptOutcome、bash 工具配置项。
- 每个完成的任务在对应功能文档（02/03/04/05）末尾"踩坑记录"追加条目，格式见流程第五步。

## 3. 全局注意事项

1. **先读文档再动手**：每批开工前重读涉及的功能文档与 `10-implementation-policy.md`（文档会更新，记忆不会）。本文与功能文档冲突时，以功能文档为准。
2. **policy 三规则不可破**：crate 只暴露工厂、trait、类型；实现类型不 `pub`；依赖严格单向。新回调/配置一律走 trait 或工厂参数，不引入可变全局状态。
3. **重试逻辑的归属**：T1 把缓冲下探时，评估将 RetryingProvider 整体下移到 rpi-ai（02 文档本就把重试/overflow 划给 ai 层）；若下移，rpi-core 的 `create_retrying_provider` 保留为薄装配厂，接缝变更登记。
4. **T5 是最高风险项**：动循环前先把 T6 的 property test 写好并跑绿，形成保护网；重构中任何事件语义变化（事件序、配对、abort）都是回归，逐一对照 03 文档验收。
5. **性能是验收项不是事后项**：T2 的快照读口避免每 delta 整份克隆；T4 的用量行只随回合终态重算；T5 的 mpsc 接收不引入忙等。有疑问先测（大会话、高频 delta 场景）。
6. **边界清单每项都要落测试**：空集合、None/Err 传播、流中断、重入、超长输入、schema 非法、钩子失败、断连。callback/hook 类新面必须验证"panic 不击穿宿主"。
7. **版本 pin**：新增依赖 `proptest`、`jsonschema`、`nix` 一律 pin 精确版本（对齐 rmcp `=3.4.1` 的既有做法）。
8. **每批收尾流程**：`cargo test --workspace` 全绿 + clippy → 派独立 reviewer 子代理（给改动文件列表 + 对应文档路径，要求对照文档与 pi 实现审实现与测试）→ 修完 reviewer 问题复测 → 文档更新（T12）→ 踩坑记录。
9. **决策记录即时落盘**：本文 §0 与 T12 的"不做/推迟"清单是用户 2026-09-26 的正式决策，实现者不得自行翻案；若实现中发现某决策与文档硬约束冲突，停下来问用户。

## 4. 验收总口径

- 全部批次完成后：`cargo test --workspace` 与 clippy 零警告通过；
- TUI 可见：流式 thinking/工具参数、每回合用量行、会话累计；
- 配置重试后流式仍逐 delta（T1 的核心验收，最容易被悄悄退化回去，review 时重点盯）；
- 10 §3 接缝表与各文档踩坑记录齐全，本文档中已完成的任务逐条勾销或标注完成日期。
