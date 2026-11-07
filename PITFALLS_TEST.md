# 测试坑记录

本文件记录 latent 测试编写/调试中实际踩过的坑。每条：坑、证据、规避方式。

---

## TEST-001: 异步事件与脚本化回合的时序撞车会产生"两种结局"

- **坑**: midrun 通知测试里，任务 `sleep 2` 的自然退出与 turn2 的 `sleep 1.2, timeout:1` 超时杀灭撞在同一 ~50ms 窗口：通知先入队 → 边界注入（PASS）；turn2 先 settle → 通知触发 `continue_run` 补投递，脚本 turn3 已被消费 → 通知落在其后 + 脚本耗尽多出空轮（FAIL，断言 4≠3）。约 30% 不稳定，且表象（"通知没出现"/"多一轮"）极易误导为 watcher 挂死。
- **证据**: 加 SIGCHLD 诊断任务实测：SIGCHLD 到达后 ~100µs 即完成 reap，watcher 从未挂死；失败运行的转录序为 `assistant:noticed completion → user:通知 → assistant:""`。
- **规避**: 涉及"事件发生在某个工具执行期间"的断言，把事件时刻**严格放进阻塞工具的执行窗口内部**，两侧余量 >400ms（如任务 `sleep 1.5`、工具 `sleep 1.5` 无 timeout：事件在 ~1.55s，窗口 ~1.1–2.6s）。修复后 10/10 稳定。

## TEST-002: 诊断异步丢失要区分"广播未发生"与"通道丢唤醒"

- **坑**: 只在 watcher 内部打点，无法区分 SIGCHLD 广播根本没发生（驱动层）还是广播了但 Reaper 没醒（通道层），容易往错误方向修。
- **规避**: 在被测 watcher 旁 spawn 一个独立的 `signal(SignalKind::child())` 诊断任务打时间戳，并把测试观察窗口延长（+2.5s）后再断言。两个信号源同时缺失 = 驱动层；只有目标通道缺失 = 通道层。

## TEST-003: 测试环境变量会假失败——cargo test 前剥离 LATENT_*

- **坑**: 本会话跑在 latent 会话内，进程 env 带有宿主注入的 `LATENT_MODEL`/`LATENT_SESSION_ID` 等；`bash_tool_receives_pi_session_env_via_build_session` 验证"LATENT_* 注入不覆盖已有变量"，期望值 `test-model` 被会话真实值顶掉，必现失败（断言消息："LATENT_MODEL 应被注入: glm-5.3-flash 01a12445-…"）。用户裸 shell 环境干净，同一命令全部通过——"我这里失败、你那里通过"先查环境差异。
- **证据**: `env | grep "^LATENT"` 与断言消息中的值逐字吻合（model + session id）；`env -u LATENT_MODEL -u LATENT_SESSION_ID … cargo test` 剥离后 modes 20/20、全量 1037/0 全绿。
- **规避**: 统一用 `env $(env | grep -E "^LATENT_" | cut -d= -f1 | sed 's/^/-u /') cargo test …` 剥离后运行。**注意：git stash 对照对环境型假失败无效**——stash 只还原代码、环境变量原样保留，干净 HEAD 上照样失败，会误判成"存量失败"；环境疑点用 `env -u` 做对照实验，而不是代码二分。

## TEST-004: 本会话长命令会被自身的旧版 latent 转后台

- **坑**: 会话内的 latent 二进制阈值是旧版（60s），>60s 的 cargo test 命令会被转后台并迟到回传，表现为"命令提前返回"。
- **规避**: 长命令用 `nohup … > /tmp/xxx.log 2>&1 &` 后台化 + 轮询日志/退出码。

## TEST-005: e2e 断言语义随通知语义改变而反转

- **坑**: pull-only 化后"模型已取结果 → 通知不出现"不再成立——通知（纯指引）**必然**出现一次；`expect_absent(r"\[latent\] background task")` 会扫全转录，必然失败。
- **规避**: 断言改为"通知出现 + 任务输出只能经 result 获取后可见"（转后台结算与通知文本都不得含任务输出；`late-marker` 在 result 调用后才出现）。`expect_absent` 只用于真正不应出现的场景（kill 即消费后无完成通知）。

## TEST-006: 并行测试共享进程级全局状态——信号/子进程类测试易受邻居干扰

- **坑**: cargo test 默认并行，每个 `#[tokio::test]` 一个 runtime，进程级全局（SIGCHLD 处理器、self-pipe、orphan 队列）被所有 runtime 共享；单测时序问题可能只在全量运行时暴露（或反之）。
- **规避**: 时间敏感的异步测试先单独连跑（≥10 次）验证，再进全量；全量失败时先用 `--nocapture` + 打点区分邻居干扰与自身竞态。

## TEST-007: 写盘型单测必须先隔离 `state.cwd`，否则在仓库里造出文件

- **坑**: `handle_key` 驱动的斜杠命令测试用 `test_state()`（`cwd = std::env::current_dir()`，即 crate 目录；`home` = 真实 HOME）。只要命令会写 models.json，测试就会在 `crates/latent-cli/.latent/` 或用户真实配置目录里落盘：一次早期版本的 `/context` 测试就留下了 `crates/latent-cli/.latent/models.json`（内容 `{}`），被 git status 当场揭发。事发测试当时恒免（断言选择器不开），只是碰巧没写成功——**“现在没写”不等于“将来不会写”**。
- **规避**: 任何可能触到 `preferred_models_path`/`latent_dir` 的 CLI 单测，先隔离写盘根（`state.cwd` 仅剩 `.latentignore`/检索类路径还在用；全局配置见 TEST-010 的 `LatentHome`），并在测试内断言实际写入路径。另：`latent_dir()` 读 `LATENT_HOME`，跑单测前按 TEST-003 剔离 `LATENT_*`。

## TEST-008: e2e 断言 footer 内容要开宽终端，窄屏右侧段会被左侧挤掉

- **坑**: `footer` 是一行左右对齐，左侧是 cwd/模式段。在默认 `cols=100` 下，临时目录长路径占满行宽，右侧用量段被截成 `ctx 0% (`——断言 `(…/256k)` 必然超时，而屏幕上“确实有值”。
- **证据**: `test_context_command.py` 首版在 cols=100 下报“当前屏幕上未出现 `/\s*256k\s*\)`”，失败输出里就是 `ctx 0% (` 被截断。
- **规避**: 断言 footer 右段的场景显式 `LatentApp(..., cols=160)`（并把理由写进注释）；或用 `expect_text` 扫全转录（但注意它不区分历史与当前屏，见 TEST-009）。

## TEST-009: 选择器/弹窗断言用 `expect_visible`，`expect_text` 会扫全转录而误命中

- **坑**: `expect_text` 在**全量输出**里搜正则，选择器一显一隐后仍能匹配到早已消失的文本；用它断言“四档候选”时，`"256k"` 可能只是命中了上方转录里的 `context → 256000 tokens` 回执，等于什么都没验证。
- **规避**: 断言“当前屏幕上有什么”用 `expect_visible(pattern)`（轮询 `visible_text`）；选中行标记也可断言——`_squeeze` 去掉空白后 `❯ 128k` 就是 `❯128k`（`expect_visible(r"❯\s*128k")`）。`expect_text` 留给“历史上出现过”的内容（如带耗时的用量行）。

## TEST-010: 写全局配置的交互单测必须串行隔离 `LATENT_HOME`（进程级环境变量竞态）

- **坑**: handlers 走 `latent_dir(state.home)` → 先读进程级 `LATENT_HOME`。cargo test 并行下，两个各自 set/remove `LATENT_HOME` 的 RAII 测试互相覆盖：A set 后 B set，A Drop 又 remove——B 在中途拿到 `None`，`preferred_models_path` 返回 `None`，测试随机报「无法定位全局 models.json」。仅靠每个测试自建自删的守卫**不够**，这是典型低概率竞态（见 TEST-006）。
- **证据**: `context_command_writes_models_json_and_updates_window` 与 `model_form_adds_new_provider_and_switches` 同时写全局 models.json。
- **规避**: `tests.rs` 的 `LatentHome` 守卫内部持 `static LATENT_HOME_LOCK: Mutex<()>`，整个测试期间持锁串行；守卫跨 `.await` 持锁需 `#[allow(clippy::await_holding_lock)]`（`#[tokio::test]` 默认 current_thread runtime，无 Send 约束，合法）；断言写盘路径用 `home.dir.join("models.json")`。

## TEST-011: E2E 请求体断言 user 消息要兼容字符串与 blocks 两种 wire 形态

- **坑**: 断言恢复请求的末条 user 消息时假设 `content == "继续"`（纯文本消息经 anthropic 适配器序列化为字符串），但 TUI 交互路径的用户输入走 `UserContent::Blocks` → content 是块数组，且末块带 `cache_control`（缓存断点附加在最后一条 user/system 消息上）。同一份请求体里两种形态并存，单一形态断言随机性失败。
- **证据**: `test_tool_budget_pause.py` 首版失败：`{'content': [{'cache_control': {'type': 'ephemeral'}, 'text': '继续', 'type': 'text'}], 'role': 'user'} != '继续'`；同请求前一条 tool_result 消息恒为 blocks（`tool_result` 只能进块数组）。
- **规避**: 断言消息文本先归一化——content 为字符串直接用，为数组则拼接各块的 `text`（工具内 `_msg_text` helper）；结构性断言（role、块类型）与文本断言分离。tool_result 消息可直接遍历 `block["type"] == "tool_result"`。

## TEST-012: E2E 断言 pattern 不得与 UI chrome 文本（命令描述/help）撞车——弹窗先于 run 出现

- **坑**: 新增 `/init` 的 E2E 用 `expect_text(r"新会话生效")` 等最终回复，而命令注册描述写成「…创建/更新项目根 AGENTS.md（新会话生效）」——`/init` 输入时补全弹窗就渲染了描述文本，`expect_text` 扫全量输出立即命中，断言在 run 尚未开始（mock 请求数为 0、文件未写盘）时通过，表现为“回复出现了但产物不存在”的假阳性，且加 `wait_for_requests` 前置会“修好”它（恰好等到了 run 结束），极具迷惑性。
- **证据**: 失败现场 `NREQ: 0`；转录末尾只有弹窗帧与编辑器 `/init`，无任何 run 输出。
- **规避**: E2E 断言文本必须只可能来自模型回复/工具结果：命令 `description`、`/help` 文案、footer 等固定 UI 文本不得包含断言 pattern（命令描述避免复用回复措辞）；产物类断言（文件落盘）用“请求体反向断言”兜底（`wait_for_requests` 确认轮次后断言请求内容与落盘结果一致）。
