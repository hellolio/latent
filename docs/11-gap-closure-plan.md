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

| # | 任务 | crate | 源文档 | 优先级 | 状态 |
|---|---|---|---|---|---|
| T1 | 真流式重试（SSE 帧级缓冲） | rpi-core + rpi-ai | 04 §踩坑① | 高 | ✅ 已完成（2026-09-26） |
| T2 | thinking/toolCall 流式增量 | rpi-ai + rpi-agent | 03 §踩坑② | 高 | ✅ 已完成（2026-09-26） |
| T3 | 观察回调 onPayload/onResponse/onProviderStreamEvent | rpi-ai | 02 §M4 | 高 | ✅ 已完成（2026-09-26） |
| T4 | 用量展示（token/缓存/费用） | rpi-cli | 用户决策 | 高 | ✅ 已完成（2026-09-26） |
| T5 | mpsc 推送式注入 + Phase 状态机 | rpi-agent | 03 §10.3/§10.5 | 高 | ✅ 已完成（2026-09-26） |
| T6 | I1–I6 property test | rpi-agent | 03 §10.1/§10.7 | 高 | ✅ 已完成（2026-09-26） |
| T7 | jsonschema 参数校验 | rpi-agent | 03 §踩坑③ | 中 | ✅ 已完成（2026-09-26） |
| T8 | PromptOutcome + wait_idle 去轮询 | rpi-core + rpi-agent | 04 §踩坑②③ | 中 | ✅ 已完成（2026-09-26） |
| T9 | PI_* 环境变量注入 | rpi-tools + rpi-core | 05 §踩坑③ | 中 | ✅ 已完成（2026-09-26） |
| T10 | spawnHook / commandPrefix | rpi-tools + rpi-core | 05 §踩坑③ | 中 | ✅ 已完成（2026-09-26） |
| T11 | bash 进程组杀灭（孙进程清理） | rpi-tools | 05 §踩坑② | 中 | ✅ 已完成（2026-09-26） |

第一批（T1–T6，T3 提前完成）与第二批（T7–T11）均已通过独立 reviewer 审查。

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

> 文档更新与接缝登记（原 T12，2026-09-26 用户决策：07 §8.7 过时标注、02 裁剪条目、03 待定点）已完成并从本计划删除；接缝登记义务移入 §3 注意事项第 8 条。

## 3. 全局注意事项

1. **先读文档再动手**：每批开工前重读涉及的功能文档与 `10-implementation-policy.md`（文档会更新，记忆不会）。本文与功能文档冲突时，以功能文档为准。
2. **policy 三规则不可破**：crate 只暴露工厂、trait、类型；实现类型不 `pub`；依赖严格单向。新回调/配置一律走 trait 或工厂参数，不引入可变全局状态。
3. **重试逻辑的归属**：T1 把缓冲下探时，评估将 RetryingProvider 整体下移到 rpi-ai（02 文档本就把重试/overflow 划给 ai 层）；若下移，rpi-core 的 `create_retrying_provider` 保留为薄装配厂，接缝变更登记。
4. **T5 是最高风险项**：动循环前先把 T6 的 property test 写好并跑绿，形成保护网；重构中任何事件语义变化（事件序、配对、abort）都是回归，逐一对照 03 文档验收。
5. **性能是验收项不是事后项**：T2 的快照读口避免每 delta 整份克隆；T4 的用量行只随回合终态重算；T5 的 mpsc 接收不引入忙等。有疑问先测（大会话、高频 delta 场景）。
6. **边界清单每项都要落测试**：空集合、None/Err 传播、流中断、重入、超长输入、schema 非法、钩子失败、断连。callback/hook 类新面必须验证"panic 不击穿宿主"。
7. **版本 pin**：新增依赖 `proptest`、`jsonschema`、`nix` 一律 pin 精确版本（对齐 rmcp `=3.4.1` 的既有做法）。
8. **每批收尾流程**：`cargo test --workspace` 全绿 + clippy → 派独立 reviewer 子代理（给改动文件列表 + 对应文档路径，要求对照文档与 pi 实现审实现与测试）→ 修完 reviewer 问题复测 → 踩坑记录。**接缝签名变更**（如 StreamOptions、AgentEvent 负载、prompt() 返回类型、bash 工具配置）必须同步在 `10-implementation-policy.md` §3 接缝表登记。
9. **决策记录即时落盘**：本文 §0 的"不做/推迟"清单与各功能文档中已标注的 2026-09-26 决策是正式决策，实现者不得自行翻案；若实现中发现某决策与文档硬约束冲突，停下来问用户。

## 4. 验收总口径

- 全部批次完成后：`cargo test --workspace` 与 clippy 零警告通过；
- TUI 可见：流式 thinking/工具参数、每回合用量行、会话累计；
- 配置重试后流式仍逐 delta（T1 的核心验收，最容易被悄悄退化回去，review 时重点盯）；
- 10 §3 接缝表与各文档踩坑记录齐全，本文档中已完成的任务逐条删除。

## 5. 交接实施清单（2026-09-26，T7–T11 + 收尾；供后续实施者使用）

> 第一批（T1/T2/T3/T4/T5/T6）已完成：`cargo test --workspace` 27 个测试二进制全绿、
> `cargo clippy --workspace --all-targets` 零警告、独立 reviewer 审查通过（P0/P1 问题已修复），
> 接缝变更已在 `10-implementation-policy.md` §3 登记，踩坑记录已落 02/03/04 文档。
> **实施 T7–T11 前必读**：`10-implementation-policy.md` 全文 + 对应功能文档；遵循
> `rpi-dev-workflow` 流程（读文档 → 编码+测试 → 全仓测试与 clippy 零警告 → 独立 reviewer
> 子代理 → 接缝登记与踩坑记录）。

### T7 jsonschema 参数校验（#8）

- **落点**：`crates/rpi-agent/src/loop_.rs` 的 `pub fn validate_arguments(schema, args) -> Result<(), String>`
  （当前是 type/required/嵌套 properties 子集，`integer` 放过任意 number）；调用点在 `prepare_call`
  （初校验 + beforeToolCall 改参后重校验，两处）。
- **做法**：rpi-agent 加 `jsonschema` 依赖，**pin 精确版本**（对齐 rmcp `=3.4.1` 的做法）；用
  `jsonschema::JSONSchema::compile` 替换子集实现，保持函数签名与错误语义（Err(String)，
  由现有路径转错误 ToolOutcome，不 panic、不改 Tool trait）。
- **必须明确并测试**：schema 本身非法（compile 失败）时的行为——建议 fail-closed（返回 Err，
  文案说明 schema 非法），并写测试钉住。
- **验收**：enum/minimum/数组元素类型等此前漏过的非法参数在执行前被拦截；现有用例
  `argument_validation_checks_type_required_and_nesting` 与全部既有测试不回归；schema 非法有明确测试。
- **坑**：jsonschema 对内置 8 工具的 schema 必须照常通过——先跑全仓测试确认兼容。

### T8 PromptOutcome + wait_idle 去轮询（#13）

- **落点**：`crates/rpi-core/src/session.rs` 的 `AgentSession::prompt`（流式中复用
  `RunStop::EndTurn` 表达"已入队"）；`crates/rpi-agent/src/agent.rs` 的 `wait_idle`（10ms 轮询）。
- **做法**：
  - 引入 `pub enum PromptOutcome { Started(RunStop), Enqueued }`，`AgentSession::prompt` 返回
    `Result<PromptOutcome, CoreError>`；interactive/print/json/rpc 四个调用点同步更新。
  - `Agent` 的 `streaming: AtomicBool` 改 `tokio::sync::watch<bool>`：`is_streaming()` 读 watch 值，
    `wait_idle()` 用 `borrow_and_update() + changed().await`（无轮询、无丢失唤醒）；
    run 生命周期里所有 `streaming.store` 点改为 `send`。
  - **决策记录**：`Agent::prompt` 保持 inline await 整个 run（pi 的 prompt 即 await run 完成）；
    计划里"run 生命周期 spawn 化"是去轮询的手段而非目标，若改 spawn 需重构 overflow 恢复
    （`run_with_recovery` 依赖 RunStop），不做。
- **接缝登记**：`AgentSession::prompt` 返回类型变更是 mode 面破坏性变更，须在 10 §3 登记表补条目。

### T9 PI_* 环境变量注入（#14）

- **落点**：`crates/rpi-tools/src/bash.rs`（`run()` 构造 `tokio::process::Command` 处）与工厂
  （`create_bash_tool` / `create_shell_tool`）；`crates/rpi-cli/src/assembly.rs` 装配接线。
- **pi 清单**（05 §4）：`PI_SESSION_ID`、`PI_SESSION_FILE`、`PI_PROVIDER`、`PI_MODEL`、
  `PI_REASONING_LEVEL`，按 `exposeSessionEnvironment` 注入。
- **做法**：
  - rpi-tools 定义 `pub type SessionEnvFn = Arc<dyn Fn() -> Vec<(String, String)> + Send + Sync>`，
    工厂加 `create_bash_tool_with_session_env(cwd, env)`（powershell 同理）；`run()` 里对每项：
    **用户进程环境已有同名变量则不覆盖**，否则 `.env(name, value)`。
  - rpi-core/cli：装配期先建共享 cell（如 `Arc<Mutex<Weak<AgentSession>>>`），工具工厂拿
    "读 cell 的闭包"，session 建好后回填 Weak；session 侧提供按需快照（provider/model/thinking
    取 `agent().state_snapshot()`，session id/file 取 SessionManager）。
  - **禁用可变全局状态**（policy §2）；注入走工具配置参数。
- **验收**：脚本 `echo $PI_MODEL` 输出注入值；预设 `PI_MODEL=keep` 再执行验证不覆盖；
  空 cell（无 session）时行为 = 现状。

### T10 spawnHook / commandPrefix（#15）

- **落点**：`crates/rpi-tools/src/bash.rs` 的 `ShellToolConfig` + `run()`；rpi-core 装配侧接线。
- **做法**：
  - `ShellToolConfig` 增加 `command_prefix: Option<String>` 与
    `spawn_hook: Option<Arc<dyn ShellSpawnHook>>`；
    `#[async_trait] pub trait ShellSpawnHook: Send + Sync { async fn rewrite(&self, command: String) -> Result<String, String>; }`。
  - 语义：`final = prefix + hook(original)`（hook 先改写、prefix 最后前置，保证 hook 检查的是
    用户命令）；空前缀 = 无操作。
  - **钩子错误 = 拒绝执行**：返回 Err 时工具直接产出错误结果（文案含 hook 错误信息），
    不 spawn——与扩展错误语义（07 §8.5 跳过+诊断、绝不击穿宿主）一致。
  - rpi-core 装配：`AgentSessionConfig`/cli `BuildOptions` 增加可选 shell 命令配置通道
    （prefix 来自 settings，hook 可接扩展管线经 LoopHooks 的改参链），接缝签名变更登记。
- **验收**：prefix 生效（`echo` 变 `<prefix> echo` 仍成功）；hook 改写命令生效；
  hook 返回 Err → 错误结果且无子进程产生（用副作用脚本验证）；bash/powershell 共用工厂两路都测。

### T11 bash 进程组杀灭（#16）

- **落点**：`crates/rpi-tools/src/bash.rs` 的 `run()`（`kill_on_drop(true)` 直接杀子进程，
  孙进程残留）；`crates/rpi-tools/Cargo.toml`。
- **做法**：
  - rpi-tools 加 `nix` 依赖，**pin 精确版本**（如 `=0.27.1`，确认 API 后定），只进 rpi-tools
    （可拆卸判据：移除 rpi-tools 其余 crate 零警告编译）。
  - Unix：spawn 前 `Command::process_group(0)`（tokio::process 支持，等于 setpgid 自成进程组，
    pgid = 子进程 pid）；超时/abort 分支先
    `nix::sys::signal::kill(Pid::from_raw(-(child.id() as i32)), Signal::SIGKILL)` 清整棵树，
    再 `child.wait()`；**kill_on_drop(true) 保留为兜底**。全部 `#[cfg(unix)]` 隔离，
    非 Unix（含 powershell 路径）保持现状。
  - 注意 pid 获取时机与错误处理（spawn 失败、wait 已返回后 killpg：进程已死 → 忽略 ESRCH）。
- **验收**：命令 `sleep 300 & sleep 300`（孙进程）超时后两者都被回收——先让命令把子 pid 写
  临时文件，超时后再执行一次 `kill -0 $(cat pidfile)` 断言失败（非零 exit）；
  正常结束路径（echo 等）不受影响。

### 收尾遗留（reviewer P2，非阻塞）

1. `crates/rpi-ai/src/adapters/mod.rs` 的 `observe_payload`：panic 被吞后建议加一行
   `eprintln!` 诊断（失败不抛异常 ≠ 不可观测）。
2. interactive 的 TurnEnd 用量行：0 用量（错误回合）也打印，观感问题，可按
   `usage.total_tokens == 0 && cost == 0` 跳过。
3. 预算在首轮 AwaitingRequest 即耗尽时 `initial_prompts` 被丢弃（仅 max_turns=0 等配置可达）——
   可并入 `LoopOutput::requeued_*` 机制。
4. T1 装饰器缺"退避中 abort"专项测试（`retry_assistant_call` 有，装饰器没有）；
   T6 property test 的 abort 配对路径已补（`pairing_abort_path_every_call_still_gets_result`）。
5. proptest 使用注意（写新 property test 时）：`proptest!` 宏内**不能**给测试函数标返回类型；
   断言表达式字符串含 `{ .. }` 会破坏宏的格式串（先把 `matches!` 结果存变量）；
   用 `block_on(async { ...; Ok(()) }).unwrap_or_else(|e| panic!(...))` 模式包装 async。

### 每批收尾流程（不变）

`cargo test --workspace` 全绿 + `cargo clippy --workspace --all-targets` 零警告 → 派独立
reviewer 子代理（给改动文件列表 + 对应文档路径，要求对照文档与 pi 实现审实现与测试）→
修完 reviewer 问题复测 → **接缝签名变更在 10 §3 登记**（T8 的 prompt 返回类型、
T10 的 bash 工具配置）→ 对应功能文档踩坑记录 → 本文档 §1 状态列更新、
已完成条目按 §4 约定删除。
