# 开发文档：《权限系统与计划模式设计》

**产出**：`docs/13-permission-and-plan-mode.md`（写入前核对 docs/ 现有编号，取下一个可用编号），单份综合文档，中文，与 docs/ 现有文档风格一致。

## 已确认的决策
- 三种模式（用户可自由切换）：**Plan**（默认，新 session 即进入）/ **确认模式**（写文件与命令需逐次确认）/ **完全访问**（全自动）。
- **不分期、不分里程碑**：权限系统、三模式、OS 级沙箱（macOS Seatbelt / Linux bwrap+seccomp/Landlock）、计划模式作为一次完整交付，文档中的实施章节是同一交付内的完整任务清单（按依赖排序的 checklist，但不是分批上线）。
- 计划退出流程：简化版（模型输出计划块后结束 turn，用户自行确认后切模式并发"开始实现"）。
- 对标 Codex 实现（`AskForApproval` × `SandboxPolicy` 正交、`ReviewDecision`、session 级批准缓存、plan mode prompt 注入 + 只读工具集 + OS 沙箱），文档附 Codex 对照表。

## 文档大纲（各章要点）

1. **背景与目标** — 现状（rpi 无任何权限/审批/沙箱）、目标、非目标、Codex 语义对照。
2. **总体模型** — 三个正交维度分析（会话模式 / 沙箱策略 / 审批策略），本设计将其收敛为用户可见的三模式；`SessionMode { Plan, Confirm, FullAccess }` 与底层 `SandboxPolicy`/审批引擎的映射表。
3. **核心类型定义**（Rust 签名级）— `SessionMode`、`ApprovalDecision { Approve, ApproveForSession, Deny, Abort }`、`ApprovalRequest/Resolution`、`SandboxPolicy { ReadOnly, WorkspaceWrite{writable_roots, network_access}, DangerFullAccess }`、`ToolRiskClass { ReadOnly, FileWrite, Shell, Network }` 等，标注所属 crate（权限引擎放 rpi-core，审批 hook 装饰器放 rpi-cli 装配层，类型下沉位置论证）。
4. **审批架构与数据流** — 以 `LoopHooks::before_tool_call`（hooks.rs:146，调用点 loop_.rs:1492）为唯一拦截点：async hook 内 await oneshot 应答实现"暂停等审批"；批准=返回 None，拒绝=复用 `ToolBlock`→`ToolOutcome::Blocked`；hooks 洋葱插入位置（assembly.rs:477，`ApprovalHooks` 包在 `ExtensionHooks` 内层）；事件流（`AgentEvent`/`AgentSessionEvent` 增加 ApprovalRequest/Resolved）；UI 反向通道三条路（interactive TUI 选择列表、rpc 照抄 `RpcCommand::ExtensionUiResponse` 的 id 路由、print/json 的 headless 降级语义=默认拒绝）。
5. **三种模式语义详表** — 每种模式的：可用工具集、沙箱策略、哪些操作触发审批、系统提示词差异、状态栏显示、切换约束。
6. **权限判定引擎** — 工具风险分类（read/grep/find/ls=只读；edit/write=FileWrite；bash=Shell 需命令级解析；MCP 工具默认最高风险）；shell 命令前缀解析与 allow/deny 规则（挂 `ShellSpawnHook`，bash.rs:24）；审批缓存键设计（命令指纹 / 文件路径，对标 Codex `ApprovalCacheKey`）；"本次会话允许"的内存缓存；写路径越界（workspace 外写入）判定。
7. **Sandbox 设计（一次性完整实现）** — macOS Seatbelt（`sandbox-exec` + SBPL profile 生成，writable_roots 注入，绝对路径防 PATH 注入）；Linux（bwrap + seccomp，Landlock 备选）；平台选择与降级策略（无沙箱可用→审批升级，绝不静默裸跑）；writable_roots 自动含 cwd//tmp，`.git`/`.rpi` 永远只读；网络开关。Plan 模式 = ReadOnly 沙箱 + 只读工具集双保险。
8. **Plan 模式详解** — 新 session 默认进入；系统提示词 section（参考 Codex plan.md 模板：探索→澄清→规格三阶段，禁止变异操作，计划定稿输出 `<proposed_plan>` 块）；工具集收紧路径（`read_only_tools` 工厂 + `AgentSession::set_active_tools_by_name`，自动落 `ToolSetChange` entry）；计划块在 TUI 的特殊渲染；简化退出流程；与 TODO/计划工具的关系（对标 Codex update_plan 与 plan mode 的明确分离）。
9. **持久化与恢复** — settings.json 新键（`sessionMode` 默认 `plan`、`sandbox`、审批规则）；会话 entry 增加 `ModeChange` 变体（或复用 `Entry::Custom`）；resume 时按 entry 恢复模式（对齐 seed_active_tools 恢复路径）；`/new` 新会话重置为 plan 模式。
10. **CLI 与交互** — CLI 参数（`--mode plan|confirm|full-access`，默认 plan）；`/mode` slash 命令（按 slash.rs 四步扩展法）；Shift+Tab 循环切换；TUI 审批弹窗（选项：批准一次 / 本次会话批准 / 拒绝 / 中止，Esc=拒绝）；模式指示。
11. **Headless 模式语义** — print/json（无 UI：可配置 deny 或 full-access，默认 deny 并给出明确报错）；rpc（完整审批双向通道，供编辑器集成）。
12. **配置项汇总表** — settings.json 键、CLI flag、环境变量全集及优先级（CLI > 项目 settings > 全局 settings）。
13. **测试计划** — 按 docs/12 分层：单测（判定引擎纯函数）、集成（hook 拦截/恢复、模式切换 entry、沙箱 profile 生成）、e2e（tests/e2e/scenarios 新增 plan_mode、approval_flow 场景，含反向断言 LLM 请求体中系统提示词含 plan section）。
14. **实施任务清单（单一交付）** — 按依赖关系排序的完整 checklist：类型与配置 → 审批引擎与 hook → 事件与 UI 三通道 → 三模式与切换 → Plan 提示词与工具收紧 → 沙箱（macOS/Linux）→ 持久化恢复 → 测试补齐。全部属于同一次交付，无分期上线。
15. **风险与开放问题** — 并行工具批内多次审批的 UI 排队、审批等待时的 abort 语义、bash 复杂命令解析的绕过风险、Windows 沙箱缺席等。
16. **附录** — Codex 关键源码路径与机制对照表（protocol.rs / sandboxing/ / approval_overlay.rs / plan.md 等），便于实现期查阅。

## 执行步骤
1. 核对 docs/ 目录现有编号，确定文件名。
2. 通读 `docs/10-implementation-policy.md`、`docs/12-testing-standard.md` 与 `06-session-compaction.md` 开头，对齐文档风格与编号引用惯例。
3. 补充精读挂载点代码（session.rs 的 `set_active_tools_by_name`/`SessionSink`、assembly.rs 装配洋葱、event.rs、rpc.rs 反向通道），确保文档中每个 file:line 引用准确。
4. 撰写文档（以上 16 章），内部自审一遍引用与一致性后交付。