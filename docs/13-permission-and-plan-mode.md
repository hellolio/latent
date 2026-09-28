# 13 — 权限系统与计划模式(Permission & Plan Mode)

> **一句话**:三种会话模式(Plan / Confirm / FullAccess)一次完整交付 —— 权限判定引擎 + 人工审批流 + OS 级沙箱 + 计划模式提示词,全部挂在现有接缝上:`before_tool_call` 唯一拦截点、`ShellSpawnHook` 沙箱包装点、`set_active_tools_by_name` 工具收紧点。语义对标 OpenAI Codex CLI(`codex-rs`)。

本文档是**设计文档 + 实施清单**。所有引用的 file:line 以 2026-09-28 代码为准。

---

## 目录

1. [背景与目标](#1-背景与目标)
2. [总体模型](#2-总体模型)
3. [核心类型定义](#3-核心类型定义)
4. [审批架构与数据流](#4-审批架构与数据流)
5. [三种模式语义详表](#5-三种模式语义详表)
6. [权限判定引擎](#6-权限判定引擎)
7. [沙箱设计](#7-沙箱设计)
8. [Plan 模式详解](#8-plan-模式详解)
9. [持久化与恢复](#9-持久化与恢复)
10. [CLI 与交互](#10-cli-与交互)
11. [Headless 模式语义](#11-headless-模式语义)
12. [配置项汇总](#12-配置项汇总)
13. [测试计划](#13-测试计划)
14. [实施任务清单(单一交付)](#14-实施任务清单单一交付)
15. [风险与开放问题](#15-风险与开放问题)
16. [附录:Codex 对照表](#16-附录codex-对照表)

---

## 1. 背景与目标

### 1.1 现状

rpi 目前**没有任何权限、审批、沙箱机制**。检索 `approve/permission/confirm/sandbox/policy/allow/deny` 全仓库,最接近的三个机制都不是权限系统:

| 现有机制 | 位置 | 与本设计的关系 |
|---|---|---|
| `ToolBlock`(hook 拦截/改参) | `crates/rpi-agent/src/hooks.rs:75` | 审批拒绝的**复用载体**(`block=true` → `ToolOutcome::Blocked`) |
| `ExtensionUi::confirm/select` | `crates/rpi-core/src/extensions/mod.rs` | 仅 MCP elicitation 用;其 interactive/rpc/headless 三种 UI 实现模式是审批 UI 的**先例样板** |
| `settings.tools` 静态激活集 | `crates/rpi-cli/src/assembly.rs:133` | 静态装配期开关,不是逐调用审批;Plan 模式的工具收紧走运行期 `set_active_tools_by_name` |

### 1.2 目标

1. **三种会话模式**,用户可自由切换(运行期 slash 命令 / 快捷键,装配期 CLI / settings):
   - **Plan**(默认:新建 session 即进入):只有可读权限,OS 沙箱隔离,提示词要求模型先列计划;
   - **Confirm**(确认模式):拥有全部工具,但文件修改与命令执行需按权限规则确认后执行;
   - **FullAccess**(完全访问):全自动,无审批、无沙箱(对标 Codex `danger-full-access`)。
2. **权限判定引擎**:工具风险分类 + shell 命令前缀规则 + 会话级批准缓存 + 可写根判定,纯函数可测。
3. **OS 级沙箱**:macOS Seatbelt(SBPL)、Linux bwrap + Landlock 备选,shell 子进程强制隔离;无沙箱平台/工具时降级为审批,绝不静默裸跑。
4. **审批 UI 三通道**:interactive(TUI 弹窗)、rpc(JSONL 反向通道)、print/json(headless 策略)。
5. **持久化**:模式变更落会话 entry,resume 恢复;settings 全键可配。

### 1.3 非目标

- **不做** Codex 的 `UnlessTrusted`(execpolicy 项目信任级别)、`GranularApprovalConfig` 细粒度审批位、自动 reviewer(`--approve-for-me`)、受管网络代理(MITM)。这些在 §15 开放问题登记,需要时按同一接缝扩展。
- **不做** Windows 沙箱(restricted token)。Windows 上 Confirm 模式全部走审批、Plan 模式禁用 bash(见 §7.5)。
- **不做**写白名单的跨会话持久化(用户逐次批准的 shell 前缀规则只进会话缓存 + 显式配置文件;不做自动写回 `~/.rpi` 规则文件)。

### 1.4 关键洞察:三层防线与两个"工具世界"

rpi 的工具分两个执行世界,权限手段不同,设计必须分开处理:

- **进程内工具**(read/edit/write/grep/find/ls):不存在子进程,OS 沙箱管不到。防线 = 工具集收紧(哪些工具可用)+ 权限判定引擎(路径是否越界、是否需审批)。
- **shell 子进程**(bash/powershell):spawn 出去后进程内判定失效。防线 = 命令级判定 + **OS 沙箱包装**(Seatbelt/bwrap/Landlock)。

因此整体是三层防线,逐层兜底:

```
第 1 层  工具集     Plan 模式收掉 powershell,暴露 read/grep/find/ls/bash
                    (复用 set_active_tools_by_name,session.rs:457)
第 2 层  权限引擎   每次工具调用前判定:Allow / Ask(审批)/ Deny
                    (挂在 before_tool_call,hooks.rs:146)
第 3 层  OS 沙箱    shell 子进程按 SandboxPolicy 强制隔离
                    (挂在 ShellSpawnHook,bash.rs:34)
```

---

## 2. 总体模型

### 2.1 与 Codex 的维度对照

Codex 有两个正交维度:**审批策略**(`AskForApproval`:untrusted / on-request / never)决定"越权时问不问用户",**沙箱策略**(`SandboxPolicy`:read-only / workspace-write / danger-full-access)决定"进程能碰什么"。两者自由组合,共 9 格,对普通用户过于复杂。

rpi 把常用组合**收敛为用户可见的三种模式**,底层仍保留两个正交类型(§3),模式只是它们的**预设映射**:

| SessionMode | SandboxPolicy | 审批行为 | 对标 Codex |
|---|---|---|---|
| `Plan` | `ReadOnly` | 非只读操作一律 **Deny**(不是 Ask) | Codex Plan mode(preset)≈ read-only + 只读工具 + developer 指令 |
| `Confirm` | `WorkspaceWrite` | 越界操作 **Ask**;规则/缓存命中则 Allow | `workspace-write` + `on-request` |
| `FullAccess` | `DangerFullAccess` | 全部 **Allow** | `danger-full-access` + `never`(≈ `--yolo`) |

收敛的理由:用户心智只有"安全档位"一维;未来需要细粒度(如 `untrusted`)时,在 `PermissionEngine` 里加规则即可,不动模式层(§15)。

### 2.2 数据流总览

```
                    ┌────────────────────────── rpi-cli 装配(assembly.rs) ─────────────────────────┐
                    │                                                                              │
  settings.json ────┤  SessionSettings{session_mode, sandbox, rules}                                │
  CLI flags ────────┤        │                                                                     │
                    │        ▼                                                                     │
                    │  PermissionEngine ◄─── ApprovalHooks(实现 LoopHooks)──包── ExtensionHooks ──包── PassthroughHooks
                    │        │            │  before_tool_call: evaluate → Ask? → ApprovalUi       │
                    │        │            │                    await oneshot 应答                │
                    │  SandboxSpawnHook ◄─── ShellSpawnOptions.spawn_hook(实现 rpi_tools::ShellSpawnHook)
                    │        │            │  rewrite: 按当前 SandboxPolicy 包装命令               │
                    └────────┼────────────┼──────────────────────────────────────────────────────┘
                             ▼            ▼
                      loop_.rs:1492   bash.rs spawn
                    (prepare_call)   (真正子进程,已被沙箱包住)
```

审批等待期间,循环天然暂停:`before_tool_call` 是 async hook,在 `prepare_call`(`loop_.rs:1493`)被 await;hook 内 await 人工应答,不需要改循环状态机。拒绝走 `ToolBlock{block:true}` → `ToolOutcome::Blocked`(`loop_.rs:340`)→ 错误 tool result(`loop_.rs:1613` 附近),拒绝原因自动进转录,模型可继续换路径。

---

## 3. 核心类型定义

### 3.1 类型放置(遵守 10 文档 §2 三规则)

| 类型/模块 | 放置 crate | 理由 |
|---|---|---|
| `SessionMode`、`ToolRiskClass`、`Verdict`、`ApprovalRequest/Decision`、`PermissionEngine`、`ApprovalHooks`、`SessionModeChange` 事件 | **rpi-core**(新模块 `permission/`) | 纯业务概念;rpi-core 已依赖 rpi-agent,可实现 `LoopHooks` |
| `SandboxPolicy`、`SandboxAvailability`、`trait Sandbox`、Seatbelt/bwrap/Landlock 实现 | **新 crate `rpi-sandbox`**(L2,与 rpi-tools 平级) | 平台能力,零内部依赖(纯类型 + 平台 API);**可选组件**,拆卸后其余 crate 照常编译(10 文档 §1 判据)。rpi-core/rpi-tools 按需依赖其类型 |
| `SandboxSpawnHook`(实现 `rpi_tools::ShellSpawnHook`) | **rpi-cli 装配层** | 它粘合 rpi-sandbox 与 rpi-tools,只有装配点知道两者 |
| TUI 审批弹窗、rpc 审批通道 | rpi-cli 各 mode | UI 是模式的事(接缝 #5) |

rpi-sandbox 保持"零内部依赖"意味着 `SandboxPolicy` 定义在 rpi-sandbox、被 rpi-core 引用 —— 依赖方向 core → sandbox,合规(L3 依赖 L2)。

### 3.2 会话模式与风险分类(rpi-core)

```rust
/// 会话模式(用户可见的安全档位)。新会话默认 Plan。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionMode {
    Plan,        // 只读 + 沙箱 + 列计划(默认)
    Confirm,     // 全工具,变更前确认
    FullAccess,  // 全自动
}

impl Default for SessionMode { fn default() -> Self { SessionMode::Plan } }

/// 工具风险分类。内置工具静态映射;MCP 扩展工具一律 External。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRiskClass {
    ReadOnly,   // read / grep / find / ls
    FileWrite,  // edit / write
    Shell,      // bash / powershell
    External,   // 一切 MCP 扩展工具(mcp__* 前缀或非内置名)
}

/// 单次工具调用的判定结论。
pub enum Verdict {
    /// 放行(hook 返回 None)
    Allow,
    /// 需人工审批(载荷展示给用户)
    Ask(ApprovalRequest),
    /// 直接拒绝(hook 返回 ToolBlock{block:true, reason})
    Deny(String),
}

/// 审批请求载荷(UI 渲染依据)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub risk: ToolRiskClass,
    /// 触发原因(决定 UI 文案与可用选项)
    pub reason: ApprovalReason,
    /// 展示文本:shell 命令原文 / 将写入的文件路径 / 工具参数摘要
    pub detail: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalReason {
    /// 文件写入越出可写根(Confirm)
    OutsideWorkspace,
    /// 需审批的 shell 命令(Confirm;规则未放行)
    ShellCommand,
    /// MCP 扩展工具(Confirm)
    ExternalTool,
    /// 沙箱不可用,升级为无沙箱执行需批准(Confirm;对标 Codex no-sandbox approval)
    NoSandboxEscalation,
}

/// 用户决策(对标 Codex ReviewDecision 的 rpi 子集)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// 批准这一次
    Approve,
    /// 本会话内同类请求(同 ApprovalKey)自动批准
    ApproveForSession,
    /// 拒绝本次,会话继续(拒绝原因作为错误 tool result 给模型)
    Deny,
    /// 拒绝并终止本次 run(block + terminate)
    Abort,
}
```

### 3.3 沙箱类型(rpi-sandbox)

```rust
/// 沙箱策略(对标 Codex SandboxPolicy 的收敛子集)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SandboxPolicy {
    /// 全盘只读 + 网络关
    ReadOnly,
    /// 全盘可读 + 可写根内可写
    WorkspaceWrite {
        /// 额外可写根;cwd 与系统临时目录自动并入(§7.3)
        #[serde(default)]
        writable_roots: Vec<String>,
        #[serde(default)]
        network_access: bool,
    },
    /// 不设防(FullAccess 模式;等价于不包装)
    DangerFullAccess,
}

/// 平台沙箱可用性(启动时探测一次)。
pub enum SandboxAvailability {
    MacosSeatbelt,   // /usr/bin/sandbox-exec 存在
    LinuxBwrap,      // bwrap 在 PATH
    LinuxLandlock,   // 内核支持 Landlock(备选,无外部依赖)
    None,            // Windows / 探测失败
}

/// 沙箱执行器工厂 + trait(10 文档 §2 规则 1/2:工厂出厂,上游只见 trait)。
pub trait Sandbox: Send + Sync {
    /// 把命令包装为沙箱内执行。返回 Err(String) = 无法构造沙箱。
    fn wrap_command(&self, command: &str, cwd: &Path) -> Result<String, String>;
    fn policy(&self) -> &SandboxPolicy;
}

pub fn create_sandbox(policy: &SandboxPolicy) -> Result<Option<Arc<dyn Sandbox>>, String>;
// None = DangerFullAccess(无需沙箱);Err = 策略要求沙箱但平台不可用
```

### 3.4 审批 UI trait(rpi-core)

不复用 `ExtensionUi`(其 `confirm(message) -> bool` 表达力不够:审批要展示命令/diff、四种决策),新增独立 trait,与 `ExtensionUi` 平行注入:

```rust
/// 审批 UI 接缝(接缝 #5 的姊妹面):mode 提供实现,core 的 ApprovalHooks 调用。
#[async_trait]
pub trait ApprovalUi: Send + Sync {
    /// 返回 None = UI 通道关闭(rpc 客户端断连/print 模式),按 Deny 处理。
    async fn request_approval(&self, request: ApprovalRequest) -> Option<ApprovalDecision>;
}
```

---

## 4. 审批架构与数据流

### 4.1 唯一拦截点:`before_tool_call`

**不新增接缝、不改循环状态机**。审批是 `LoopHooks::before_tool_call`(`hooks.rs:146`,调用点 `loop_.rs:1493`)的一个装饰器实现,插在 hooks 洋葱最外层(`assembly.rs:477-484` 现有洋葱上再包一层):

```rust
// 装配(assembly.rs build_session 内,替换现有 477-484 行的 hooks 构造):
let engine = Arc::new(PermissionEngine::new(session_mode, rules));
let hooks: Arc<dyn LoopHooks> = ApprovalHooks::new(
    if bus.is_empty() {
        Arc::new(PassthroughHooks) as Arc<dyn LoopHooks>
    } else {
        Arc::new(ExtensionHooks::new(Arc::new(PassthroughHooks), bus.clone()))
    },
    engine.clone(),
    approval_ui,          // Arc<dyn ApprovalUi>,mode 提供
    subscribers.clone(),  // 广播 ApprovalRequested/Resolved 事件
);
```

顺序:**Approval 最外 → Extension 最内**。审批先问(便宜、人审),批准后才轮到扩展埋点;审批拒绝时扩展不感知(省一次分发)。

### 4.2 ApprovalHooks 执行流程

```rust
#[async_trait]
impl LoopHooks for ApprovalHooks {
    async fn before_tool_call(&self, ctx: ToolCallCtx) -> Option<ToolBlock> {
        let risk = classify_tool(&ctx.name);            // §6.1
        // 1. 引擎判定(纯函数 + 会话缓存 + 当前模式)
        match self.engine.evaluate(&ctx, risk) {
            Verdict::Allow => return self.inner_before(ctx).await,   // 透传内层
            Verdict::Deny(reason) => return Some(ToolBlock { block: true, reason, ..Default::default() }),
            Verdict::Ask(request) => {
                // 2. 广播事件(UI 渲染弹窗),再 await 人工应答
                self.broadcast_requested(&request).await;
                let decision = self.ui.request_approval(request.clone()).await
                    .unwrap_or(ApprovalDecision::Deny);       // UI 通道关闭 = Deny
                self.broadcast_resolved(&request, decision).await;
                // 3. 决策落地
                match decision {
                    ApprovalDecision::Approve => {
                        self.cache_key(&request);             // 仅本次?不需要,Approve 不入缓存
                        self.inner_before(ctx).await
                    }
                    ApprovalDecision::ApproveForSession => {
                        self.engine.approve_for_session(&request);  // 写缓存
                        self.inner_before(ctx).await
                    }
                    ApprovalDecision::Deny => Some(ToolBlock {
                        block: true,
                        reason: format!("用户拒绝了该操作:{}", request.detail),
                        ..Default::default()
                    }),
                    ApprovalDecision::Abort => Some(ToolBlock {
                        block: true,
                        reason: "用户中止".into(),
                        terminate: Some(true),                // 批全部 terminate → 提前结束 run(loop_.rs:1300 语义)
                        ..Default::default()
                    }),
                }
            }
        }
    }
    // convert_to_llm 等其余方法全部透传 inner
}
```

要点:

- **暂停即 await**:hook 是 async,`prepare_call` await 它;串行批逐个工具调用(`loop_.rs:1310`),并行批的 prepare 阶段也是顺序执行 → 多个审批天然排队弹出,一次一个,无并发竞态(§15 讨论)。
- **拒绝原因进转录**:`ToolBlock.reason` 变成错误 tool result,模型看到"用户拒绝了…"可自行调整 —— 与 Codex `ReviewDecision::Denied` 语义一致(拒绝但会话继续)。
- **Abort 复用 terminate**:`ToolBlock.terminate` 参与批结算"全部 terminate 提前结束 run"(`loop_.rs:1300` 附近、`tool.rs:26`),单工具调用批中即等于终止本次 run。run 结束原因仍为 `RunStop::EndTurn`(现状语义,UI 可凭 ApprovalResolved 事件区分展示)。

### 4.3 审批事件(ui 只消费 AgentSessionEvent,接缝 #5)

`AgentSessionEvent`(`crates/rpi-core/src/session.rs:86`)新增两个变体:

```rust
pub enum AgentSessionEvent {
    // ... 现有变体不变 ...
    /// 审批请求已弹出(hook 内广播;UI 据此绘制弹窗/压队列)
    ApprovalRequested { request: ApprovalRequest },
    /// 审批已决策(含会话缓存命中自动批准的,decision 照发)
    ApprovalResolved { tool_call_id: String, decision: ApprovalDecision },
}
```

广播方式复用 `SessionRetryHooks::spawn_event` 的既有模式(`session.rs:344-353`):ApprovalHooks 持有 `subscribers` 列表,`tokio::spawn` 转发(审批广播不参与 run 结算语义)。wire 形态:json/rpc 模式的 `session_event_to_json`(`modes/mod.rs`)对两个新变体各加一个映射分支即可。

**为什么不加到 `AgentEvent`(rpi-agent)**:审批是业务概念,rpi-agent 基座不应感知(10 文档 §1 分层);`AgentSessionEvent` 正是为"agent 事件之上追加业务面"而设。

### 4.4 三条 UI 反向通道

| 模式 | 实现 | 机制 |
|---|---|---|
| **interactive** | `TuiApprovalUi`(新,`modes/interactive/`) | 复用 interactive 现有 UI 通道格局(`events.rs::create_tui_ui` 的 mpsc 模式):`request_approval` 发 `ApprovalRequested` 渲染消息给主循环 → 主循环进入审批 overlay(§10.3)→ 用户按键 → 经预建的 oneshot 回传决策。请求方(hook)与应答方(键盘处理)通过 `Arc<Mutex<HashMap<id, oneshot::Sender>>>` 关联 —— 结构照抄 `RpcUi`(`modes/rpc.rs:87-163`) |
| **rpc** | `RpcApprovalUi`(新) | 完全照抄 `RpcUi` 的 id 路由:上行 `{"type":"approval_request","id":N,"request":{...}}`,等客户端 `{"type":"approval_response","id":N,"decision":"approve"}`;新增 `RpcCommand::ApprovalResponse { id, decision }` 变体(`rpc.rs:33` 处的枚举)。客户端断连走 `close_all()` 同款兜底(`rpc.rs:160`) |
| **print / json** | `HeadlessApprovalUi` | 无交互面。按 settings `headlessApproval` 策略(§12):`deny`(默认)→ 直接返回 `Deny`(不经 await,零阻塞);`auto-approve` → 返回 `Approve`(CI 明知风险显式开启)。两种都不弹任何东西 |

---

## 5. 三种模式语义详表

| 维度 | Plan(默认) | Confirm | FullAccess |
|---|---|---|---|
| 可用工具集 | `read` `grep` `find` `ls` `bash`(见下注) | 全部 8 个内置 + 扩展 | 全部 + 扩展 |
| `SandboxPolicy` | `ReadOnly`(网络关) | `WorkspaceWrite{cwd+tmp 可写, 网络关}` | `DangerFullAccess`(不包装) |
| read/grep/find/ls | Allow | Allow | Allow |
| edit / write(路径在可写根内) | **Deny**(原因:Plan 模式只读) | **Ask**(`ShellCommand` 之外的第二类必审项) | Allow |
| edit / write(越出可写根) | Deny | Ask(`OutsideWorkspace`) | Allow |
| bash(只读命令,§6.2 判定) | Allow(**仍在 ReadOnly 沙箱内执行**) | Allow | Allow |
| bash(非只读命令) | Deny(原因:Plan 模式只读) | Ask(`ShellCommand`) | Allow |
| powershell | 工具集内不存在(收掉) | 同 bash 规则 | Allow |
| MCP 扩展工具 | 工具集内不存在(收掉) | Ask(`ExternalTool`) | Allow |
| 系统提示词 | 附加 `<mode>` 节(§8.2) | 无附加节 | 无附加节 |
| 状态栏 | `plan` 标记(黄色) | `confirm` 标记 | `full-access` 标记(红色) |
| 切换方式 | `/mode`、Shift+Tab、CLI、settings | 同左 | 同左 |

> **Plan 模式为何保留 bash**:列计划需要探索环境(git log、grep、跑测试看现状)。bash 保留但受双重约束:权限引擎只放行只读命令(§6.2),且 OS 层强制 ReadOnly 沙箱(§7)。powershell 与 MCP 工具在 Plan 模式直接从激活集收掉(模型看不到 schema,从源头杜绝);bash 不能收掉,所以走判定 + 沙箱。
>
> **Plan 模式被拒的语义**:Deny(不是 Ask)。计划模式中不弹审批框 —— 用户意图就是"别动任何东西",模型收到拒绝原因后应继续只读探索或输出计划。

模式与工具集的关系实现:三模式的"基线激活集"如下,模式切换时经 `AgentSession::set_active_tools_by_name`(`session.rs:457`)应用,自动落 `ToolSetChange` entry、重建提示词 sections:

```
Plan:        [read, grep, find, ls, bash]
Confirm:     [read, bash, powershell, edit, write, grep, find, ls] + 扩展工具
FullAccess:  同 Confirm
```

用户 settings `tools` 显式配置与模式的交集语义:settings 是**用户上限**,模式是**安全上限**,实际激活集 = 两者交集(Plan 模式强制剔除 powershell/MCP 工具,即使 settings 要求)。

---

## 6. 权限判定引擎

新模块 `crates/rpi-core/src/permission/`(`mod.rs` + `engine.rs` + `rules.rs` + `shell.rs`)。

### 6.1 工具分类

```rust
/// 内置工具名 → 风险类的静态映射;未知名字(MCP 扩展)一律 External。
pub fn classify_tool(name: &str) -> ToolRiskClass {
    match name {
        "read" | "grep" | "find" | "ls" => ToolRiskClass::ReadOnly,
        "edit" | "write" => ToolRiskClass::FileWrite,
        "bash" | "powershell" => ToolRiskClass::Shell,
        _ => ToolRiskClass::External,
    }
}
```

文件写工具的路径提取:`edit`/`write` 的 schema 中路径字段为 `path`(`rpi-tools` 各工具 schema;实现时以 schema 实测为准),从 `ctx.args["path"]` 取相对路径,基于会话 cwd 解析并 `canonicalize` 后参与可写根判定。路径解析失败(如参数缺失)按"越界"保守处理。

### 6.2 shell 命令只读判定(`shell.rs`,纯函数)

```rust
/// 判定一条 shell 命令是否只读(可安全免审)。
pub fn is_readonly_command(command: &str) -> bool;
```

规则(保守优先,宁可误判为非只读):

1. **元字符即非只读**:命令含 `|` `&&` `||` `;` `>` `>>` `<` `` ` `` `$(` 换行中的任意一个 → 非只读(管道右侧重写文件、重定向写盘都无法静态保证)。
2. **前缀白名单**:首个 token(含 `env VAR=x cmd`、`sudo -n` 剥离后的实际命令)命中内置只读前缀表 → 只读。内置表(初版,可经 settings `approval.allowCommands` 追加):

   ```
   ls cat head tail wc file stat readlink realpath which whereis type
   grep rg find fd ls-tree
   git status git log git diff git show git blame git branch git tag
   git remote git rev-parse git describe git shortlog git ls-files
   cargo check cargo tree cargo metadata rustc --version python3 -c(只读校验类不进表,保持保守)
   echo printf pwd date whoami uname hostname env printenv
   ```

3. **否定优先于白名单**:`git push`、`curl`、`rm` 等即使部分前缀相似也不在表中;deny 规则(settings `approval.denyCommands`,如 `"git push"`)命中一律非只读。

### 6.3 引擎主判定

```rust
pub struct PermissionEngine {
    /// 当前模式(运行期可切换;RwLock,与 set_mode 同锁)
    mode: RwLock<SessionMode>,
    policy: SandboxPolicy,
    rules: ApprovalRules,                       // settings 的 allow/deny 规则
    cache: Mutex<HashSet<ApprovalKey>>,         // 会话级批准缓存
    cwd: PathBuf,
    writable_roots: Vec<PathBuf>,               // 由 policy + cwd 预计算(§7.3)
}

/// 审批缓存键(对标 Codex ApprovalCacheKey):决策可复用的最小单元。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ApprovalKey {
    /// 规范化后的整条命令(空白折叠)
    ShellCommand(String),
    /// canonical 文件路径(edit/write 各自独立键)
    FilePath(PathBuf),
    /// 扩展工具名
    ExternalTool(String),
}

impl PermissionEngine {
    pub fn evaluate(&self, ctx: &ToolCallCtx, risk: ToolRiskClass) -> Verdict { /* §6.4 */ }
    pub fn approve_for_session(&self, request: &ApprovalRequest) { /* 键入缓存 */ }
    pub fn set_mode(&self, mode: SessionMode) { /* 切换时清空缓存 */ }
}
```

判定流程(每步短路):

```
FullAccess                        → Allow
risk == ReadOnly                  → Allow(所有模式)
缓存键命中(仅 Confirm 会产生 Ask)→ Allow
mode == Plan:
    FileWrite                     → Deny("Plan 模式为只读;先完成计划,退出 Plan 模式后再修改文件")
    Shell + is_readonly_command   → Allow(沙箱兜底)
    Shell(非只读)                → Deny("Plan 模式只允许只读命令:{detail}")
mode == Confirm:
    FileWrite:路径在可写根内 → Ask(理由=ShellCommand 同级的文件确认;越界 → OutsideWorkspace)
    Shell:is_readonly_command → Allow;否则 deny 规则命中 → Deny,否则 Ask(ShellCommand / NoSandboxEscalation)
    External → Ask(ExternalTool)
```

Confirm 模式下 Ask vs NoSandboxEscalation 的区分:平台沙箱可用 → 请求以"将在沙箱内执行"文案 Ask;平台沙箱不可用(§7.5)→ 同一条命令以"无法沙箱隔离,将以无沙箱方式执行"文案 Ask(`NoSandboxEscalation`)。用户批准的是"这次执行方式",缓存键相同 —— 无沙箱平台批准一次后,同命令后续不再打扰。

### 6.4 与 ShellSpawnHook 的分工

`before_tool_call` 判定发生在**工具执行前**(决定"问不问/拒不拒");`ShellSpawnHook`(`bash.rs:34`)发生在**spawn 前**(决定"怎么跑")。两者职责不重叠:审批层不包装命令,沙箱层不问人。`SandboxSpawnHook::rewrite` 只做纯包装(§7.4),不返回 Err 拒绝 —— 拒绝是权限引擎的事。

---

## 7. 沙箱设计

新 crate `rpi-sandbox`(L2,零内部依赖,可选组件)。一次性完整实现三个后端,无分期。

### 7.1 平台探测与选择

```rust
pub fn detect_availability() -> SandboxAvailability {
    // macOS:  /usr/bin/sandbox-exec 存在(fs::metadata,绝对路径,防 PATH 注入 —— 对齐 Codex
    //         seatbelt.rs 的 MACOS_PATH_TO_SEATBELT_EXECUTABLE)
    // Linux:  bwrap 在 PATH → LinuxBwrap;否则内核支持 Landlock(检查
    //         /sys/kernel/security/lsm 含 landlock 或 probe syscall)→ LinuxLandlock
    // 其他:   None
}
```

`create_sandbox(&SandboxPolicy)` 按平台返回对应实现;`ReadOnly`/`WorkspaceWrite` 在 `None` 平台上返回 `Err`(策略要求沙箱但平台不支持)→ 装配层降级为"Confirm 模式全部审批 + Plan 模式禁 bash"(§7.5),**绝不静默裸跑**。

### 7.2 macOS Seatbelt(SBPL)

实现要点(对齐 Codex `sandboxing/src/seatbelt.rs`):

- 命令形态:`/usr/bin/sandbox-exec -p '<profile>' /bin/sh -c '<command>'`。profile 由模板拼装,参数用环境占位注入可写根:

```
(version 1)
(deny default)
; 全盘可读(两个策略都是 full-disk-read)
(allow file-read*)
; 写权限:仅可写根
(allow file-write* (subpath "WRITABLE_ROOT_0") (subpath "WRITABLE_ROOT_1") ...)
; 进程与基础系统操作
(allow process-exec* (subpath "/usr") (subpath "/bin") (subpath "/sbin") (subpath "/opt/homebrew"))
(allow process-fork) (allow sysctl-read) (allow mach-lookup ...) ; 平台必需集,按实测增补
; 网络:ReadOnly 一律关;WorkspaceWrite 默认关, network_access=true 时 (allow network*)
(deny network*)
```

- ReadOnly 与 WorkspaceWrite 的 profile 差异**只有**可写根列表与网络两条;模板单测做字符串断言(§13)。
- 可写根先 `canonicalize`,再解析 symlink 祖先防绕过(对齐 Codex 对 `read_only_subpaths` 的处理);`.git`、`.rpi` 以 `(deny file-write* (subpath ".../.git"))` 显式压在可写根之后(SBPL 后规则覆盖前规则)。

### 7.3 可写根计算(`WorkspaceWrite`)

```
writable_roots = canonicalize(cwd)
               + ${TMPDIR}(macOS 每用户临时目录)/ /tmp(Unix 通用,取存在者)
               + settings sandbox.writableRoots(用户显式追加)
永远只读(叠加在所有根之上):<root>/.git、<root>/.rpi、<root>/.env
```

### 7.4 Linux:bwrap 优先,Landlock 备选

- **bwrap**(对齐 Codex 现架构:文件系统限制交给 bubblewrap):

  ```
  bwrap --unshare-all --share-net(仅 network_access=true 时)
       --ro-bind / /                     全盘只读视图
       --bind <root> <root>...           可写根放开
       --dev /dev --proc /proc
       --die-with-parent --new-session
       -- /bin/sh -c '<command>'
  ```

- **Landlock 备选**(bwrap 未安装时;对齐 Codex legacy landlock 思路,`landlock` crate):进程内 `landlock_restrict_self`,RULES:读全盘、写仅可写根、`LANDLOCK_ACCESS_FS` 按策略;网络限制 Landlock 覆盖不到(ABI v4 前无 network 控制)→ `network_access=false` 时回退到"拒绝执行需网络命令"不可行,**如实降级**:Landlock 后端下 `network_access=false` 仅文档提示不强隔离,或经 seccomp(`seccompiler`)过滤 `socket(2)` 的非 AF_UNIX 调用 —— 初版直接采用 seccomp 过滤 socket,与 bwrap 后端同一依赖(`seccompiler` crate),行为一致。

### 7.5 降级矩阵(平台 × 模式)

| 平台 | Plan | Confirm | FullAccess |
|---|---|---|---|
| macOS(Seatbelt 可用) | 只读命令沙箱内跑;非只读 Deny | 非只读命令沙箱内跑 + Ask | 无沙箱无审批 |
| Linux(bwrap/Landlock 可用) | 同上 | 同上 | 同上 |
| Windows / 探测失败 | **bash 从激活集剔除**(退化为纯 read/grep/find/ls) | 命令一律 Ask(`NoSandboxEscalation`);文件写照常 Ask | 无沙箱无审批 |

装配期把降级结果打 stderr 诊断(对齐扩展开局诊断格式,`assembly.rs:466-471`),如 `[rpi][sandbox] 未检测到可用沙箱,Confirm 模式将逐命令请求批准`。

### 7.6 接入点:`SandboxSpawnHook`

```rust
/// rpi-cli 装配层:实现 rpi_tools::ShellSpawnHook,按当前模式取 SandboxPolicy 包装命令。
pub struct SandboxSpawnHook {
    engine: Arc<PermissionEngine>,          // 读当前模式 → policy
    sandbox_cache: Mutex<HashMap<SessionMode, Option<Arc<dyn Sandbox>>>>,
}
#[async_trait]
impl ShellSpawnHook for SandboxSpawnHook {
    async fn rewrite(&self, command: String) -> Result<String, String> {
        match self.sandbox_for_current_mode() {   // DangerFullAccess → None → 原样返回
            None => Ok(command),
            Some(sandbox) => sandbox.wrap_command(&command, &cwd),  // 包装失败 → Err(拒绝 spawn)
        }
    }
}
```

复用现有装配参数链:`BuildOptions.spawn_hook`(`assembly.rs:298`)→ `ShellSpawnOptions.spawn_hook` → bash/powershell 工具(`assembly.rs:533-540`)。bash 工具零改动;`commandPrefix` 语义保持(前缀包在沙箱之外,如 `timeout 300 sandbox-exec ...` 的顺序在实现时以"沙箱包整条命令、prefix 包沙箱"验证)。

注意:`spawn_hook` 与审批缓存无关 —— 命令改写发生在审批之后,模型与用户看到的都是原始命令。

---

## 8. Plan 模式详解

### 8.1 触发与生命周期

- **新会话默认 Plan**:`SessionMode::default() == Plan`;装配(`build_session`)、`/new`(`switch_new_session`,`assembly.rs:241`)、resume(无 ModeChange entry 的旧会话)三个入口都落到 Plan。用户 settings `sessionMode` 改变新会话默认值(§12)。
- **简化版退出流程**(已确认不做 Codex 三选项弹窗):

  ```
  模型输出 <proposed_plan>…</proposed_plan> 计划块 → run 正常结束(EndTurn)
      → TUI 对计划块做特殊渲染(标题 + 边框,§10.4)
      → 用户自行审阅 → /mode confirm(或 Shift+Tab)→ 发 "开始实现,按计划执行"
  ```

  模型不被赋予"退出计划模式"的能力 —— 切换是用户的动作,不设 `exit_plan_mode` 工具。计划文本天然在转录里(assistant 消息),切模式后模型可见,无需额外注入。
- **resume**:模式从最后一条 ModeChange entry 恢复(§9);CLI 显式 `--session-mode` 优先于 entry。

### 8.2 系统提示词:新增 `<mode>` section

Plan 模式下装配/切换时向 `SystemPromptOptions.sections`(`system_prompt.rs:43`)注入 `mode` 节(BTreeMap 有序,渲染于 `<rules>` 后),经 `AgentSession::update_system_prompt`(`session.rs:553`)运行期重建。文本(参考 Codex `collaboration-mode-templates/templates/plan.md` 收敛):

```xml
<mode>
You are in Plan mode (read-only). Your goal is to produce an implementation plan, not to change anything.

1. Explore: read code, run read-only commands (git log/diff, grep) to understand the current state.
2. Clarify: if requirements are ambiguous, ask the user before planning.
3. Plan: produce a step-by-step implementation plan.

Hard rules:
- You MUST NOT create, modify, or delete files. Write operations are blocked and will be rejected.
- Only read-only shell commands are allowed (no redirects, pipes into writers, package installs, or network commands).
- Do not attempt to bypass restrictions by rephrasing a mutation as a read.

When the plan is final, output it inside a single block:
<proposed_plan>
- step 1 ...
- step 2 ...
</proposed_plan>
After the plan block, stop and end your turn. The user will review it and switch modes when ready.
</mode>
```

切出 Plan 模式时移除该节 + 工具集放宽,一次 `set_mode` 完成(§8.4)。

### 8.3 `<proposed_plan>` 的渲染与识别

- TUI(与 print)对 assistant 文本中的 `<proposed_plan>...</proposed_plan>` 块特殊渲染:提取为带边框的"实施计划"卡片(实现点:markdown 渲染前置的文本预处理,`rpi-tui` 渲染管线;原始文本仍按普通消息入转录,渲染只是展示层)。
- json/rpc 模式原样透传(编辑器客户端自行识别)。
- **与 TODO/计划工具的关系**:rpi 无 `update_plan` 工具,不引入 —— Codex 明确区分"Plan mode(协作模式)"与"update_plan(TODO 工具)",且禁止二者混用。rpi 的计划就是计划块文本,不需要状态化 TODO;若未来加 TODO 工具,按 Codex 语义在 Plan 模式禁用并报"这是 checklist 工具,与 Plan 模式无关"。

### 8.4 `AgentSession::set_mode`(rpi-core 新方法)

模式切换的**唯一入口**,组合三个既有机制,一次落盘:

```rust
impl AgentSession {
    /// 切换会话模式:工具集 + 系统提示词 mode 节 + entry 落盘,原子完成。
    pub async fn set_mode(&self, mode: SessionMode) -> Result<(), CoreError> {
        let names = mode_baseline_tools(mode);            // §5 表
        self.set_active_tools_by_name(&names).await?;     // 复用:重建提示词工具节 + ToolSetChange entry
        // 在 runtime.system_prompt_options.sections 上增删 "mode" 节后整体重建
        self.apply_mode_section(mode)?;                   // 内部走 update_system_prompt
        self.permission_engine.set_mode(mode);            // 引擎切档 + 清审批缓存
        if let Some(sink) = &self.session_sink {
            sink.append_mode_change(mode.as_str()).await?; // §9
        }
        Ok(())
    }
}
```

`PermissionEngine` 与 `AgentSession` 的关联:引擎实例在 cli 装配层创建,经 `AgentSessionConfig` 新字段 `permission: Option<Arc<PermissionEngine>>` 注入 core(core 只持 Option,不装配 = 行为退化为现状,可拆卸)。`set_mode` 时引擎经该字段触达。

---

## 9. 持久化与恢复

### 9.1 会话 entry:新增 `ModeChange`

`crates/rpi-session/src/entry.rs:57` 的 `Entry` 枚举追加变体,形态对齐 `ThinkingLevelChange`:

```rust
/// 会话模式变更(元数据,不进模型上下文):恢复时按此重建模式。
#[serde(rename_all = "camelCase")]
ModeChange {
    id: String,
    #[serde(default)]
    parent_id: Option<String>,
    mode: String,          // "plan" | "confirm" | "full-access"
    #[serde(default)]
    timestamp: i64,
},
```

配套:

- `SessionSink`(`rpi-core/src/session.rs:40`)加默认空实现方法 `append_mode_change(&self, _mode: &str)`(既有模式:`append_tool_set_change` :52);`SessionManagerSink`(`assembly.rs:686`)实现为 `manager.append_mode_change(...)`。
- `SessionManager`(`rpi-session/src/manager.rs`)加 `append_mode_change`(对齐 `append_thinking_level_change` :479 附近的内联写盘)。
- `projection`(`rpi-session/src/projection.rs`)加 `mode: Option<String>` 字段,扫描 branch entries 取最后一条 ModeChange;`build_session_context` 返回值带上,装配 resume 分支(`assembly.rs:520-529`)回填。
- `CURRENT_SESSION_VERSION`(`entry.rs:16`)5 → 6:新变体写入;读取侧对旧版本文件保持兼容(旧文件无 ModeChange = 投影 mode=None = 默认 Plan)。若现有加载器做严格版本校验,同步放宽为 `version <= CURRENT`。

### 9.2 与 ToolSetChange 的次序

`set_mode` 内部先 `set_active_tools_by_name`(落一条 ToolSetChange)再 `append_mode_change`,两条 entry 相邻;恢复时以 **ModeChange 为准**重建模式,再由模式推导基线工具集(ToolSetChange 仅用于用户在模式内做过 `/tools` 级微调的场景 —— 当前无此命令,次序约束是为将来留兼容)。恢复路径:resume 时若投影 mode 存在,装配层按 §5 表推导工具集,与 seed_active_tools 的既有回填(`assembly.rs:566-572`)合并:模式基线为底,seed 覆盖交集之外的部分。

### 9.3 settings 持久化

见 §12。注意分工:**settings 决定"新会话的初始模式"**;entry 决定"这个会话当前是什么模式"。`/new` 新建时按 settings 默认(Plan)落一条 ModeChange entry,使新会话文件自描述(对齐 `switch_new_session` 落模型/思考级别 entry 的既有做法,`assembly.rs:270-277`)。

---

## 10. CLI 与交互

### 10.1 CLI 参数(`main.rs::parse_args`,:51)

注意 `--mode` 已被运行模式(interactive/print/json/rpc)占用,会话模式用独立长参数:

| 参数 | 语义 |
|---|---|
| `--session-mode <plan\|confirm\|full-access>` | 会话模式;缺省读 settings `sessionMode`,再缺省 `plan` |
| `--plan` | `--session-mode plan` 的别名(高频快捷) |
| `--yolo` | `--session-mode full-access` 的别名(对标 Codex `--dangerously-bypass-approvals-and-sandbox`;help 里注明风险) |
| `--sandbox-write <dir>` | 追加可写根(可重复;仅 Confirm 有意义) |
| `--sandbox-network` | Confirm 模式放行沙箱内网络访问 |

组合约束:FullAccess 与沙箱参数互斥,同传报参数错误(对标 Codex 的 clap conflict)。

### 10.2 slash 命令与快捷键

按 slash.rs 四步扩展法(`COMMANDS` 表 `slash.rs:13` → `SlashAction` 枚举 :58 → `parse` match :93 → `execute_command` 分派 `handlers.rs`):

- `/mode` — 无参:显示当前模式与切换提示;`/mode plan|confirm|full-access`:切换。执行即 `session.set_mode(...).await` + 状态栏刷新 + 系统消息("已切换到 confirm 模式",走转录 system message,对齐 `/model` 的做法)。
- **Shift+Tab** 循环 Plan → Confirm → FullAccess → Plan(对标 Codex BackTab;键盘处理在 interactive 键盘线程,运行中 ignore 的约束对齐现有 steering 快捷键的处理)。
- 流式期间切换:允许(引擎与工具集在下一工具调用生效;正在流式的 turn 不打断)。ToolSetChange/ModeChange entry 在 set_mode 内联落盘,顺序安全。

### 10.3 TUI 审批弹窗

- 触发:`AgentSessionEvent::ApprovalRequested` 事件 → interactive 主循环渲染模态 overlay(压在输入框上方),内容:风险原因文案 + 命令/路径详情(等宽块)+ 决策列表。
- 选项与快捷键(对标 Codex `approval_overlay.rs` 的列表选择):

  ```
  │ bash  rm -rf build/                     │
  │ 原因: 需要审批的 shell 命令(confirm 模式) │
  │ > 批准一次            (Enter/1)          │
  │   本会话批准同类命令    (2)                │
  │   拒绝                 (Esc/3)            │
  │   中止本次任务          (Ctrl+C/4)         │
  ```

- 排队:多个请求串行到达(prepare 顺序保证),overlay 逐个处理;处理完当前才显示下一个。
- 应答:TuiApprovalUi 的 oneshot 回传(§4.4);`ApprovalResolved` 事件到达时若 overlay 还在(超时/竞态)则强制关闭。
- 状态栏:header/footer 显示当前模式标记;Plan 模式用醒目颜色提示"计划模式 · 只读"。

### 10.4 计划卡片

`<proposed_plan>` 块渲染为独立边框卡片 + "计划模式产出的实施计划"标题 + 提示行:"确认后 `/mode confirm` 并让模型开始实现"(§8.1 简化流程的 UI 引导)。

---

## 11. Headless 模式语义

| 模式 | 审批 | 沙箱 | 默认模式 |
|---|---|---|---|
| **print** | `HeadlessApprovalUi`:按 `headlessApproval`(deny 默认 / auto-approve) | 与 interactive 一致(平台探测) | settings `sessionMode`(默认 plan;脚本场景建议显式 `--session-mode confirm` 或 `--yolo`) |
| **json** | 同 print | 同上 | 同上 |
| **rpc** | `RpcApprovalUi` 反向通道(§4.4),客户端不响应时挂起等待(客户端断连 → Deny 兜底) | 同上 | 同上 |

print/json 默认 `deny` + Plan 默认意味着:`echo "修复 bug" | rpi` 会得到一份计划而不是改动 —— 这是设计意图;脚本用户显式传 `--yolo` 或在 settings 固化 `sessionMode`。

---

## 12. 配置项汇总

优先级:**CLI flag > 项目 `.rpi/settings.json` > 全局 `~/.rpi/settings.json` > 内置默认**(解析复用 `read_settings_files` 的"项目优先、首个非空生效"既有管线,`assembly.rs:74`)。

### settings.json 新键

```jsonc
{
  // 会话模式:新会话初始档位(默认 "plan")
  "sessionMode": "plan",                      // plan | confirm | full-access
  // headless(print/json)遇审批请求的策略(默认 "deny")
  "headlessApproval": "deny",                 // deny | auto-approve
  // 沙箱(Confirm 模式的 WorkspaceWrite 细节;Plan 固定 ReadOnly,FullAccess 固定关)
  "sandbox": {
    "writableRoots": ["../shared-lib"],       // 额外可写根
    "networkAccess": false
  },
  // 审批规则(Confirm 模式下免审/必禁)
  "approval": {
    "allowCommands": ["make test", "npm run build"],  // 前缀匹配,追加到内置只读表
    "denyCommands": ["git push", "rm -rf /"]          // 命中即 Deny,优先于 allow
  }
}
```

### CLI / 环境变量

| 项 | 值 | 默认 |
|---|---|---|
| `--session-mode` | plan \| confirm \| full-access | settings `sessionMode` → `plan` |
| `--plan` / `--yolo` | 别名 | — |
| `--sandbox-write <dir>` | 路径 | — |
| `--sandbox-network` | flag | off |
| (无新环境变量) | — | — |

### 类型落位

`SessionSettings`(`assembly.rs:378`)新增 `session_mode: SessionMode`、`sandbox: SandboxConfig`、`approval: ApprovalRules`、`headless_approval: HeadlessApproval` 字段;`BuildOptions` 同步透传;`build_session` 据此构造 `PermissionEngine` 与 `SandboxSpawnHook`。

---

## 13. 测试计划

按 12 文档分层,全部为本次交付内补齐:

### L0 单元(纯函数,量最大)

| 位置 | 用例 |
|---|---|
| `permission/engine.rs #[cfg(test)]` | 判定矩阵全排列:3 模式 × {ReadOnly, FileWrite(根内/越界), Shell(只读/非只读/deny 规则), External} × {缓存未命中/命中};`set_mode` 清缓存;FullAccess 短路 |
| `permission/shell.rs` | `is_readonly_command`:元字符拒绝(管道/重定向/命令替换/换行)、前缀表命中、`env`/`sudo -n` 剥离、deny 优先、大小写与空白规范化 |
| `rpi-sandbox` 各后端 | SBPL profile 生成:ReadOnly(无可写根+deny network)/ WorkspaceWrite(根列表、`.git`/`.rpi` deny 后置、symlink 祖先解析)字符串断言;bwrap 参数序列断言;`create_sandbox` 在策略-平台不匹配时的 Err |
| `assembly.rs` | settings 新键解析(项目优先/回退/坏 JSON 跳过,复用既有 TempDir 测试范式 :929) |

### L1 集成

| 位置 | 用例 |
|---|---|
| `rpi-core/tests/permission_hooks.rs`(新) | `ScriptedProvider` 出一次 tool_call:ApprovalHooks 在 Confirm 模式对 write 发 Ask → mock ApprovalUi 决策四种各一例;ApproveForSession 后第二次同键调用不再 Ask;Deny 的 reason 进 tool result;Abort 终止 run(`RunStop::EndTurn` + terminate 语义) |
| `rpi-core/tests/session_integration.rs`(增) | `set_mode` 组合:工具集切换 + mode 节增删 + ModeChange/ToolSetChange entry 落盘次序;resume 投影回填 mode |
| `rpi-session/tests/session_tree_and_compaction.rs`(增) | ModeChange entry 序列化/投影 mode 字段/旧版本文件兼容 |
| `rpi-cli/tests/modes.rs`(增) | SandboxSpawnHook 包装:bash 命令经 ShellSpawnOptions 进入 wrap_command(FullAccess 原样;Confirm 带沙箱前缀);rpc 审批往返(approval_request → approval_response,照抄 e2e_mcp_extension 的内存缓冲测试法) |

### L2 TUI 屏幕(`rpi-tui/src/app.rs` tests)

审批 overlay 渲染(命令详情、四选项、选中态);计划卡片边框渲染;状态栏三模式标记。

### L3 真终端 E2E(`tests/e2e/`)

| 场景 | 断言 |
|---|---|
| `scenarios/plan_mode.json` + `test_plan_mode.py` | 新会话启动即 Plan:反向断言 mock LLM 收到的 system prompt 含 `<mode>` 与只读规则;场景让模型调 `write` → 屏幕出现拒绝文案("Plan 模式为只读"),无审批弹窗;模型输出 `<proposed_plan>` → 屏幕出现计划卡片;`/mode confirm` 后重发 → write 触发审批 overlay(键盘 1 批准)→ 工具真实执行 |
| `scenarios/approval_flow.json` + `test_approval_flow.py` | Confirm 模式:首个 bash 触发弹窗 → Esc 拒绝 → 模型收到拒绝原因并继续;同命令再次调用 → 仍弹(未缓存)→ 按 2 本会话批准 → 第三次同类命令**不弹**直接执行;Ctrl+C 中止路径 |
| `test_new_session.py`(既有,增断言) | `/new` 后状态栏回到 plan;新会话文件含 ModeChange entry |

---

## 14. 实施任务清单(单一交付)

全部条目属于**同一次交付**,按依赖关系排序(checklist,完成即勾):

**A. 类型与配置底座**
- [ ] A1 rpi-sandbox crate 骨架:`SandboxPolicy`/`SandboxAvailability`/`trait Sandbox`/`create_sandbox`(§3.3),workspace 注册,L2 层
- [ ] A2 rpi-core `permission/` 模块:`SessionMode`/`ToolRiskClass`/`Verdict`/`ApprovalRequest`/`ApprovalDecision`/`ApprovalKey`(§3.2)
- [ ] A3 settings 新键解析 + `SessionSettings`/`BuildOptions` 透传(§12)

**B. 判定引擎**
- [ ] B1 `classify_tool` + 路径提取与可写根判定(§6.1)
- [ ] B2 `is_readonly_command` + 内置只读前缀表 + allow/deny 规则(§6.2)
- [ ] B3 `PermissionEngine::evaluate` 全矩阵 + 会话缓存(§6.3)
- [ ] B4 平台探测 + SBPL 模板生成 + bwrap 参数 + Landlock+seccomp 后端(§7.1–7.3)

**C. 审批流**
- [ ] C1 `ApprovalHooks` 装饰器(hooks 洋葱最外层,事件广播,四决策落地)(§4.2)
- [ ] C2 `AgentSessionEvent` 新变体 + json/rpc wire 映射(§4.3)
- [ ] C3 `trait ApprovalUi` + 三实现:`TuiApprovalUi`/`RpcApprovalUi`/`HeadlessApprovalUi`(§4.4,§11)
- [ ] C4 rpc:`approval_request/approval_response` 反向通道 + `RpcCommand::ApprovalResponse`(§4.4)
- [ ] C5 循环配合:`before_tool_call` 的 await 与 cancel token `select!`,使审批等待可被 abort 打断(打断 = Blocked + "已中止";`loop_.rs:1492` 调用点小改)

**D. 模式与 Plan**
- [ ] D1 `AgentSession::set_mode` + `AgentSessionConfig.permission` 注入(§8.4)
- [ ] D2 `<mode>` 提示词节 + 三模式基线工具集推导(§5,§8.2)
- [ ] D3 `<proposed_plan>` 渲染(TUI 卡片;print 纯文本)(§8.3)
- [ ] D4 `SandboxSpawnHook` 接入 `ShellSpawnOptions`(§7.6)

**E. 持久化与恢复**
- [ ] E1 `Entry::ModeChange` + `SessionManager::append_mode_change` + projection.mode(§9.1)
- [ ] E2 `SessionSink::append_mode_change` + `SessionManagerSink` 实现 + resume 回填(§9.1–9.2)
- [ ] E3 `/new` 落初始 ModeChange(§9.3)

**F. CLI 与交互**
- [ ] F1 `--session-mode/--plan/--yolo/--sandbox-*` 参数 + 组合约束(§10.1)
- [ ] F2 `/mode` 命令四步扩展 + Shift+Tab 循环(§10.2)
- [ ] F3 TUI 审批 overlay + 状态栏模式标记(§10.3)

**G. 测试补齐(§13 全表)与诊断输出(§7.5 降级提示)**

**验收标准**(对齐 10 文档 §5 不变条款):`cargo test --workspace` 全绿;`rpi --mock` 走通三模式切换 + 审批全决策路径;拆卸 `rpi-sandbox` 后其余 crate 零警告编译;接缝变更按 10 文档 §3 登记(`AgentSessionEvent` 负载变更、`SessionSink` 新默认方法、`AgentSessionConfig` 新字段)。

---

## 15. 风险与开放问题

| # | 风险/问题 | 处置 |
|---|---|---|
| 1 | **bash 命令解析的绕过**:只读前缀表是启发式,`git log` 后跟分号注入等已被元字符规则挡住,但 `python3 -c "open('x','w')"` 这类单命令写盘挡不住 | 双保险:Plan 模式所有 bash(包括只读判定通过的)都在 ReadOnly OS 沙箱内执行,写盘在 OS 层失败;无沙箱平台直接收掉 bash。判定只做第一道筛,不承诺完备 |
| 2 | **并行工具批的多次审批**:prepare 顺序执行使审批天然串行,但一次批里 5 个写文件会连弹 5 次 | 交付按逐个弹(行为正确);体验优化(合并同类、一次批全)留开放问题,接缝支持(缓存键已按文件粒度) |
| 3 | **审批等待与 abort**:oneshot await 不会被 cancel token 自动打断 | C5:`loop_.rs` 调用点包 `select!`;另 TuiApprovalUi 侧 Ctrl+C 直接回 `Abort` 决策,双通道兜底 |
| 4 | **`sandbox-exec` 的未来**:Apple 未承诺长期保留 | 探测失败自动落入降级矩阵(§7.5),行为仍安全(多审批);bwrap 经 Homebrew 可作为 macOS 备选(开放问题:是否实现 `SandboxAvailability::LinuxBwrap` 在 macOS 的探测) |
| 5 | **ApproveForSession 缓存的键粒度**:整条命令规范化后作键,`ls -la` 与 `ls -a` 是两个键 | 与 Codex 一致(命令指纹);文件键按 canonical 路径。首版不做前缀泛化,开放问题 |
| 6 | **print 模式默认 Plan 对脚本的破坏性** | 文档明示(§11)+ `--yolo`/`--session-mode` 逃生口;开放问题:是否对 `--continue` 的 print 保留原会话模式(当前设计:保留,entry 恢复) |
| 7 | **rpi-sandbox 测试不能真跑沙箱断言**(CI 无 macOS Seatbelt 的可控环境) | L0 做 profile/参数生成的纯字符串断言;L1 用包装后的命令串断言;真沙箱行为由 L3 在开发机手测 + `test_plan_mode.py` 里对写盘失败做容忍性断言(平台有沙箱才断言) |
| 8 | **FullAccess 的误触**:`--yolo` 无确认 | help 文案红色警告;状态栏红色 `full-access` 常显;开放问题:是否加首次使用的 stderr 提示 |

---

## 16. 附录:Codex 对照表

实现期查阅用(源码路径基于 `openai/codex` main 分支,codex-rs/):

| rpi 机制 | Codex 对应 | 参考源码 |
|---|---|---|
| `SessionMode` 三档 | preset 化的 collaboration mode + approval/sandbox 组合 | `protocol/src/config_types.rs`(ModeKind/SandboxMode)、`models-manager/src/collaboration_mode_presets.rs` |
| `PermissionEngine::evaluate` | `ExecApprovalRequirement::{Skip, NeedsApproval, Forbidden}` 三态判定 | `core/src/tools/sandboxing.rs` |
| `ApprovalKey` 会话缓存 | `ApprovalCacheKey` + `with_cached_approval()` | `core/src/tools/sandboxing.rs` L71-117 |
| `ApprovalDecision` | `ReviewDecision`(Approved/ApprovedForSession/Denied/Abort…) | `protocol/src/protocol.rs` ~L4157 |
| `ApprovalRequest` | `ExecApprovalRequestEvent`(含 `available_decisions` 驱动 UI 选项) | `protocol/src/approvals.rs` |
| `SandboxPolicy` | 同名类型(ReadOnly/WorkspaceWrite/DangerFullAccess + writable_roots/network_access) | `protocol/src/protocol.rs` ~L1072、`protocol/src/permissions.rs` |
| Seatbelt | 硬编码 `/usr/bin/sandbox-exec` + SBPL 模板拼装 + symlink 祖先防护 | `sandboxing/src/seatbelt.rs` + `seatbelt_*.sbpl` |
| bwrap + seccomp(Landlock 备选) | 同架构(bwrap 管文件系统,seccomp 管网络) | `sandboxing/src/bwrap.rs`、`linux-sandbox/src/{linux_run_main,landlock}.rs` |
| 降级(无沙箱 → 审批) | `wants_no_sandbox_approval()` / `escalate_on_failure()` | `core/src/tools/sandboxing.rs` |
| Plan 提示词 | `templates/plan.md`(include_str! 内嵌)+ 每消息携带模式 | `collaboration-mode-templates/templates/plan.md`、`core/src/context/world_state/collaboration_mode.rs` |
| `<proposed_plan>` | 同名块 + "Implement this plan?" 三选项(rpi 简化为用户手切) | `tui/src/chatwidget/plan_implementation.rs` |
| 审批 overlay | `ApprovalRequest` 四类 → ListSelectionView,Esc=Cancel | `tui/src/bottom_pane/approval_overlay.rs` |
| Shift+Tab 循环 | BackTab `cycle_collaboration_mode()` | `tui/src/chatwidget/interaction.rs` L224-240 |
| `--yolo` | `--dangerously-bypass-approvals-and-sandbox`(强制 Never + DangerFullAccess) | `cli/src/main.rs` L1988-1997 |
| rpc 审批通道 | app-server 的 approval 双向 RPC | `app-server-protocol/src/protocol/v2/` |

**有意不采纳的 Codex 机制**(§1.3):`UnlessTrusted` 项目信任、`GranularApprovalConfig` 审批位、execpolicy 规则文件(`$CODEX_HOME/rules`)自动追加、受管网络代理、auto reviewer、Windows restricted token。全部可在现接缝上按需追加,不需要破坏性变更。
