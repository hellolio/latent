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

## DP-007: 删 models.json 的键并收回空条目，会把自定义 provider 的模型一并抹掉

- **坑**: `/context` 的「默认」档 = 删除该模型的 `contextWindow` 键。写完马上收回「已无有效字段」的空条目/空 provider 看着干净，但自定义 provider（非内置）的模型**只能靠 models.json 注册**：条目一删，当前会话的模型在同一次 `resolve` 里就再也解析不出来——`/context 默认` 会以「写入成功但解析失败」收场，窗口也回不去。
- **证据**: CLI 单测（provider=`mock`）中 `/context default` 后窗口仍停在 256000；改用 `latent_ai::default_provider_endpoint(provider_id).is_some()` 只在内置 provider 上收回后，同一测试回落到 128000。gateway 端同类。
- **规避**: 收回空条目只对**内置 provider** 做；自定义 provider 的条目保留（`upsert_models_json_context_window` 里 `let builtin = …`）。写任何「删除配置键」的命令前，先问一句「删完之后这个对象还剩谁认识」。

## DP-008: `preferred_models_path` 提供项目目录就恒返回项目路径（与函数文档相反）

- **坑**: 函数文档写「项目 models.json 存在则写它，否则全局数据目录」，代码却是 `if path.exists() { return path } return path` —— 两支返回同一条路径（测试 `preferred_models_path_prefers_project_dir` 才是真契约：「存在与否一致，避免歧义」）。于是任何写盘型命令（/model 表单、$EDITOR 入口、/context）在 cwd 下会**凭空创建 `<cwd>/.latent/models.json`**；更糟的是 no-op 也走一遍 `save_models_file`，一次查询式操作便留下空文件 `{}`。
- **证据**: 一次 `/context` 测试把 `.latent/models.json`（内容 `{}`）写进了 `crates/latent-cli/`（cargo test 的 cwd），被 `git status` 当作未跟踪文件暴露。
- **规避**: ① 写盘函数先算 `changed`，无变化直接 `return Ok(())`（`upsert_models_json_context_window` 已如此）；② 测试里把 `state.cwd` 指到临时目录（见 TP-007）；③ 想写全局就显式传 `None` 给 `project_dir`，不要指望这个函数帮你兜底。

## DP-009: 补全弹窗的 variants 会顶掉裸命令，带 variants 的命令永远无法「裸执行」

- **坑**: `CommandPopup::filtered` 在查询首词精确命中带 variants 的命令时，把列表**整个换成变体子项**（原命令条目不在其中），于是 `is_exact_match()` 永远为假、回车被补全分支截走：敲 `/context` 回车只会补全+展开变体页，再回车则落到**第一个变体**（`/context 默认`）——「裸命令开选择器」的设计意图不可达，还多一个「回车即清空 contextWindow」的意外动作。
- **证据**: 单测 `bare_context_opens_selector_on_current_preset` 在把 `/context` 挂进 variants 时 `state.select` 恒为 `None`；去掉 variants 后正常开选择器（`/mode`、`/session` 是有意选择变体页的命令，裸执行无意义）。
- **规避**: 需要「裸命令 → 交互式选择器」的命令**不要**接 variants 机制；档位名放 runtime 常量表（`CONTEXT_PRESETS`），选择器从表派生。

## DP-010: `resolve("<provider>")`（裸 provider）走 `Model::minimal`，丢模型条目级字段

- **坑**: `ModelResolver::resolve` 对已有 spec（`provider/id`，或命中 `extra_models`）返回注册好的 Model，但裸 provider 会先用 `default_model_for`/首个 extra model 取 id，再 `Model::minimal(id, api, provider)` 重建——provider 级 `baseUrl/apiKey/headers` 会补上，**模型条目级的 `contextWindow`/`maxTokens` 却丢了**。表现为 `latent --provider e2e`（无 `--model`）启动后 footer 窗口是缺省 128k，而不是 models.json 里写的 32000。
- **证据**: e2e `test_context_command.py` 首版断言「选择器提示行显示 32000」：实测启动窗口 128000（❯ 停在 128k 行），而 `/context 默认` 按完整 spec 重解析后才是 32000。
- **规避**: 断言「启动窗口」的测试先确认自己走的是哪条解析路径（裸 provider vs `provider/id`）；要模型级字段生效就显式指定 `--model`/完整 spec。

## DP-011: 命令的 owner 门只在 `handle_command` 顶部判一次，分支里别再判

- **坑**: `/mode` 这类命令的权限靠顶部 `(is_group && command.owner_only_in_group()) || full_access_requested` 一处把守；给 `/context` 分支另加一段「私有 API 才拦」的 `is_owner` 检查（照 `/queue` 的写法抄）后，**私聊非 owner 也被拒**，与「群聊收紧、私聊放开」的设计相反。
- **证据**: gateway e2e `chat_commands_work`（发件人 `u1` 非 owner，DM）断言 `/context` 回执却拿到「该命令仅 owner 可用」。
- **规避**: 新命令只选一类：`owner_only_in_group()`（±群聊）**或** 分支内显式 owner 门（如 `/queue` 这种私聊也要 owner 的），不要两处都写。
