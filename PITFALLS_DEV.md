# 开发坑记录

本文件记录 latent 开发过程中实际踩过的代码级坑。每条：坑、证据、规避方式。

---

## DP-001: tokio 子进程收割依赖全局 SIGCHLD 唤醒链,必须有 try_wait 兜底

- **坑**: `tokio::process::Child::wait()` 的唤醒完全依赖 SIGCHLD → signal_hook 处理器写**进程唯一的全局 self-pipe** → 各 runtime 的 signal driver 各自 dup 该 pipe 注册进自己的 kqueue → 谁醒谁 `globals().broadcast()` → 全局 `watch` 通道唤醒 Reaper。这条链上任何一环丢事件,`wait()` 永不返回。
- **证据**: tokio 1.53.1 `src/runtime/io/driver/signal.rs` 注释自认不确定（*"I'm not sure if … there could be some race condition when consuming readiness events"*），且依赖"至少一个 dup 收到通知"。多 runtime 并存（cargo test 并行）时风险放大。
- **规避**: watcher 收割不用裸 `child.wait()`，而是 `loop { try_wait → timeout(500ms, child.wait()) }`——退出状态统一由 `try_wait` 取（wait 完成后 tokio FusedChild 已缓存状态），收割延迟上限 500ms。见 `bash.rs::watch_background`。

## DP-002: 通知抑制闸门必须放在会话感知层,测试替身会绕过它

- **坑**: 把"已消费任务不再投递完成通知"的检查放进 `AgentFollowUpNotifier::notify` 后，用 `CollectingNotifier` 测试替身的 kill 测试仍观察到通知——替身没有闸门。
- **规避**: 断言"会话层不投递"的测试必须走真 `AgentFollowUpNotifier` + 裸 agent + `ScriptedProvider`（断言 `provider.remaining()` 不减、`message_count` 为 0）。替身只用于断言通知文本内容。

## DP-003: fixture 内部自建共享对象,外置实例会与之脱节

- **坑**: kill 测试重写时先 `Arc::new(BackgroundTaskRegistry::new())` 再调 `fixture(notifier)`，而 fixture 内部又自建了 registry——bash 工具登记进 fixture 的 registry，测试断言查的是外置 registry，`get("bash-1")` 返回 None。
- **规避**: 共享状态（registry/notifier）要么全部由外层构造后注入，要么全部取 fixture 返回值；禁止混用。

## DP-004: 注入顺序重排时 take_steering_batch 只能调用一次

- **坑**: 重排 `collect_injectables` 注入顺序（steering → follow-up → prepared）时，曾把 `take_steering_batch()` 写成两处调用——同一个 steering 批被消费两次，第二条用户插话重复注入，且首轮未注入会在错误轮后继续循环。
- **规避**: 消费型 take 方法在一个调用点取、显式传给所有需要的位置；改动注入顺序后必须跑全量 steering 测试。

## DP-005: 通知注入点不能插进工具调用与工具结果之间

- **坑**: 工具结果在 `execute_tool_calls` 内直接进转录，异步通知若"按时间序"插入，可能落在 toolCall 与 toolResult 之间——破坏 provider 的消息配对协议。
- **规避**: 通知统一在轮边界注入，落点固定在最近的工具结果**之后**、下一个模型请求之前；这是有意取舍，不追求全局时间序。

## DP-006: 多点替换的 oldText 必须唯一

- **坑**: `agent.prompt("run it").await.unwrap();` 在测试文件中出现两次，edit 直接失败。
- **规避**: replace/edit 的锚点带上足够的上下文（函数体特征行）保证唯一；批量改用脚本时先 grep 计数。
