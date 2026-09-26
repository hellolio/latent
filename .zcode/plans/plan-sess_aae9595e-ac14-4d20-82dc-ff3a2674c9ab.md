## rpi interactive CLI 完善（对齐 pi）

### 背景与根因（已确认）

- **错误不可见**：错误被循环编码为 `stop_reason=Error` 的 assistant 消息走完整事件序（`TurnEnd` 事件带 `message.stop_reason/error_message`），但 `crates/rpi-cli/src/modes/interactive.rs:454` 的 `TurnEnd` 分支只打用量行；`AutoRetryEnd` 事件被 `_ => {}` 吞掉（interactive.rs:469）。错误文本从未上屏。
- **历史不回放**：`run_interactive_mode`（interactive.rs:197）启动只调 `welcome()`，从不把 `BuiltSession.session_manager.branch_entries()` 渲染进 scrollback。`--continue` 时上下文在、模型能看到，用户看不到。

对照源码：本机 `/Users/kin/Documents/10source/pi`（commit d5629e204，与 docs 所记一致）。

### 改动内容

**A. 错误渲染（修 bug 1）** — `crates/rpi-cli/src/modes/interactive.rs`
- `TurnEnd` 分支按 pi 的 assistant-message.ts 语义渲染：`Error` → 红字 `Error: {error_message}`；`Aborted` → 红字 `Operation aborted`；`Length` → 红字截断提示。错误/中止回合不打用量行（顺带修掉 docs/11 提到的"错误回合 0 用量也打印"问题）。
- 消费 `AutoRetryEnd`：重试最终失败 → 红字 `Retry failed after N attempts: ...`。
- `handle_key` 里 `let _ = session.prompt()` 改为检查返回值兜底。

**B. 历史回放（修 bug 2）** — 对齐 pi 的回放深度
- 新增回放函数：遍历 `branch_entries()`（leaf→root 反转为时间序），渲染——user 消息反色块（`REVERSE`）；assistant 正文；`ToolCall` 与后续 `ToolResult` 消息配对成一行摘要（错误结果红字）；`Compaction`/`BranchSummary` 渲染为 dim 摘要行；结尾如有压缩补 `Session compacted N times`。
- 回放后渲染 welcome 横幅（对齐 pi：bold 标题 + 键位提示 + `/help` 提示）。

**C. 斜杠命令框架** — 新模块 `crates/rpi-cli/src/modes/slash.rs`
- 命令表 + 分发，Enter 提交时先查命令再走 prompt；未识别的 `/xxx` 本地警告 `Unknown command: /foo（/help 查看可用命令）`，不发给模型。
- 实现：`/help`、`/quit`、`/model [provider/model]`（无参弹 `SelectList` 选择器，数据源为 `ModelResolver` 新增 `list_models()`：内置默认表 + models.json）、`/thinking [level]`（off/minimal…max，复用 assembly.rs 的 `parse_thinking_level`）、`/session`（会话文件/id/model/thinking/累计用量）、`/compact` 保留。
- `/model` `/thinking` 走 `AgentSession::set_model/set_thinking_level`（已有，含落盘）。

**D. Footer 状态栏** — 对齐 pi footer 核心信息
- 状态行扩为两行：第 1 行活动状态；第 2 行 `model · thinking · context% · Σ tokens $cost`。context% = 最后一条 assistant `usage.total_tokens` / `model.context_window`，>70% 黄、>90% 红（同 pi 阈值）。model/thinking 变化即时刷新。

**E. 退出语义对齐 pi** — 双击 Ctrl+C（500ms 内）退出，首次按提示 `Press Ctrl+C again to exit`；流式中 Ctrl+C 仍先 abort；Ctrl+D 空编辑器退出保留。

### 测试
沿用现有 `MainScreenTui` + `SharedVec` 断言模式，新增：错误/中止/重试失败渲染、未知命令警告、回放渲染（user 反色/toolcall 配对/压缩行）、`/thinking` 解析、`/model` 参数路径、footer context% 阈值变色、双击 Ctrl+C 状态机；`ModelResolver::list_models()` 单测。最后跑全量 `cargo test` + `clippy` + `fmt`。

### 收尾
实现并测试通过后，派一个子 agent 对 diff 做代码与测试审查（性能、实现方针、与 pi 一致性），按审查结论修复后交付。