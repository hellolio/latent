# rpi 代码评审与 E2E 测试报告（2026-09-27）

评审方式：两个并行深读（agent 循环/AI 层；工具/会话/TUI/CLI 层）+ 关键发现逐条人工核验（读源码证实）+ 按 `tests/e2e/README.md` 方法的真终端 E2E 验证。参考系为 pi（TypeScript 原版）的语义（docs/00-12 为蓝本）。

结果：**P1 × 9 全部修复，P2 × 14 修复、6 项记录不改**，Rust 侧 423 个测试全绿，E2E 29/29 通过。

---

## 一、P1（已全部修复）

| # | 问题 | 位置 | 修复 | 验证 |
|---|---|---|---|---|
| 1 | JSONL 崩溃恢复不补半行换行：恢复后第一条 append 与半行拼成一行，新 entry 静默丢失 | `rpi-session/src/manager.rs` load_file/append_entry | `repair_missing_trailing_newline`：load 后检测末尾缺 `\n` 则补齐 | Rust 单测 `crash_half_line_is_isolated_and_next_append_survives` + E2E T20 |
| 2 | HTTP 529 与 Retry-After 完全未处理：529 空 body 被判不可重试直接终止；限流退避不尊重服务端节奏 | `rpi-ai/src/retry.rs`、`adapters/mod.rs` | 529 入可重试表；`http_error_message` 写入 `(retry-after: Ns)` 标记，重试层优先采用 | Rust 单测 `retry_after_marker_is_parsed_from_error_message`、`retry_after_header_takes_precedence_over_backoff` |
| 3 | 并行工具执行时 thunk 并发调用 `Subscriber::on_event`，违反 event.rs 串行保序契约 | `rpi-agent/src/loop_.rs` execute_batch_parallel | 新增 `QueuedSubscriber`：thunk 事件入 mpsc，主任务统一串行 await | cargo test（loop 不变量 I1-I6 全过） |
| 4 | Windows bash 超时/中止路径挂死：非 Unix `kill_process_tree` 是 no-op，子进程不退则 `wait()` 永不返回 | `rpi-tools/src/bash.rs` | 非 Unix 分支先 `start_kill()` | 代码审查（macOS 不受影响） |
| 5 | `should_compact` 是死代码：阈值式自动压缩从未接线，长会话只能事后 overflow 恢复 | `rpi-core/src/session.rs`、`rpi-cli/src/assembly.rs` | `ContextCompactor` 增加 `should_auto_compact`（core 不依赖 rpi-session，装配方实现）；run 成功结束后阈值触发压缩；TUI footer 的 auto 标记接通 | Rust 单测 `auto_compact_triggers_at_threshold_after_run` |
| 6 | rpc 模式 `Bash` 命令内联阻塞 stdin 命令循环：一条 `sleep 600` 让 abort/get_state 全部无响应 | `rpi-cli/src/modes/rpc.rs` | 仿 `Prompt` spawn 化 | 编译 + 既有 rpc 测试 |
| 7 | read 工具字节截断提示误导：多行命中字节限也报"单行超限"，且 `tail -c +N` 偏移未计入 offset 跳过的字节 | `rpi-tools/src/read.rs` | 按 `output_lines == 1` 区分单行/多行两种文案；偏移计入 `skipped_bytes` | Rust 单测 `multiline_byte_truncation_hints_offset_not_bash` |
| 8 | **（E2E 新发现）`/quit` 完全失效**：`execute_command` 返回的退出信号被 `submit_input` 丢弃，事件循环永远收不到 true；Ctrl+D 正常所以此前未暴露 | `rpi-cli/src/modes/interactive/handlers.rs` | `submit_input` 改返回 bool 逐层上抛 | Rust 单测 `slash_quit_via_enter_returns_quit_signal` + E2E T9 |
| 9 | **（E2E 新发现）空对话摘要请求**：短会话切点落在首个 user 消息前，待摘要范围只有 system 元数据，`serialize_conversation` 跳过 System → LLM 在看不到任何对话的情况下编造"摘要"入库 | `rpi-session/src/compaction.rs` run_compaction | 序列化为空时返回 `Ok(None)`：不发摘要请求、不产生 Compaction entry | Rust 单测 `run_compaction_skips_empty_conversation_range` + E2E T19 |

## 二、P2（已修复 14 项）

| 问题 | 位置 |
|---|---|
| `trim_oldest_messages` 恰好多出 1 条时会删掉打头 system baseline | `rpi-agent/src/agent.rs`（单测 `trim_oldest_never_removes_system_baseline`） |
| `follow_up_mode` 死配置，实际跟随 steering_mode | `loop_.rs` LoopConfig/LoopState 独立传递并消费 |
| `max_tokens=0` 产出非法请求体 | 两个 adapter 统一走 `effective_max_tokens`（0 视为未指定） |
| 流中错误丢弃已流出内容 | 两个 adapter 错误终态改用 `error_with_partial` 保留内容 |
| edit 混合行尾：未编辑区域的裸 LF 被改写成 CRLF | 改为直接在原始内容上替换（old/new 按文件行尾转换），单测 `mixed_line_endings_unedited_lf_lines_are_untouched` |
| truncate 恰好等于 max_bytes 误报截断（末行多算 +1 换行） | `truncate.rs` head 路径（单测 `content_exactly_at_byte_limit_is_not_truncated`） |
| read `limit:0` 返回空内容而非报错；无二进制文件检测 | 参数校验 + NUL 字节检测（同 grep 判据） |
| Ctrl+C 双击窗口未被 Enter/Ctrl+O 重置 | `handlers.rs` |
| `session_name()` 跳过 `name: None` 导致"清除会话名"失效、旧名复活 | `manager.rs`（find_map → find + Some(name)） |
| `set_model`/`set_thinking_level` spawn 异步落盘与消息 append 乱序 | 改为内联 await（调用方同步改 async） |
| apiKey 的 env 变量存在但为空时把变量名当 key 发给 provider | `config.rs`：空值视为未配置 |
| `/compact` 带参数仅暗淡提示，用户误以为自定义指令生效 | 改为显式警告"本次压缩将使用默认模板" |
| LineDecoder 逐行 `drain(..=pos)` 前缀搬移 O(n²) | `sse.rs` 改扫描偏移一次定位全部行边界 |
| 流式预览每帧对全量文本重折行 O(n) | `view.rs` 只折尾部窗口（约 3 屏余量） |

## 三、记录不改（含理由）

- **MCP UI 断连 confirm fail-open 返回 true**：docs/07 §8.5 已声明为决策；安全敏感场景建议后续做成可配置。
- **bash PID 复用理论窗口**（kill 前 `child.id()` 可能命中复用进程组）：窗口极小，先 `try_wait()` 会引入竞态复杂度，收益低。
- **JSONL 无跨进程文件锁**：两进程同写一个 session 文件会分叉 leaf 链；pi 同样无锁，属使用约束。
- **grep 同步 IO 阻塞 tokio worker**：大仓库遍历会卡；取消按 entry 粒度。需要 `spawn_blocking` 改造，建议独立任务。
- **续聊回放对全部历史做 markdown+syntect 高亮**：超长会话启动卡顿；需渲染缓存/懒高亮，建议独立任务。
- **未知 StopReason 一律映射 Error 硬终止**：良性未知值（如 `eos` 变体）会中断整轮；涉及产品语义（如何呈现），本轮不动。

另记录一个轻微显示观察：折叠工具卡片在 scrollback 中的增量提交对 5~N 行输出可能缺 "+N lines" 提示与下边框（Ctrl+O 展开态渲染完整）；不影响正确性。

## 四、已核验无问题的项（评审确认）

SSE 跨 chunk 解析与多字节 UTF-8 安全、delta 拼接顺序（Anthropic index_map / OpenAI stream_index）、input_json_delta 分片拼接与修复解析、[DONE]/message_stop 处理、重试帧缓冲（提交点前无重复 delta，有集成测试钉住）、abort 各 phase 响应性、Phase 状态机无死锁路径、工具结果槽位配对不变量、树投影/环检测/坏 parent 兜底、compaction 切点不落 toolResult、进程组杀灭（Unix）、PI_* 环境注入不覆盖用户变量、锁持有范围（锁内不跨 await）、models.json 合并语义（含测试）。

## 五、测试基线修正

`test_output_display.py` 的 2 个断言在 TUI 布局调整后过期（`[tokens]` 文本已改为标题盒子渲染；CJK 测试的首个背景色定位命中编辑器），已按新渲染修正——确认是测试过期而非产品回归（转录中 CJK 无空格注入、用量行正常）。

---

# E2E 测试报告

运行方式（tests/e2e/README.md）：真实 PTY + mock LLM（anthropic-messages SSE）+ pyte 屏幕断言。

```bash
cargo build --bin rpi
pip install -r tests/e2e/requirements.txt
pytest -v          # 在 tests/e2e/ 下
```

**最终结果：29 passed / 0 failed（73.96s）；Rust 侧 cargo test --workspace 423 passed / 0 failed。**

| # | 文件 | 覆盖 |
|---|---|---|
| 基线 | test_startup.py | 横幅/footer/零 LLM 请求 |
| 基线 | test_ask_and_reply.py | 流式问答 + 请求反向断言 |
| 基线 | test_tool_roundtrip.py | read 工具往返 |
| 基线 | test_abort.py / test_ctrl_c.py / test_quit.py | Esc/Ctrl+C 中断与双击退出/Ctrl+D 退出码 |
| 基线 | test_output_display.py | 用量行/cache%/CJK 无空格注入 |
| 基线 | test_theme.py | /theme 列表/切换/未知主题/--theme |
| T9 | test_slash_commands.py | /help /session /thinking /model、未知命令本地警告、零 LLM 请求、/quit 退出码 0（钉住 P1-8） |
| T10 | test_steering.py | run 中输入转 steering：第二轮请求含首问+注入且顺序正确 |
| T11 | test_continue.py | --continue：历史回放、续聊请求携带完整历史 |
| T12 | test_abort_then_continue.py | Esc 中断后同一会话可继续 |
| T13 | test_provider_error.py | 500 错误自动重试（4 次请求）后错误上屏、会话仍可用 |
| T14 | test_tool_validation.py | 非法 arguments → 错误 toolResult 以 user 消息回传 |
| T15 | test_parallel_tools.py | 一轮两个 read：结果齐全且按源序进入第二轮 |
| T16 | test_bash_tool.py | stdout/退出码；5000 行截断：卡片折叠提示 + "Showing lines" 进 toolResult |
| T17 | test_write_edit_tools.py | write/edit 真实落盘 + toolResult 回传 |
| T18 | test_ctrl_o.py | Ctrl+O 折叠/展开/再折叠（尾部行可见性三态断言） |
| T19 | test_compact.py | 短会话 /compact 无空对话摘要请求（钉住 P1-9）；带参数警告；压缩后可续聊 |
| T20 | test_session_half_line.py | 构造半行损坏 JSONL + --continue：半行隔离、历史可见、新消息正常追加（钉住 P1-1） |

harness 扩展：`RpiApp(home=..., workdir=...)` 支持跨实例复用隔离环境（--continue/半行恢复测试）；models.json 每次启动重写（mock 端口随机）。

未纳入本轮（与评审确认一致）：print/json/rpc 三种模式的 e2e（Rust 侧 modes.rs 已覆盖）；模型选择器键盘交互、`!` 透传命令可作后续补充。
