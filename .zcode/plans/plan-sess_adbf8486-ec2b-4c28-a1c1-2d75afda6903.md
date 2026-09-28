创建 `docs/11-plugin-development.md`（插件开发说明），并修正过时的决策记录。不动 Rust 代码，subagent 实现作为文档定稿后的下一步。

## 一、docs/11-plugin-development.md 内容

**1. 插件路线盘点**：进程内编译期扩展（`Extension` trait + Tool/LoopHooks/Subscriber 等 trait 接缝，含 `assembly.rs:821` 生产接线为空的现状说明）与进程外 MCP 扩展（settings.json `mcpServers`、rpi/register、rpi/event、fail 语义）两条路线的能力对比与选型表。

**2. 两条路线开发 how-to**：
- 路线 A（编译进二进制）：实现 `Tool` trait → assembly 注册 → 嵌套 `create_agent` 要点（工具白名单裁剪即递归防护、权限洋葱继承、事件聚合隔离、超时/取消）；
- 路线 B（独立项目 MCP）：任意语言 rmcp stdio server + wire 协议细节（`rpiResult` 信封、fail-open/closed、高频事件双声明、工具名前缀）。

**3. Subagent 专项设计**（含本次确认的决策）：
- **引擎走路线 A：同进程嵌套 Agent，不建子进程**（复用权限引擎/事件汇/审批洋葱）；
- **同步 agent**：task 工具 `execute` 内 await 子 agent，声明 `ToolExecution::Parallel` 支持一条消息多个 task 并发（复用 `execute_batch_parallel` JoinSet），并发上限（建议 4）；超时 + cancel token 传播；不给子 agent 装 task 工具 = 递归防护；
- **异步 agent**：`async: true` 立即返回 run id，`tokio::spawn` + JoinSet 管理；**需新增 supervisor 基建**：监听后台完成 → `wait_idle()` → 注入 follow_up 自动唤醒新 turn（现有 steer/follow_up 通道只在 run 期间被消费，空闲态无唤醒，这是已知缺口）；后台 agent 审批默认 Deny（fail-closed）；`action: list/stop` 管理；会话退出清理全部存活运行；子 agent 转录内存态不落主会话 JSONL；
- **agent 类型定义为数据**：`.rpi/agents/*.md`（frontmatter name/description/model/tools，正文 = system prompt），项目目录优先，加类型无需重编译；
- 参数面对齐 subagent-lite：task/agent|systemPrompt/model/tools/async/timeoutMs/action。

**4. 已知问题登记**：`Extension` trait 半成品（生产死代码）、两套 UI trait headless 默认语义相反（ApprovalUi Deny vs ExtensionUi::confirm 放行）、空闲唤醒基建缺口、wire 协议无版本协商。

## 二、修正 docs/rpi-architecture-fix-tasks.md

第 14 行与 1258 行"暂时不要实现 Subagent"更新为"Subagent 已立项，按 docs/11 设计实施"，避免后续 coding agent 按旧决策拒绝。