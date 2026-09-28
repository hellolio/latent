# 14 — 插件路线 A:进程内编译期扩展(编译进二进制)

> **一句话**:实现仓库既有的 trait 接缝(`Tool`/`Extension`/`LoopHooks`/`Subscriber`…),在装配期注入,随 `rpi` 二进制一起编译;拥有宿主全部核心能力(agent loop、权限引擎、事件汇、流式 UI),是引擎级能力(如 subagent)的唯一推荐路线。

进程外独立分发形态见姊妹篇 `docs/15-plugin-mcp-extension.md`(路线 B);两条路线的对比见本文 §1。

---

## 1. 何时选路线 A

| | A:编译进二进制(本文) | B:MCP 进程外(15 文档) |
|---|---|---|
| 需要宿主核心能力(loop/权限/流式 UI) | ✅ 一等公民 | ❌ 拿不到,只能模拟 |
| 分发安装 | 需重编译 | ✅ 任意语言、独立安装 |
| 崩溃隔离 | 进程内(靠"不许 panic"约定 + catch_unwind) | ✅ 进程边界 |
| 每实例开销 | 一个嵌套结构(廉价) | 一次完整进程启动 |
| 用户自定义频率 | 低(引擎级能力) | 高(工具/守卫类) |

**一句话**:工具/守卫/看板类、想独立分发 → B;需要跑 loop、管权限、流式回传 → A。明确不做的形态:脚本引擎(deno/rquickjs)与 WASM 动态加载,理由见 07 §8.1 对比表。

## 2. 可用接缝盘点

| 接缝 | 位置 | 用途 |
|---|---|---|
| `Tool` trait | `crates/rpi-agent/src/tool.rs:63` | 注册新工具(五合一:name/schema/execute/execution_mode/prompt 片段) |
| `Extension` trait | `crates/rpi-core/src/extensions/mod.rs:124` | 扩展注册壳:`name()` + `init(&mut ExtensionApi)`,可 `register_tool`、拿 `ui()` |
| `LoopHooks` trait | `crates/rpi-agent/src/hooks.rs` | 循环决策点:before/after_tool_call、transform_context、prepare_request 等 |
| `Subscriber` trait | `rpi-agent`(事件汇) | 观察类事件单向广播 |
| `ApprovalUi` / `ExtensionUi` / `ContextCompactor` / `ShellSpawnHook` | 各自模块 | 审批 UI、压缩器、bash spawn 改写 |

**现状警告**:`Extension` trait 是半成品。生产装配点写死空注册表(`crates/rpi-cli/src/assembly.rs:821` 的 `ExtensionRegistry::default()`),全仓库 `impl Extension` 只存在于测试(`crates/rpi-core/tests/extensions_registry.rs`);它也没有事件订阅能力(`ExtensionEventBus` 只包 MCP 连接)。**进程内插件当前可用的生产接线点是 `assembly.rs::build_session()`**——内置 8 工具经 `create_tools_at_with_shell`(`crates/rpi-tools/src/lib.rs:70`)进工具列表,自定义工具在其后 `tools.extend(...)` 即可。若要走 `Extension` 形态,须先补生产接线(settings 开关 + assembly 注册),见 §5。

## 3. 开发 how-to

以新增一个自定义工具为例,全流程四步:

1. **实现 `Tool` trait**(`crates/rpi-agent/src/tool.rs:63`):

   ```rust
   struct MyTool { /* 依赖经构造器注入,不读全局 */ }

   #[async_trait]
   impl Tool for MyTool {
       fn name(&self) -> &str { "my_tool" }
       fn description(&self) -> &str { "..." }
       fn schema(&self) -> Value { /* JSON Schema */ }
       fn execution_mode(&self) -> ToolExecution { ToolExecution::Parallel }
       fn prompt_snippet(&self) -> String { /* 注入系统提示词的工具说明 */ }
       async fn execute(&self, args: Value, updater: &ToolUpdater)
           -> Result<ToolOutput, ToolError> { /* ... */ }
   }
   ```

2. **装配期注册**:`crates/rpi-cli/src/assembly.rs::build_session()` 中,`create_tools_at_with_shell(...)` 产出内置工具后 `tools.push(Arc::new(MyTool::new(...)))`。依赖(如 provider、hooks、permission engine)在装配期拿引用传入。

3. **守纪律**(10 文档):对不改 rpi-tools 的独立工具,建议放业务核(`rpi-core`)或新 crate,经工厂注入,不反向依赖 cli;工具实现不读配置文件,参数由装配层显式传入。

4. **测试**:工具单测 + 会话级集成测试(`crates/rpi-core/tests/session_integration.rs` 模式)+ L2 mock 链路(`--mock`),见 `docs/12-testing-standard.md`。

### 3.1 嵌套使用宿主核心能力

以"execute 内再起一个 agent"为例——这是 subagent 引擎的核心形态:

```rust
// 1. 子 agent 是廉价壳:create_agent(provider, hooks) 即得
let child = rpi_agent::create_agent(provider.clone(), hooks.clone());
// 2. 收紧能力面 = 递归防护的关键:不给子 agent 装本工具,其余按白名单
child.set_system_prompt(agent_def.system_prompt);
child.install_tools(whitelist_tools);          // 只读集可复用 rpi_tools::read_only_tools()
child.set_model(resolved_model);               // 可与父会话不同
// 3. 事件隔离:专属 Subscriber 聚合,不污染父转录
child.subscribe(Arc::new(CollectingSubscriber::default()));
// 4. 跑 + 超时 + 取消
let result = tokio::time::timeout(timeout, child.prompt(task)).await;
// 5. 聚合输出为 ToolOutput.text 返回
```

权限继承是**自动的**:把装配好的 `Arc<dyn LoopHooks>`(含 `ApprovalHooks` 审批洋葱,`crates/rpi-core/src/permission/hooks.rs:63`)原样传给 `create_agent`,子 agent 的每次工具调用都过同一审批引擎。

---

## 4. Subagent 专项设计(已立项,2026-09-28)

**决策**:引擎走路线 A,**同进程嵌套 Agent,不建子进程**。复用同一权限引擎、审批洋葱、事件汇与模型配置。行为参照上游 pi 的 `subagent-lite` 扩展(`/Users/kin/Documents/10source/pi-agent-extensions/subagent-lite`,约 1800 行,进程内实现)。

已确认的两个取向:**同步 agent 允许并行**;**异步完成时自动唤醒父会话新 turn**。

### 4.1 工具面

单一 `subagent` 工具(2026-09-28 实现时由 `task` 改名;settings `tools` 白名单条目同名),参数:

| 参数 | 类型 | 说明 |
|---|---|---|
| `task` | string(必填) | 给子 agent 的任务文本 |
| `agent` | string | 已发现 agent 定义名(与 `systemPrompt` 二选一) |
| `systemPrompt` | string | 内联系统提示 |
| `model` | string | 缺省继承父会话模型 |
| `tools` | string[] | 工具白名单;缺省用 agent 定义的,否则只读集 |
| `async` | bool | 后台执行(默认 false) |
| `timeoutMs` | number | 默认 30 分钟 |
| `action` | `"list"` \| `"stop"` | 管理后台运行(`stop` 需 `id`) |
| `id` | string | `stop` 的目标运行 id |

`execution_mode` 声明 `Parallel`:模型一条消息发多个 task 调用时,复用现有 `execute_batch_parallel`(`crates/rpi-agent/src/loop_.rs:1363`)的 JoinSet 自动并发。并发上限 4(超出排队或报错,实施时定)。

### 4.2 同步 agent(`async: false`)

- `execute` 内 `create_agent` → 裁剪工具面 → `prompt(task)` → **await 到完成**,聚合输出为 `ToolOutput.text`;主 loop 阻塞在该工具槽位(`Phase::ExecutingTools`),多个同步 task 经 Parallel 批并发;
- **递归防护 = 工具面裁剪**:子 agent 工具集里没有 task 工具(单层嵌套);
- 权限继承:传同一 hooks 洋葱(§3.1);
- 超时:`tokio::time::timeout` + `child.abort()`;Ctrl-C 经 cancel token 传播;
- 转录:子 agent 转录内存态,不落主会话 JSONL(对齐 subagent-lite 已知取舍)。

### 4.3 异步 agent(`async: true`)与 supervisor 基建

- `execute` 立即返回 run id;子 agent `tokio::spawn` 进后台,由运行注册表(JoinSet/AbortHandle 集合)统一管理,上限 16;
- **已知基建缺口(必须先补)**:`steer()/follow_up()` 注入通道只在 run 期间被循环消费(`crates/rpi-agent/src/loop_.rs` 的 Wake 机制);父会话空闲时消息只入队、无唤醒。需新增 **supervisor task**:
  1. 监听后台完成事件;
  2. `agent.wait_idle()`(watch 机制,`crates/rpi-agent/src/agent.rs`)等父会话空闲;
  3. 注入 follow_up(内容 = 任务 id + 最终输出摘要)并驱动新 run,模型即刻看到结果并继续;
- **后台 agent 审批策略:默认 Deny(fail-closed)**。后台运行的子 agent 不弹前台审批 UI(避免与主会话 UI 并发冲突);settings 可覆盖。前台同步 agent 照常走交互审批;
- 生命周期:`action: "list"/"stop"`;会话退出/`/new` 时 abort 全部存活运行;Ctrl-C 传播;
- 进度可见:后台运行的事件走独立 Subscriber,聚合后经事件汇供 TUI 展示状态行(不做活组件,符合 07 §8.1 表达力边界)。

### 4.4 agent 类型定义为数据

引擎编译进二进制,**类型定义是数据文件**,加类型不重编译:

- 位置:`<cwd>/.rpi/agents/*.md`(项目优先)→ `~/.rpi/agents/*.md`;
- 格式:frontmatter `name`(缺省文件名)/`description`/`model`/`tools`(dash 列表或 CSV),正文即 system prompt;
- 发现与解析失败:跳过 + 诊断(对齐扩展错误语义);

```markdown
---
name: reviewer
description: Reviews code changes for correctness and style
model: anthropic/claude-sonnet-4-5
tools:
  - read
  - grep
---

You are a meticulous code reviewer. ...
```

### 4.5 实施顺序(建议)

1. 同步 agent 最小闭环:task 工具 + agent 定义发现/解析 + 嵌套 Agent + 递归防护 + 超时(不含异步);
2. 并行:Parallel 声明 + 并发上限;
3. 异步:运行注册表 + `action: list/stop` + 后台 Deny 审批;
4. supervisor 空闲唤醒(独立基建,可先以"等下次交互再消费"过渡);
5. TUI 状态行 + E2E(L3,`docs/12-testing-standard.md`)。

---

## 5. 本路线已知问题登记(实施前需处理或知情)

1. **`Extension` trait 半成品**:生产装配写死空注册表(`assembly.rs:821`),`all()` 从未被消费,无事件订阅能力,与 MCP 路径能力不对等。subagent 引擎若走 `Tool` 直注册则不受影响;若想统一走 `Extension` 形态,须先补 settings 开关 + assembly 接线。
2. **两套 UI trait 的 headless 默认语义相反**:`ApprovalUi` UI 通道关闭 = Deny(`crates/rpi-core/src/permission/hooks.rs:20,122`),`ExtensionUi::confirm` 默认放行(`crates/rpi-core/src/extensions/mod.rs:80`)。审批路径本身安全;elicitation 的 confirm 默认放行对权限敏感扩展是隐患,建议统一 Deny-by-default。
3. **空闲唤醒基建缺口**:见 §4.3,subagent 异步形态的前置依赖。

---

## 源码文件索引

| 文件 | 关键符号 | 优先级 |
|---|---|---|
| `crates/rpi-agent/src/tool.rs` | `Tool`:63, `ToolOutput`:20, `ToolExecution`:56 | P0 |
| `crates/rpi-agent/src/agent.rs` | `Agent`:124, `create_agent`:151, `steer/follow_up`:275/280, `wait_idle` | P0 |
| `crates/rpi-agent/src/loop_.rs` | `execute_batch_parallel`:1363, Wake/注入通道 | P0 |
| `crates/rpi-agent/src/hooks.rs` | `LoopHooks` 全集 | P0 |
| `crates/rpi-core/src/extensions/mod.rs` | `Extension`:124, `ExtensionRegistry`:133, `ExtensionUi`:76 | P0 |
| `crates/rpi-core/src/session.rs` | `create_agent_session`:226, `AgentSessionConfig`:137 | P0 |
| `crates/rpi-core/src/permission/hooks.rs` | `ApprovalUi`:20, `ApprovalHooks`:63, `HeadlessApprovalUi`:43 | P1 |
| `crates/rpi-tools/src/lib.rs` | `ToolRegistry`:36, `read_only_tools`:99, `create_tools_at_with_shell`:70 | P1 |
| `crates/rpi-cli/src/assembly.rs` | `build_session`(工具/钩子装配点),:821 空注册表现状 | P0 |
| 参照实现 | `/Users/kin/Documents/10source/pi-agent-extensions/subagent-lite`(TS,进程内) | P1 |

阅读顺序:本文 → 09(接缝纪律)→ 上表 P0 源码。
