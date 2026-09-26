---
name: rpi-dev-workflow
description: rpi 仓库（Rust workspace，复刻 pi agent）的强制开发流程。在本仓库内做任何开发任务都必须遵循 —— 包括实现新功能、修改或修复任何 crate 的代码、重构、补测试、接扩展、调 UI 等，即使用户没有明说"按流程来"。涉及 docs/ 目录下文档所描述的模块时尤其要触发。
---

# rpi 开发流程

本仓库是 pi agent 的 Rust 复刻，`docs/` 下的文档就是唯一的权威设计来源。代码与文档不一致时，以文档为准；文档有误时先和用户确认再改文档。

## 第一步：读文档（任何开发动作之前）

先完整读取以下基础文档，再进入编码：

- `docs/README.md`
- `docs/00-overview.md`
- `docs/01-data-model.md`
- `docs/09-wiring-and-rust.md`
- `docs/10-implementation-policy.md`

然后根据本次要开发的功能，额外读取对应的功能文档：

| 涉及的 crate / 功能 | 功能文档 |
|---|---|
| `rpi-ai`（Provider、适配器、retry、overflow） | `docs/02-ai-provider.md` |
| `rpi-agent`（循环、Agent、LoopHooks/Tool trait） | `docs/03-agent-loop.md` |
| `rpi-core`（AgentSession、系统提示词、模型解析） | `docs/04-coding-agent.md` |
| `rpi-tools`（内置工具） | `docs/05-tools.md` |
| `rpi-session`（JSONL 会话树、compaction） | `docs/06-session-compaction.md` |
| 扩展机制（extensions registry） | `docs/07-extensions.md` |
| `rpi-tui`（差分渲染 UI、四种模式） | `docs/08-modes-tui.md` |

一次改动跨多个模块时，把对应文档都读了。不要凭记忆写代码 —— 文档会更新，记忆不会。

## 第二步：设计决策的优先级

1. 文档有明确规定的，严格按文档做。
2. 文档没提到的，先看 pi agent（参考实现）是怎么做的，照它的实现来。
3. pi 的做法对当前场景明显不合适（例如依赖了 rpi 不打算引入的东西、与本项目架构冲突），**停下来问用户**，不要自作主张选一个方案。

## 第三步：编码

- 遵守 `10-implementation-policy.md` 的全部约束：crate 只暴露工厂、trait、类型；依赖严格单向；trait 方法不 panic；无可变全局状态。
- 边界与性能是编码要求的一部分，不是事后补充：
  - 边界：空集合、None/Err 传播、JSONL 损坏行、流中断、重入、UTF-8 截断、超长输入等。
  - 性能：热路径上的分配与 clone、大会话的投影/压缩成本、TUI 每帧渲染量。有疑问就先测量，不凭感觉优化。
- 实现类型一律不 `pub`，经工厂出厂（见 policy §2）。

## 第四步：测试（每次代码改动都必须做）

- 任何代码改动都要有对应测试：新功能配正向用例 + 边界用例，bug 修复先写能复现的失败测试再修。
- 每轮改动后跑 `cargo test --workspace`，全绿才算完成；有 clippy 就一并跑。
- 边界与性能的关注点要落到测试上：至少为第三步列出的相关边界各写一个用例。

## 第五步：Reviewer 审查

代码写完、测试全绿之后，派一个**独立的 reviewer**（用 Agent 工具起一个子代理）审查两样东西：

1. 实现代码：是否符合文档与 policy 约束、依赖方向是否正确、边界处理是否完备。
2. 测试代码：是否真的覆盖了新行为和边界，而不是只测了 happy path。

给 reviewer 的提示要包含：本次改动的文件列表、对应的功能文档路径、以及"对照文档与 pi 实现检查"的要求。reviewer 提出的问题必须修完并重新跑测试，才能向用户报告完成。

## 第六步：记录踩坑（开发完成后）

每次开发结束后，在**本次涉及的功能文档**（如 `docs/02-ai-provider.md`）末尾的踩坑记录小节（没有就新建 `## 踩坑记录`）追加条目，格式：

```markdown
- **YYYY-MM-DD <一句话问题>**：现象 → 原因 → 解法。（关联文件：`crates/rpi-ai/src/xxx.rs`）
```

写"下次遇到同样问题能直接绕开"的信息，不写流水账。基础文档（00/01/09/10）原则上不改；若发现它们有错，先向用户确认。
