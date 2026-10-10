# 测试坑记录

本文件记录 latent 测试编写/调试中实际踩过的坑。每条：坑、证据、规避方式。

---

## TP-001: 异步事件与脚本化回合的时序撞车会产生"两种结局"

- **坑**: midrun 通知测试里，任务 `sleep 2` 的自然退出与 turn2 的 `sleep 1.2, timeout:1` 超时杀灭撞在同一 ~50ms 窗口：通知先入队 → 边界注入（PASS）；turn2 先 settle → 通知触发 `continue_run` 补投递，脚本 turn3 已被消费 → 通知落在其后 + 脚本耗尽多出空轮（FAIL，断言 4≠3）。约 30% 不稳定，且表象（"通知没出现"/"多一轮"）极易误导为 watcher 挂死。
- **证据**: 加 SIGCHLD 诊断任务实测：SIGCHLD 到达后 ~100µs 即完成 reap，watcher 从未挂死；失败运行的转录序为 `assistant:noticed completion → user:通知 → assistant:""`。
- **规避**: 涉及"事件发生在某个工具执行期间"的断言，把事件时刻**严格放进阻塞工具的执行窗口内部**，两侧余量 >400ms（如任务 `sleep 1.5`、工具 `sleep 1.5` 无 timeout：事件在 ~1.55s，窗口 ~1.1–2.6s）。修复后 10/10 稳定。

## TP-002: 诊断异步丢失要区分"广播未发生"与"通道丢唤醒"

- **坑**: 只在 watcher 内部打点，无法区分 SIGCHLD 广播根本没发生（驱动层）还是广播了但 Reaper 没醒（通道层），容易往错误方向修。
- **规避**: 在被测 watcher 旁 spawn 一个独立的 `signal(SignalKind::child())` 诊断任务打时间戳，并把测试观察窗口延长（+2.5s）后再断言。两个信号源同时缺失 = 驱动层；只有目标通道缺失 = 通道层。

## TP-003: 测试环境变量会假失败——cargo test 前剥离 LATENT_*

- **坑**: 本会话自身的 shell 带有 LATENT_* 环境变量，`bash_tool_receives_pi_session_env_via_build_session` 断言"不覆盖已有变量"时会拿到会话值而失败。
- **规避**: 统一用 `env $(env | grep -E "^LATENT_" | cut -d= -f1 | sed 's/^/-u /') cargo test …` 剥离后运行。

## TP-004: 本会话长命令会被自身的旧版 latent 转后台

- **坑**: 会话内的 latent 二进制阈值是旧版（60s），>60s 的 cargo test 命令会被转后台并迟到回传，表现为"命令提前返回"。
- **规避**: 长命令用 `nohup … > /tmp/xxx.log 2>&1 &` 后台化 + 轮询日志/退出码。

## TP-005: e2e 断言语义随通知语义改变而反转

- **坑**: pull-only 化后"模型已取结果 → 通知不出现"不再成立——通知（纯指引）**必然**出现一次；`expect_absent(r"\[latent\] background task")` 会扫全转录，必然失败。
- **规避**: 断言改为"通知出现 + 任务输出只能经 result 获取后可见"（转后台结算与通知文本都不得含任务输出；`late-marker` 在 result 调用后才出现）。`expect_absent` 只用于真正不应出现的场景（kill 即消费后无完成通知）。

## TP-006: 并行测试共享进程级全局状态——信号/子进程类测试易受邻居干扰

- **坑**: cargo test 默认并行，每个 `#[tokio::test]` 一个 runtime，进程级全局（SIGCHLD 处理器、self-pipe、orphan 队列）被所有 runtime 共享；单测时序问题可能只在全量运行时暴露（或反之）。
- **规避**: 时间敏感的异步测试先单独连跑（≥10 次）验证，再进全量；全量失败时先用 `--nocapture` + 打点区分邻居干扰与自身竞态。
