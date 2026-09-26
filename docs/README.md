# rpi 设计参考文档 — pi agent 源码分析

本目录是对 [earendil-works/pi](https://github.com/earendil-works/pi)(原 badlogic/pi-mono,下称 **pi**)核心实现的完整分析文档,作为用 Rust 重写 pi agent(rpi)的设计蓝本。

**分析基准**:本地仓库 `/Users/kin/Documents/10source/pi`,commit `d5629e204`(2026-09-24),全包版本 `0.87.1`,Node >= 22.19,纯 ESM TypeScript monorepo。
**引用约定**:文中 `文件:行号` 均相对 pi 仓库根,如 `packages/agent/src/agent-loop.ts:162`。

## 文档目录

| 文档 | 内容 | 对应 pi 包 |
|---|---|---|
| [00-overview.md](00-overview.md) | 总览与设计哲学、12 包结构、构建体系 | 全仓 |
| [01-data-model.md](01-data-model.md) | 核心数据模型:消息、模型、工具、事件类型 | `packages/ai`、`packages/agent` |
| [02-ai-provider.md](02-ai-provider.md) | 多 provider 抽象、流式协议、重试与 overflow | `packages/ai` |
| [03-agent-loop.md](03-agent-loop.md) | **核心 agent 循环**(最重要)、Agent 类、工具执行 | `packages/agent` |
| [04-coding-agent.md](04-coding-agent.md) | 业务层架构、AgentSession、系统提示词 | `packages/coding-agent/src/core` |
| [05-tools.md](05-tools.md) | 内置 8 个工具的行为细节 | `packages/coding-agent/src/core/tools` |
| [06-session-compaction.md](06-session-compaction.md) | JSONL 会话树、分支、上下文重建、compaction | `packages/coding-agent/src/core` |
| [07-extensions.md](07-extensions.md) | 扩展系统:发现加载、事件管线、注册面 | `packages/coding-agent/src/core/extensions` |
| [08-modes-tui.md](08-modes-tui.md) | 四种运行模式、RPC 协议、TUI、周边包 | `packages/coding-agent/src/modes`、`packages/tui` 等 |
| [09-wiring-and-rust.md](09-wiring-and-rust.md) | **模块接线机制(依赖倒置点、调用链、状态所有权)与 Rust 实现映射** | 全仓 |
| [10-implementation-policy.md](10-implementation-policy.md) | **总体实现方针:crate 拆分规则、模块契约(工厂+trait+类型)、六接缝纪律、开发流程** | 全仓 |
| [12-testing-standard.md](12-testing-standard.md) | **测试标准:L0-L3 分层(L3 为 pexpect 真终端 + 本地 mock LLM 的 E2E)、改动→必补测试映射** | `tests/e2e`、`crates/rpi-tui` |

## 建议阅读顺序

1. **先读 00**(定位与哲学)→ **03**(agent 循环是整个系统的心跳)→ **01**(类型是 03 的词汇表)。
2. 实现顺序参考:**02 → 03 → 06 → 04/05 → 07 → 08**。先有 provider 流与循环就能跑通最小 CLI;会话持久化其次;TUI 与扩展最后。
3. **动手前先读 10(总体实现方针)**:workspace 骨架已建在仓库根(`crates/`),按其里程碑 M1-M6 逐模块填充;模块契约与接缝修改纪律以 10 为准。
4. 每篇文档末尾都有 **「源码文件索引」**:该模块全部相关源文件的路径、行数、职责、关键符号行号与阅读优先级(P0 必读 / P1 重要 / P2 按需),实现对应 Rust 模块时逐文件对照。

## 术语表

| 术语 | 英文 | 释义 |
|---|---|---|
| 转录 | transcript | 会话的消息序列,是系统的唯一真值来源 |
| 回合 | turn | 一次 assistant 响应 + 其全部工具调用与结果 |
| 运行 | run | 一次 prompt 从开始到 agent 结束的完整过程(含多个 turn) |
| 转向消息 | steering | agent 工作中途注入的用户消息,在当前 turn 结束后生效 |
| 跟进消息 | follow-up | agent 本应停止时才注入的用户消息 |
| 压缩 | compaction | 用 LLM 生成的摘要替换旧上下文,原 entry 保留在会话树中 |
| 条目 | entry | 会话 JSONL 文件中的一个节点(消息/压缩/设置变更等),构成树 |
| 分支 | branch | 会话树中从根到某个叶子的一条路径 |
| 系统消息 | system message | 转录中承载系统提示词与工具集声明的消息,可增量 patch |
| 截断防线 | length 防御 | `stopReason === "length"` 时拒绝执行该消息全部工具调用 |

## 一句话架构

> **会话是 append-only 的 JSONL 树;活动分支投影成模型上下文;系统提示词与工具集作为转录中可增量更新的系统消息状态;agent 循环流式驱动 LLM,并行执行工具;steering/follow-up 队列提供中途干预;扩展以进程内代码形态订阅约 40 种事件并注册工具/命令/provider;四种运行模式(interactive/print/json/rpc)只是同一业务核的不同 I/O 壳。**
