# 已知问题记录

仓库内已确认/已取证的问题档案。每条记录：现象、触发条件、证据、相关代码位置、修复方向。
状态为「未修复」的问题在修复后应回填验证结果。

---

## KB-001: 后台任务完成触发的"空唤醒"轮 —— 模型在无用户输入时被拉起新回合并产生归因幻觉

- **发现日期**: 2026-10-09
- **状态**: 未修复（已取证,竞态点待查）
- **影响面**: 所有经 `AgentSession` 装配的运行模式（TUI/print/json/rpc）;bash 后台任务与 web fetch 后台抓取共用同一唤醒链
- **复现环境**: 本会话实测,模型 glm-5.3-flash,连续三轮同型幻觉

### 现象

1. 助手输出完一份完整回复（会话以 assistant 消息结尾）后,**约 11ms 内自动发起了新一轮模型请求,请求上下文中没有任何用户消息**。
2. 模型对"为什么又轮到我发言"产生归因幻觉:思维链里臆断"用户把我刚输出的总结发了回来"（原文: *"The user's message is... interesting. It's written as if it were MY final summary…"*）,并顺着这个不存在的前提继续行动（回复"总结准确"、主动执行 `git diff` 自查等）。
3. 同一会话连续发生 **3 次**（无用户输入的 assistant 轮次）,每轮思维链都编造了不同的"用户消息前提"。
4. 真实的后台任务完成通知文本反而**晚了 1~2 轮**才作为 user 消息到达。
5. 用户视角:屏幕上"报告输出完突然自己动起来",思维链里出现把自己的输出当作用户消息来回应的内容——用户（正确地）确信自己没有发送过任何消息,造成强烈困惑。

### 证据（会话文件取证,2026-10-09）

会话文件: `~/.config/latent/sessions/Users-kin-Documents-10source-latent/20261009-202943__01a1206d-187a-74c6-bb2a-fd7f714a93b9.jsonl`（取证时共 515 条,行号为取证时点）

| 行号 | 角色 | 内容 |
|---|---|---|
| 471 | assistant | 完整收尾报告,开头即"全部完成,收尾验证与总结如下。"（该句全程**不存在**任何含它的 user 条目） |
| 473 | context_ref | 指向请求快照 `112_01a120d17fd57298ba8c0ea2136b0c34.json` |
| 474 | assistant | thinking 臆断"用户发回了我总结" → 输出"总结准确…"（**471 与 474 之间无任何 user 条目**） |
| 487/488 | user | bash-1 失败 / bash-4 完成通知（**迟到 1 轮**才到达） |
| 493 | assistant | 又一轮空唤醒,thinking 臆断"The user is confirming…" |
| 495/502 | user | bash-3 / bash-2 通知（同样迟到） |
| 500 | assistant | 又一轮空唤醒,thinking 臆断"The user's message contains a duplicate notification…" |

请求快照 `…01a1206d….ctx/112_01a120d17fd57298ba8c0ea2136b0c34.json`（该轮实际发给模型的消息列表,共 247 条）:**最后一条是 assistant 的完整报告**（以"建议提交信息"结尾）,其后无任何 user 消息——证明该轮请求在"无新输入"下发起。时间线: 报告落盘 → +11ms 发起新请求 → +16s 生成幻觉回复。

### 触发条件

1. 存在后台任务通知源: bash 命令自动转后台（生效超时 > 阈值,默认 60s）或 web fetch `includeContent` 后台抓取;
2. 通知到达时 agent 恰好空闲（`wait_idle()` 立即通过）;
3. `follow_up(text)` 入队与 `continue_run()` 拉起的新 run **首轮请求未带上该 follow_up 文本**（唤醒与载荷脱节,竞态点见"待查"）;
4. 模型侧: 上下文"以 assistant 结尾却被要求续写"时,倾向编造用户消息前提（glm-5.3-flash 实测三次全中;其他模型未验证）。

### 相关代码位置

| 位置 | 说明 |
|---|---|
| `crates/latent-tools/src/bash.rs:34` | `trait BackgroundNotifier` 接缝定义 |
| `crates/latent-runtime/src/assembly.rs:1152` | 通知器注册进 bash 工具 |
| `crates/latent-runtime/src/assembly.rs:918-929` | `AgentFollowUpNotifier`（web fetch）: `wait_idle().await → agent.follow_up(text) → agent.continue_run().await` |
| `crates/latent-runtime/src/assembly.rs:931-948` | `ShellBackgroundNotifier`（bash 后台）: 同一唤醒链,逐字相同 |
| `crates/latent-agent/src/agent.rs:311` | `Agent::follow_up` —— 消息推入 mpsc 注入通道 |
| `crates/latent-core/src/session.rs:622-641` | `AgentSession::continue_run` —— 注释里已记载"忙/闲竞态窗口会把消息滞留到下一条消息才被吞入"的同类历史坑位 |
| `crates/latent-agent/src/loop_.rs` `collect_injectables` | 循环侧 follow-up 消费（按 QueueMode,OneAtATime 每轮一条） |

### 待查（未定论的竞态点）

通知文本入队晚于首轮请求的注入收集,具体竞态窗口在以下候选中（未逐一验证）:

- a) `notify` 的 `wait_idle()` 在 run 未完全收尾/落盘时即通过,`follow_up` 入队晚于 `continue_run` 的注入收集;
- b) 多个通知源并发 spawn（本次 bash-1 与 bash-4 几乎同时完成）,`wait_idle → follow_up → continue_run` 三步交错,部分载荷落在首轮注入收集之后;
- c) follow-up 的 QueueMode 节律（OneAtATime 每轮一条）与多次唤醒叠加后的消费顺序。

### 危害

- 空唤醒轮里模型以幻觉前提行动:产生用户不可见意图驱动的回合（本例中主动执行 `git diff` 并输出无主回复）,污染转录与上下文;
- 用户视角极度困惑（本次直接导致"是你想错了还是代码 bug"的排查）;
- 幻觉轮与迟到通知交错到达,进一步加剧上下文混乱。

### 修复方向（候选,未实施）

1. **消灭空唤醒**: `continue_run` 前断言队列深度 > 0,否则不拉起新轮（推荐,最小改动）;
2. 载荷先行: `notify` 确保 `follow_up(text)` 入队完成（且循环可见）后再触发续跑;
3. 兜底提示: 框架检测"以 assistant 结尾且无新输入"的续跑,注入占位 system reminder,禁止模型编造用户消息;
4. 模型维度: 此行为在 glm-5.3-flash 上稳定复现,其他模型表现待验证。
