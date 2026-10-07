# AGENTS.md

本仓库是 [earendil-works/pi](https://github.com/earendil-works/pi) 的 Rust 重写：一个终端 AI 编码 agent——流式驱动 LLM、并行执行工具、append-only 会话树、进程外 MCP 扩展、权限/沙箱护栏、子代理与技能系统；四种运行模式（print / interactive TUI / json / rpc）共享同一业务核 `AgentSession`。

## 常用命令

```bash
cargo build --release -p latent          # 构建可执行文件（bin 名为 latent）
cargo run -p latent -- --mock "你好"      # 无需 API key，mock provider 走通全链路
cargo test --workspace                    # Rust 全部测试（硬门槛：全绿）
cargo clippy --workspace --all-targets    # 硬门槛：零警告
cargo build --bin latent && cd tests/e2e && pytest -v   # E2E 测试（见下文测试方针）
cargo build -p latent-gateway             # 构建聊天网关 daemon（bin: latent-gateway；渠道 feature 转发）
cargo test -p latent-gateway --test e2e --test control   # 网关引擎/控制面集成测试
cargo test -p latent-channel --features qq,wecom,telegram # 渠道集成测试（假平台服务器，全离线）
```

API key 从环境变量读取（`ANTHROPIC_API_KEY`、`OPENAI_API_KEY`、`ZAI_API_KEY` 等）；`--provider <id> [--model <id>]` 指定模型，`provider/model` spec 经 `latent-core/src/model.rs` 的 `ModelResolver` 解析。

## 目录索引

### 依赖方向（严格单向，禁止反向/同层依赖）

```
L4  latent-cli ──── latent-tui ┐
L3  latent-core ────────────┤  依赖下方全部
L2  latent-agent ─ latent-session ─ latent-tools ─ latent-web ─ latent-sandbox ─ latent-runtime ┘
L1  latent-ai ─────────────────────────────────────────────────────────────────┘  零内部依赖

聊天栈（GATEWAY_PLAN §1，与上面同仓并存）:
L3  latent-gateway   # 引擎 + bin：仅依赖 latent-runtime + latent-channel
L1  latent-channel   # 纯聊天域：零内部依赖（不 import 任何 latent-* crate）
```

latent-session / latent-tools / latent-tui / latent-web 为可选组件：移除任意一个，其余 crate 仍须零警告编译；可选能力一律经 trait 在装配期注入（装配点在 `crates/latent-runtime/src/assembly.rs`，latent-cli 侧 re-export 保持 `latent::assembly::…` 路径兼容）。latent-cli **永不依赖** latent-channel / latent-gateway；渠道 crate 之间互不可见；渠道适配器在 latent-channel 内部按 feature 门控（`mock`/`telegram`/`qq`/`wecom`）。编译隔离验收：

```bash
test -z "$(cargo tree -p latent-channel | grep -E 'latent-(core|ai|agent|session|tools|web|sandbox|tui|runtime|gateway)')"  # channel 不认识任何业务 crate
test -z "$(cargo tree -p latent          | grep -E 'latent-(channel|gateway)')"                                            # CLI 不含聊天栈
test -z "$(cargo tree -p latent-gateway  | grep 'latent-tui')"                                                             # gateway 不含 TUI
```

### 根目录

| 路径 | 说明 |
|---|---|
| `Cargo.toml` | workspace 定义（12 个 crate；`default-members = ["crates/latent-cli"]`，裸 `cargo build` 只编 CLI）+ `[workspace.dependencies]`（外部依赖统一 pin 在此）+ `unsafe_code = "forbid"` lint |
| `README.md` | 用户向总览（特性、四模式、provider 表、工具表） |
| `GATEWAY_PLAN.md` | latent-gateway 开发计划（OpenClaw 架构级复刻，自包含实施指南 + 偏离记录 §8） |
| `crates/` | 全部源码，逐 crate 索引见下 |
| `tests/e2e/` | Python E2E 测试（真 PTY + 本地 mock LLM），逐文件索引见"测试方针" |
| `.latent/` | 项目级配置目录（settings.json / models.json / skills / agents / system-prompt.md） |
| `target/` | 构建产物（勿改） |

### crates/latent-ai — L1 基座：LLM 接入（零内部依赖）

| 文件 | 说明 |
|---|---|
| `src/types.rs` | LLM 世界纯类型：`ContentBlock`/`Message`/`AssistantMessage`/`StopReason`/`Usage`/`Model`（分层定价）、统一流协议 `AssistantMessageEvent`（start/delta/end/done/error）、`StreamOptions`（on_payload/on_response 观察回调） |
| `src/provider.rs` | 接缝 #1：`trait Provider::stream`（返回事件流，不返回 Result）+ `ProviderRegistry` + 11 家内置 provider 表（id/api/baseUrl） |
| `src/retry.rs` | 流式重试装饰器：指数退避封顶 60s；**提交点（首个内容 delta）之前失败静默整轮重试，之后直通**；aborted/quota 永不重试；`(retry-after: Ns)` 标记优先于退避 |
| `src/overflow.rs` | context overflow 检测：20+ provider 错误文案正则 + z.ai 式静默溢出（usage>窗口）+ 小米式 Length 假截断；上下文口径 = input + cache_read + cache_write |
| `src/transcript.rs` | 转录折叠：`normalize_context` 把系统提示词/工具声明折叠进首条 system 消息（转录即真相）；`get_current_tools` 按 tools_added/removed 重放工具集 |
| `src/env_keys.rs` | 17 家 provider 的 env key 白名单与凭据解析三级规则（显式 > env > 自定义 provider 允许无 key） |
| `src/json_parse.rs` | 流式 JSON 尽力解析：`repair_json` 状态机修复 + `complete_partial_json` 补全未闭合容器，全败返回空对象永不 panic |
| `src/sse.rs` | SSE / 按行解码器（跨 chunk 多字节安全、扫描偏移避免 O(n²)） |
| `src/mock.rs` | 测试用 `MockProvider`（文本分块流式）与 `ScriptedProvider`（按 turn 出脚本，供集成测试） |
| `src/adapters/mod.rs` | 适配器共享设施：HTTP 错误统一文案、cancel 竞速读块、观察回调 panic 吞掉、thinking level 映射 |
| `src/adapters/anthropic.rs` | anthropic-messages 适配器：7 个 compat 开关、Developer→user 降级、连续 toolResult 合并、thinking 签名回退、温度与思考互斥 |
| `src/adapters/openai_completions.rs` | openai-completions 适配器：按 provider/baseUrl 自动 compat 检测（max_tokens 字段、六种 thinking_format、deepseek reasoning_content 等） |
| `tests/adapters.rs` | 适配器集成测试：本地一次性 SSE 服务器；toolcall partial json、429 分类、缺 key 前置报错、流早断、取消 → Aborted |
| `tests/retry_streaming.rs` | 重试验收：真流式（非缓冲回放）、提交点前/后失败语义、空回复 Done 不被改写成 error |
| `tests/observations.rs` | on_payload 可就地替换请求体、三观察回调 panic 不击穿流 |

### crates/latent-agent — L1 基座：主循环与工具接缝（仅依赖 latent-ai）

| 文件 | 说明 |
|---|---|
| `src/loop_.rs` | 核心循环（1800+ 行）：显式 `Phase` 状态机（AwaitingRequest/Streaming/ExecutingTools/Settling/Done）；mpsc steering/follow-up 注入通道；`TurnLimits` 五项护栏（max_turns/max_tool_calls/max_total_tokens/deadline/max_truncation_retries）超限 → 可区分的 `RunStop::BudgetExhausted`；并行工具批执行（事件入队主任务串行派发）；jsonschema 参数校验；超长 tool result 头 60%/尾 40% 裁剪 |
| `src/agent.rs` | `Agent` 有状态薄壳：`run_with_lifecycle`（watch streaming 标志、panic 捕获合成 error 消息）、`steer`/`follow_up`/`wait_idle`（零轮询）、overflow 恢复用的 `trim_oldest_messages` |
| `src/tool.rs` | 接缝 #3：`trait Tool`（name/schema/execution_mode/prompt_snippet/execute）；错误走 `Err(ToolError)` 不编码进 content；`ToolOutput.terminate` 提前结束 |
| `src/hooks.rs` | 接缝 #2：`trait LoopHooks`（唯一必填 `convert_to_llm`，不得 panic）；`ToolBlock`（block 拦截 / args 改参两用）；`PassthroughHooks` 把 BashExecution/BranchSummary/CompactionSummary 包成 XML user 消息 |
| `src/event.rs` | `AgentEvent` 10 种 + `MessageDeltaPayload` 类型化增量 + `SharedPartial` 流式快照读口；`trait Subscriber` 串行 await 按订阅序保序 |
| `src/message.rs` | `AgentMessage`：user/assistant/toolResult + 五个自定义变体（BashExecution/BranchSummary/CompactionSummary/ModeSection/Custom 逃生口） |
| `tests/loop_integration.rs` | 循环集成测试（1500+ 行）：工具配对不变量（每 toolCall 恰一 toolResult，abort/panic/截断路径无缺口）、I3 硬退出 requeue、I4 并行双保序、I5 截断防振荡、护栏各项、转录纯净性 |
| `tests/loop_properties.rs` | proptest property test：随机 turn 序列下配对/双保序/abort 配对/截断防振荡不变量 |
| `tests/stream_deltas.rs` | 流式增量验收：事件序、thinking/toolcall args 逐块 delta、SharedPartial 单调增长、TurnEnd 携带 usage |
| `tests/block_images.rs` | `block_images` 配置将 Image 块替换为占位符的循环级验证 |

### crates/latent-session — L2 可选组件：会话持久化与压缩

| 文件 | 说明 |
|---|---|
| `src/entry.rs` | 会话 entry 类型：14 种变体（pi 11 种 + latent 扩展 tool_set_change/mode_change/context_ref），8 位 hex id + parentId 构成树；时间戳毫秒整数 |
| `src/manager.rs` | `SessionManager`：append-only JSONL 树唯一权威；`branch()` 移动 leaf 指针零拷贝分叉；崩溃半行恢复；`find_latest_session_file`/`list_session_files`（--continue/-l/TUI 会话切换共用候选集，按 mtime 倒序、排除子会话 tag 文件） |
| `src/projection.rs` | 上下文重建三算法（纯函数）：leaf→root 回溯、最新 compaction 虚拟展开、context_edit 投影（只投影 index==0 的 compaction）；哪些 entry 进/不进模型上下文 |
| `src/compaction.rs` | 自动压缩全套：触发阈值（窗口 − reserve，reserve 支持绝对值/百分比双轨）、token 估算（usage 优先 + chars/4 启发式）、切点算法（**绝不在 toolResult 处切**）、序列化（tool result 截 2000 字符）、`Summarizer` trait 注入接缝、length/error 摘要拒绝入库 |
| `tests/session_tree_and_compaction.rs` | 16 个集成测试：分叉投影、多压缩、切点边界、端到端 run_compaction、半行恢复 |

### crates/latent-tools — L2 可选组件：内置 8 工具

| 文件 | 说明 |
|---|---|
| `src/lib.rs` | `ToolRegistry`（装配期注册、重名 panic、运行期只读）+ 工具集工厂：默认集 read/bash/edit/write、只读集 read/grep/find/ls、全量 8 工具 |
| `src/read.rs` | read 工具：1 起始 offset/limit 切片、截断附续读提示、图片/二进制拒绝 |
| `src/bash.rs` | bash/powershell 共用工厂：流式输出、超时/中止杀进程树（Unix `process_group(0)` + kill(-pgid)，含孙进程）、默认超时（120s，`ShellTimeoutPolicy`）与自动转后台（生效超时 > 阈值 60s 时结算 tool result、watcher 托管进程、完成经 `BackgroundNotifier` follow_up 唤醒）、LATENT_* 环境注入（不覆盖已有变量）、commandPrefix 前置、`ShellSpawnHook` 改写/拒绝、沙箱拒绝事后提示；**工具结果字节级保真不净化** |
| `src/edit.rs` | 多点精确替换（每个 oldText 在原文件中唯一、互不重叠），BOM/行尾保持 |
| `src/write.rs` | 整文件写入，自动创建父目录 |
| `src/grep.rs` | 内容搜索：`ignore` crate 原生遍历（无外部 rg 依赖）、尊重 .gitignore 与 .latentignore、匹配行截 500 字符 |
| `src/find.rs` | glob 文件查找（含 `/` 的 pattern 自动锚定搜索根；尊重 .gitignore 与 .latentignore） |
| `src/ls.rs` | 目录列表（目录加 `/` 后缀、含 dotfiles；`.git` 恒排除 + `.latentignore` 过滤） |
| `src/powershell.rs` | bash 的 Windows 等价物，4 行 re-export 同工厂 |
| `src/truncate.rs` | 统一双限截断（默认 2000 行 / 50KiB，先到为准，永不返回半行）：`truncate_head`（read 保留头）/ `truncate_tail`（bash 保留尾）；工具自身输出预算 = agent 上限 − 2000 余量 |
| `src/output_accumulator.rs` | bash 流式聚合：增量 UTF-8 解码、超限完整输出落临时文件（`fullOutputPath`） |
| `src/search_ignore.rs` | 检索忽略列表（grep/find/ls/@弹窗共享）：`.latentignore`（gitignore 语法）经 ignore crate `GitignoreBuilder` 编译，全局数据目录 `.latentignore` + 项目 `.latentignore` 拼接、last-match-wins（项目可 `!` 反选）；锚定模式相对会话 cwd、命中目录剪枝不进入遍历；坏行诊断跳过，无规则 = 关闭过滤 |
| `src/file_listing.rs` | 文件清单采集（TUI `@` 文件弹窗数据源）：ignore crate 遍历（对齐 pi `--hidden --exclude .git`）+ searchIgnore 剪枝；按深度→目录优先→字母排序后截断（默认 2000 条） |
| `src/sanitize.rs` | ANSI/控制字符净化——**只用于 `!` 裸命令路径**，模型工具结果不做净化 |

### crates/latent-core — L3 业务核

| 文件 | 说明 |
|---|---|
| `src/session.rs` | 业务核 `AgentSession`：prompt/steer/follow_up、模式切换、激活工具集切换、overflow 恢复（`run_with_recovery`）与自动压缩（`maybe_auto_compact`）、`SessionSink`/`ContextCompactor` trait、事件翻译层 `SessionBridge`；EndTurn 后补追加流式切档被跳过的模式节（转录已有模式节才补） |
| `src/system_prompt.rs` | 系统提示词命名 sections 机制（preamble/tools/rules/project_context/env/addendum）；env 节含工作目录与当前本地时间，rules 节支持 system-prompt.md `<rules>` 标记块自定义追加；提示词不持久化，恢复时按配置重组 |
| `src/model.rs` | `ModelResolver`：`provider/model` spec 解析、内置默认模型表、/model 候选清单 |
| `src/config.rs` | models.json/settings.json 配置体系：全局数据目录（见 `src/paths.rs`）先读、项目 `.latent` 逐字段覆盖合并；apiKey 按环境变量名解析 |
| `src/paths.rs` | 用户数据目录解析唯一权威：`LATENT_HOME` 环境变量 → 旧版 `~/.latent`（存在即沿用）→ 默认 `~/.config/latent`；入口层解析后作参数下传，纯函数面不读环境 |
| `src/retry.rs` | provider 重试薄装配（`RetryHooks` 上报 AutoRetryStart/End 事件；quota 类不重试） |
| `src/permission/types.rs` | `SessionMode`（Plan/Confirm/FullAccess，默认 Plan，Shift+Tab 循环）、`ToolRiskClass` 静态分类、`Verdict`（Allow/Ask/Deny）、沙箱策略映射 |
| `src/permission/engine.rs` | `PermissionEngine` 判定引擎：FullAccess 全放 → 会话级审批缓存 → 按风险类分模式判定；可写根 = cwd + TMPDIR + /tmp + 配置。OS 沙箱仅 Plan 包装（Confirm 闸门 = 逐命令人审、批准后不被沙箱二次拦截；FullAccess 无闸门） |
| `src/permission/hooks.rs` | 审批流（before_tool_call 最外层装饰器）：Ask 时 await UI 应答；通道关闭 = Deny 不 fail-open。模式节不经 hooks：`apply_mode` 把当前模式提示词作为持久 `ModeSection` 消息 append 进转录（位置永久固定，append-only 保 KV 缓存前缀；压缩/溢出恢复后经 `ensure_mode_node` 补追加） |
| `src/permission/shell.rs` | shell 命令三态判定（只读/联网查询/明确写/未知，1400+ 行纯函数）：mini shell lexer、内置只读前缀表 + 明确写前缀表 + 联网查询前缀表、按 `;|&&` 分段校验、awk/sed/find/git 专用安全解析器 |
| `src/permission/interp.rs` | awk/sed 脚本词法分析：拒绝管道/system/重定向等写副作用 |
| `src/extensions/mod.rs` | 扩展接缝：`Extension` trait（编译期）、`ExtensionRegistry`、`ExtensionUi`（select/confirm/input/notify 反向通道）、`ExtensionActions` |
| `src/extensions/event_bus.rs` | 事件分发：15 观察类 + 6 决策类事件；block 短路、改参链（改参后重新过 schema）、fail-open/closed 按注册声明、断连诊断 |
| `src/extensions/mcp_host.rs` | MCP 协议宿主：settings `mcpServers` 声明 → stdio 子进程连接；自定义方法 `latent/register`/`latent/event`；崩溃/断连 = 标记 stale + 诊断 + 跳过，绝不击穿宿主 |
| `src/extensions/mcp_tool.rs` | MCP 工具桥：wire 名 `{extension}__{tool}`、progress 转发、取消通知映射、600s 兜底超时 |
| `src/subagent/mod.rs` | 子代理门面：同步 execute 内嵌套 await（并发上限 4）、异步立即返回 run id + supervisor 批量合并唤醒（wait_idle → 打字门控 → follow_up 合并通知 → continue_run）、递归防护 = 子工具面剔除 subagent |
| `src/subagent/defs.rs` | `AgentDef` 发现：`.latent/agents/*.md`（项目优先），frontmatter name/description/model/tools，正文即 system prompt |
| `src/subagent/runner.rs` | `run_child`：run/parent_cancel/stop/timeout select 竞速；进度节流；默认超时 30min |
| `src/subagent/registry.rs` | `SubagentRegistry`：8 位单调 id、后台 run 上限 16、`spawn_supervisor`（wait_idle → 打字门控 → follow_up 预算内合并通知唤醒父会话，panic 防护 + 诊断；通知抑制按 run 标记，abort_all 不泄漏进新登记的运行；pending_notices 供 print/json 退出等待）、stop/list/abort_all、run_state（TUI 卡片翻转依据） |
| `src/subagent/factory.rs` | `/subagent` 平行会话工厂：独立 PermissionEngine、系统提示词被定义 md 整体替换、可选落盘；工具池经 `ToolPoolFactory` 按会话现建（shell 沙箱钩子绑本会话引擎，模式切档即时生效） |
| `src/subagent/tool.rs` | `SubagentTool`：同步排队（信号量 4）、后台审批默认 Deny（fail-closed）、`action:"list"/"stop"` |
| `src/subagent/store.rs` | 子会话 JSONL 落盘（`ChildStoreFactory`，条目格式与主会话一致） |
| `src/skills/mod.rs` + `defs.rs` + `tool.rs` | 技能系统：`.latent/skills/*/SKILL.md`（项目优先）发现；唯一出口是 `load_skill` 工具 description（不注入系统提示词）；调用时才读全文 |
| `tests/permission_hooks.rs` | Approve/Deny/ApproveForSession/Abort 全路径、Plan 模式拒写 |
| `tests/session_integration.rs` | 事件流+持久化、steer/followUp、overflow 恢复两路、阈值自动压缩 |
| `tests/extensions_mcp.rs` / `extensions_registry.rs` | MCP 集成（in-memory transport 23 用例）、坏扩展隔离语义 |
| `tests/subagent_integration.rs` | 同步/异步闭环、递归防护、超时、并发排队、落盘 |

### crates/latent-web — L2 网页搜索/抓取（仅依赖 latent-agent + latent-ai）

| 文件 | 说明 |
|---|---|
| `src/router.rs` | 搜索路由总入口：auto 降级链 searxng→exa→brave→tavily→duckduckgo（有 key 用 key、没 key 用免费；每层缺失只跳过）；`all` 扇出合并去重 |
| `src/providers/` | 5 个 `SearchProvider` 实现（brave/duckduckgo/exa/searxng/tavily），`is_available` 纯本地零开销；tavily 支持 key-pool failover（`TAVILY_API_KEY_1..20`） |
| `src/bounded.rs` | 有界输出：模型可见文本默认截 30k 字符，附截断标记 + responseId 取回指引——**搜索结果不直接全文塞给模型** |
| `src/storage.rs` | 内存 Map（跨工具调用 responseId 协议）+ fetch 全文磁盘缓存（原子写、TTL 1h、LRU 128 条/128MB） |
| `src/tools/web_search.rs` / `fetch_content.rs` / `source_check.rs` / `get_search_content.rs` | 四工具（随会话常驻激活，保 prompt 缓存）：多 query 并发限 3；fetch 支持 readable/raw/answer 三模式；source_check 产出 research artifact；get_search_content 按 responseId 分页/findText 检索 |
| `src/content/ssrf.rs` | SSRF 校验：协议白名单、私网/保留段封锁、DNS 解析断言公网、CIDR 豁免；fetch 重定向**逐跳重校验** |
| `src/content/extract.rs` / `find.rs` / `page_query.rs` | 正文提取（readable→markdown）、findText 模糊检索（NFD + 编辑距离）、页面问答（小模型，预算 = 窗口×60%） |
| `src/prompts.rs` | 四工具 description/schema 的逐字契约（"模型行为的唯一依据"，含契约测试） |
| `src/credential.rs` / `http.rs` / `error.rs` / `config.rs` / `llm.rs` / `summary.rs` / `types.rs` | 凭据三来源（明文/$ENV/!shell 命令，redact）、手动重定向剥敏感头、错误分类驱动 fallback、web-search.json 配置、一次性 LLM 调用、auto-summary 30s 竞速回退确定性摘要、搜索纯类型 |
| `examples/e2e_manual.rs` | 手工联网端到端验证（`cargo run -p latent-web --example e2e_manual`） |

### crates/latent-tui — L4 可选组件：终端 UI（零内部依赖）

| 文件 | 说明 |
|---|---|
| `src/app.rs` | `TuiApp`：全帧差分渲染，双渲染模式——**regular**（pi TuiMainScreen 对应：定稿行缓存 ANSI 序列化只追加，与活动尾部拼成全帧逐行差分、只重绘变化区间，追加行越过屏幕底自然滚入原生 scrollback，变化落在已滚出区域或尺寸变化时全量重绘兜底，DECAWM 关闭防回绕）与 **fullscreen**（pi TuiAltScreen 对应：alternate screen + 屏幕内滚动，follow 时历史窗口贴底显示、尾部（预览尾窗+chrome）钉死屏幕底部；非 follow 时滚动锚扩展为「committed + 流式预览全量行」组合坐标——预览全量内容并入滚动视口、正在输出的内容随滚动移动，chrome（状态行/弹窗/编辑器/footer）仍钉底，`set_scroll_tail` 每帧接收预览上下文（尾窗行数/全量行数/全量行），整屏逐行差分、绝对定位重写绝不用 `\r\n` 滚动，翻页重叠 4 行、上滚视口冻结、End 恢复 follow，鼠标捕获（滚轮滚动 + 全屏模式恒可用的应用层选区：拖选反色高亮、Shift+点击扩展选区、有选区时 Ctrl+X 复制并清除高亮（/setting 可关，复制成功经 OSC 52 入系统剪贴板并显示右上角 Copied! toast 1.5s）、Alt+点击扩展选区（Shift 多被终端截留）；选区以帧行号锚定内容，滚动后高亮/复制跟随文字，光标永远钉在输入框不随滚动出屏），finish 时转录 dump 回主屏 scrollback）；`set_fullscreen` 运行时切换（共享 committed 缓存零迁移）；滚动 API `scroll_page_up/page_down/top/bottom/lines`；`on_mouse`/`copy_selection` 选区接口；`suspend/resume` 给 `$EDITOR` 让屏（全屏先退/重回 alt screen）；`Drop` 兜底恢复 |
| `src/editor.rs` | 多行编辑器（缓冲按 `Vec<Vec<char>>` 避免多字节索引问题）：undo、kill-ring、词导航、↑/↓ 历史；`token_before_cursor`/`replace_token_before_cursor` 供 `@` 弹窗做光标 token 检测与替换 |
| `src/view.rs`（modes/interactive 内） | 尾部帧组装纯函数：实时预览（流式活跃时按屏高给预算，工具命令卡片全显不设限；`ViewportFrame` 同时携带 `preview_window` 尾窗行数与 `scroll_extra` 预览全量行——仅非 follow 帧构建，供全屏滚动视口）→ 状态行（仅 busy，紧贴输出）→ 两行间隔 → 补全弹窗 → 编辑器 → footer；全帧差分下尾部高度逐帧自由变化 |
| `src/markdown.rs` | Markdown 渲染（标题/列表/围栏代码块 syntect 高亮/GFM 表格/行内样式） |
| `src/command_popup.rs` | 斜杠命令补全弹窗（前缀>子串>模糊打分；`/mode` 展开变体子项；↑/↓ 循环导航） |
| `src/file_popup.rs` | `@` 文件选择弹窗（pi @ autocomplete 对应）：光标 token 触发、目录 `/` 下钻直接子项、文件名前缀>子串>路径子串>模糊打分（↑/↓ 循环导航）；文件补全带尾随空格退场、目录保持下钻；`base`（工作目录）注入后**选中补全绝对全路径**（手输原样，过滤前自动剥离 base 前缀）；候选集由上层注入（不感知文件系统） |
| `src/tool_card.rs` | 工具调用卡片：命令本身完整折行（不随 ctrl+o 变化），输出折叠保留前 4 行 + `ctrl+o to expand`；状态色背景块（无外框，上下各一行同色内边距；成功绿/失败红为压暗低饱和色调，运行中中性）；Pending 卡片标题用旋转字符（实时预览逐帧动画，定稿转录行帧间不可变保持静态 ⏺） |
| `src/footer.rs` | 两行状态栏：上=左 cwd+git 分支 + 右 token 段（↑prompt 含缓存明细 U/R 与命中率 / ↓out / ctx% 变色 / $cost）；下=左 agent:main(或当前子 agent)│模式标记（plan 黄、full-access 红）+ 右 model·thinking |
| `src/theme/` | 语义主题：ratatui-themes 映射 + 逐主题微调（Tokyo Night/Catppuccin/Dracula…），16 色降级 ANSI |
| `src/popup.rs` | 弹窗圆角外框绘制（补全弹窗/选择器面板共用）：内容行截断内宽 + 补空格对齐右边框，边框色 `popup_border`、背景透明 |
| `src/highlight.rs` / `text.rs` / `width.rs` / `key.rs` / `loader.rs` / `select_list.rs` / `header.rs` | syntect 高亮单例 / span 感知折行截断 / 零依赖 CJK 宽度表 / 按键语义归一（含鼠标滚轮/左键手势 `Mouse`（携带 Shift 修饰）、Ctrl+X 复制在事件循环层拦截）/ spinner / 单选列表（↑/↓ 循环导航，选择器/审批弹窗共用） / 启动横幅 |

### crates/latent-cli — L4 可执行壳（bin: latent）

| 文件 | 说明 |
|---|---|
| `src/main.rs` | CLI 入口：flag 解析（--mode/--mock/--provider/--model/--theme/--continue|-c|-r [序号]/-l|--list/--session-mode/--plan/--yolo/--sandbox-*/--mcp-mock-server）、模式自动判定（两端 TTY → interactive 否则 print）、装配分支；入口助手与 landlock helper 分发来自 `latent_runtime::bootstrap` |
| `src/assembly.rs` | re-export `latent_runtime::assembly::*`（装配点已下沉 L2 装配层，路径兼容层） |
| `src/modes/print_mode.rs` / `json.rs` / `rpc.rs` | 三种非交互模式：print 流式打 stdout；json 事件 JSONL（剥离流式 partial）；两者退出前经 `wait_background_subagents`（assembly）等后台 subagent 全部结算并投递，结果不随进程丢失；rpc stdio JSONL 协议（prompt/steer/abort/getState/setModel/extension_ui_response/approval_response 等命令，长命令异步执行保持 stdin 可响应；approval_ui 与路由句柄同实例注入装配） |
| `src/modes/slash.rs` | re-export `latent_runtime::slash`（解析层下沉）+ `popup_entries()`（依赖 latent_tui::CommandEntry，interactive 专用） |
| `src/modes/interactive/` | TUI 装配与事件循环（`mod.rs`，含 tuiMode/copyOnSelect/ctrlXCopy 读取与快捷键配置注入键盘线程（RwLock 共享,/setting 切换即时生效）、滚动请求与模式切换的消费、有选区时 Ctrl+X 复制拦截、toast 到期驱动重绘、/model 配置入口的挂起跑 $EDITOR + 热重载）、UI 状态机（`state.rs`，含添加模型表单、ScrollRequest/tui_mode_switch 挂起标记、`StreamWrapCache` 流式文本增量折行缓存（append-only 时仅重折最后一个未完成源行，流式预览全量行的数据源）、`sync_mention_popup` @ 文件弹窗同步（cwd 采集候选 + 检索忽略剪枝 + 注入补全根（选中补全全路径）+ `/`/`!` 让位）、`subagent_run_cards` 异步卡片 (runId, transcript 下标) 绑定）、事件处理与按键（`handlers.rs`：双击 Ctrl+C 500ms 退出、Shift+Tab 切模式、审批数字键 1 批准/2 本会话批准/3 拒绝/4 中止、`!`/`!!` bash 透传、`@` 文件弹窗 ↑/↓/Tab/Enter/Esc 补全经编辑器 token 替换落文本、全屏模式 PageUp/PageDown/Home/End/滚轮 → 滚动请求、/setting 选择器与应用（apply_setting_selection 切换并经 write_setting_field 写回全局数据目录 settings.json）、/model 选择器+添加模型表单）、异步 subagent 卡片 runId 绑定与结算翻色（`flush_settled_subagent_cards`：pending → 绿/红，全文重绘至多一次）、打字门控回写（`sync_wake_gate`：编辑器非空 → supervisor 唤醒延迟）、UI 事件通道（`events.rs`）、启动回放（`replay.rs`）、用量追踪（`usage.rs`）、视图渲染（`view.rs`，选择器/审批/扩展 confirm 等模态选择列表与「添加模型」表单信息区为圆角外框面板（`popup::frame`），选择器 prompt 按 `\n` 拆行、超宽折行）、bash 净化（`bash.rs`，latent 唯一内容净化路径，8000 字符截断）、装配级单测（`tests.rs`） |
| `src/mcp_mock.rs` | mock MCP 扩展服务端（`latent --mcp-mock-server`）：订阅 tool_call 拦截危险 bash + 注册 echo 工具 + elicitation 确认，供扩展全链路验收 |
| `tests/modes.rs` | 四模式集成测试（ScriptedProvider 不联网）：json 剥 partial、rpc 反向通道、LATENT_* 注入、Plan 只读 bash、JSONL 重建 == 内存 context、ModeChange 持久化、**rpc 审批装配链路回归**（approval_ui 与路由句柄同实例） |
| `tests/e2e_mcp_extension.rs` | 真实子进程 MCP 扩展端到端验收 |

### crates/latent-runtime — L2 装配层（业务核共享装配点）

| 文件 | 说明 |
|---|---|
| `src/assembly.rs` | **共享装配点** `build_session`（自 latent-cli 迁出，四种运行模式与聊天网关共用）：扩展总线 + 权限引擎 + 审批/扩展两层 hooks 洋葱（Approval 最外 → Extension）+ 沙箱 spawn 钩子（仅 Plan 包装）+ LATENT_* 环境 + 重试装饰器 + web 四工具 + LoadSkill/Subagent 工具 + 会话持久化与压缩器；settings 解析（项目 `.latent/settings.json` 优先）；`switch_new_session`/`switch_resume_session` |
| `src/bootstrap.rs` | 入口层共享助手：`resolve_provider_and_model`（mock + models.json 体系）、`resolve_session_store`（-c/-r）、`print_session_list`、`notify_legacy_data_dir`、`maybe_dispatch_landlock_helper`（**两个 bin 的 main 开头都必须调用**，否则 Linux 沙箱静默失效） |
| `src/events.rs` | json/rpc/聊天网关共用的事件 → JSON 映射（剥离流式 partial） |
| `src/slash.rs` | 斜杠命令解析层（`COMMANDS`/`SlashAction`/`parse`/`help_markdown`；TUI 的 `popup_entries()` 留在 latent-cli） |

### crates/latent-channel — L1 纯聊天域（零内部依赖，feature 门控渠道）

| 文件 | 说明 |
|---|---|
| `src/types.rs` | 统一消息模型：`ChatRef`/`Sender`/`Segment`（Text/At/Image/File/Reply）/`InboundMessage`（`to_me`/`reply_to_me` 由渠道归一化算好）/`OutboundMessage`/`ChannelEvent`/`ChannelStatus`；**防环是渠道责任**（归一化层丢弃自消息，不加字段） |
| `src/plugin.rs` | `trait ChannelPlugin`（id/start/apply_config）+ `ChannelSender`（mpsc 命令通道，出站串行化）+ `ChannelHandle`（send/typing/status）+ `spawn_guarded` panic 边界（任务 panic → `Status::Failed`，绝不击穿 daemon） |
| `src/debounce.rs` | 入站防抖合并：按 (platform, conversation, user) 缓冲；窗口首条固定不顺延（`MAX_DEBOUNCE_WINDOW_MULTIPLIER = 5` 上界）；键上限 2048；命令/At 消息立即冲刷；flush 产物 message_id = `debounce:{首条}`（合成新键防撞去重表） |
| `src/chunk.rs` | 出站长文分段：`DEFAULT_CHUNK_LIMIT = 4000`；length 模式（括号外换行 → 空白 → 硬切）/ newline 模式（空行段落）；markdown 版闭合 code fence 续块重开 |
| `src/mention_gating.rs` | @机器人判定（纯函数）：私聊恒 to_me；群聊 At == bot id ∨ reply_to_me ∨ mentionPatterns（大小写不敏感）；patterns 优先级 agent > groupChat > identity.name 派生 |
| `src/typing.rs` | typing 指示器生命周期：keepalive 3000ms / TTL 60000ms / 连续失败 2 次停；typingMode 四档（message 默认 —— DM 与被 @ 群聊入站即发）；**无"typing 完成才发送"的门** |
| `src/credential.rs` | 凭据三来源解析（明文/`$ENV`/`!shell`）L1 自包含副本（§8 偏离 10，语义/单测对齐 latent-web credential.rs）；`is_shell_source` 供项目级配置拒绝 `!shell` |
| `src/error.rs` | `ChannelError`：出站分类（ChatNotFound/NotInGroup/RateLimited{retry_after_ms}/Unsupported/DeliveryFailed）+ 生命周期（Config/Startup）；`retry_after`/`is_retryable` |
| `src/telegram/` | feature `telegram`：Bot API getUpdates 长轮询（reqwest 直连不引 SDK，`apiBase` 可指向本地假服务器）；getMe 自检 + bot 身份；sendMessage/reply_to_message_id/sendChatAction；429 → RateLimited；mention 实体 offset/length 按 **UTF-16 code units** 解析（CJK 前缀群 @ 判定；畸形实体跳过不丢消息）；reqwest 错误文本折叠 `/bot<token>/` 段防凭据进日志 |
| `src/qq/` | feature `qq`：OneBot 11 反向 WebSocket（axum 服务端，NapCat 是客户端）；Bearer Header 常数时间校验（不走 query）+ 5 次失败 60s 冷却（**按对端 IP 计键** + 失败表有界）；echo oneshot 表关联 action 应答；meta_event 记 self_id；**user_id == self_id 自消息源头丢弃（防私聊恒 to_me 死循环）**；已发消息 id 表支撑 reply_to_me；单帧 1MiB（WS 升级前置）/并发 ≤8；`path` 配置非法 → Config 拒绝启动不 panic |
| `src/wecom/` | feature `wecom`：企业微信智能机器人 WS 长连接（wss://openws.work.weixin.qq.com；`aibot_subscribe` 握手 → `aibot_msg_callback` 回调 → `aibot_send_msg` 推送；30s ping）；订阅被拒（errcode≠0）→ Failed 不再重试；其余握手/传输故障 → 可重试分类退避重连；帧上限前置到 WS 客户端配置 |
| `src/mock.rs` | feature `mock`：内存假渠道（push_inbound 注入 / sent() 断言 / 可编程发送失败）——gateway 集成测试基础设施 |
| `tests/telegram.rs` / `tests/qq.rs` / `tests/wecom.rs` | 假平台服务器集成测试：更新归一化与 @/reply 判定、出站协议帧、自消息防环、错误 token 拒绝、订阅拒绝 → Failed |

### crates/latent-gateway — L3 聊天网关（引擎 + bin: latent-gateway）

| 文件 | 说明 |
|---|---|
| `src/main.rs` | daemon 入口：默认前台跑 daemon；子命令 `pairing list/approve`、`channels status [--probe]`、`status`（**控制面薄客户端**，token 读 `$LATENT_GATEWAY_TOKEN`，禁止直写 state.json）；配置非法/缺 token → 退出码 78；workspace 非空则进程级切换 cwd；Ctrl-C 优雅停机（abort subagent + 渠道断开 + state 落盘） |
| `src/config.rs` | gateway.json schema（全 deny_unknown_fields）+ 校验：未知键/坏类型/非法值 → `ConfigError` → 退出码 78；项目逐字段覆盖全局（Value 深合并）；**项目级 `!shell` 凭据来源拒绝启动**（只扫项目文件自身子树）；项目级覆盖安全敏感字段 → stderr 警告；危险配置 stderr 警告不阻断（dmPolicy open / 空 allowlist / 明文 token）；`materialize_channel_credentials`（$ENV/!shell 解析结果同时写回类型化配置与 raw Value） |
| `src/routing/session_key.rs` | session key 计算（对齐上游 `buildAgentPeerSessionKey`）：群 `agent:{agent}:{channel}:group:{id}`、私聊 dmScope 四档；空段 `"unknown"` 兜底；agent 默认 `main` |
| `src/agents.rs` | `ChatSessionRegistry`（每 session key 一个 ChatSession，**惰性创建**；registry 锁只护 map 读写，构建走 per-key 锁 + 双检）+ `SessionFactory`（provider/settings/MCP specs 全局解析一次共享；run_lock 同会话串行）+ `SessionEventPump`（订阅者只推 channel；assistant MessageEnd 权威定稿、AgentSettled 只发一次；进度 30s 节流 try_send 满即丢、final 阻塞不丢） |
| `src/auto_reply/mod.rs` | dispatch 管线：去重 claim（**先于 ACK**）→ 授权（dmPolicy/groupPolicy/requireMention；**owner 绕过 DM 门禁** —— 审批应答必须可达，GATEWAY_PLAN 偏离 14）→ 命令拦截 → 防抖（冲刷逐条 spawn，不阻塞其他会话）→ session key → 队列处置 → execute_turn（envelope → prompt → pump 收集；wait_idle 后复查滞留注入经 `continue_run` 消化）；四模式队列（steer/followup/collect/interrupt）；`inject_operator_message`（chat.send，sender 固定 operator，忙则 steer）；命令权限天花板（full-access 按 `SessionMode::parse` 语义判定恒 owner、Mode 分支二次校验；群会话修改命令仅 owner）；忙时 `/new` `/reset` `/compact` 预检拒绝 |
| `src/auto_reply/queue.rs` | `QueueSettings`（steer/500ms/cap 20/summarize 默认）+ `PendingQueue`（collect 的网关自管缓冲；drop=new 回执拒绝、summarize 取过即清合成提示） |
| `src/auto_reply/envelope.rs` | 入站包装：`[{channel} {chat_type}:{id} {sender} {HH:mm}] body`；头部字段清洗防伪造层级（方括号替换 + 换行/控制字符压平，单行信封头不可拆） |
| `src/auto_reply/commands.rs` | 聊天命令解析（/new /reset /compact /stop /status /model /thinking /mode /queue /activation /approve /help）；**仅 `/approve` 允许裸词**（对齐上游正则，其余必须行首 `/`）；`/approve` id 与 decision 顺序可换、别名表；owner 判定 `<channel>:<userId>` |
| `src/auto_reply/reply_dispatcher.rs` | 出站保序 sendChain（每会话单消费任务）：进度/最终回复串行；final 只发一次；长文 chunk 分段（首段不延迟）；单段发送 60s 兜底超时（渠道悬挂不永久卡死链）；`ReplySink::Channel / ControlPlane`（operator 会话回复上控制面事件线）；限速 retry_after 重试一次 |
| `src/approval.rs` | `ChatApprovalUi` 全局单例（id 全局自增 + pending oneshot 表，`/approve` 跨会话路由）；审批消息发 owner DM（模板附裸词应答提示）；owner 回复裸 decision 词（allow-once/allow-always/deny）→ 应答最新待审；发送失败/发送超时（10s）立即 Deny 不等 120s；超时/断开 → None = Deny（fail-closed） |
| `src/pairing.rs` | DM 配对：8 位码（大写剔除 0O1I）1h 过期；每渠道 pending ≤ 3；`pairing|allowlist|open|disabled`（open 需列表含 `*`）；批准只授 DM |
| `src/state.rs` | `<数据目录>/gateway/state.json`（原子写 tmp+rename 且 write+rename 全程持锁，并发 save 无撕裂；**唯一写者是 daemon**）：session key → 会话文件（重启 resume）+ pairing pending/approved |
| `src/channels.rs` | 渠道工厂 `create_channel`（feature 门控，具体渠道类型唯一出现点）+ `ChannelManager`（attach/start_enabled/状态记录/每渠道转发任务补名） |
| `src/control/` | WS 控制面（axum，帧格式对齐上游）：`server.rs`（首帧 connect 强制、单帧 1MiB 断连、并发 ≤8、事件 per-connection seq 单调）、`auth.rs`（token 常数时间比较 + 5 次失败 60s 冷却）、`methods/`（connect/health/status/chat.send/chat.abort/chat.history/sessions.list/sessions.reset/channels.status/pairing.list/pairing.approve/config.get redact）、`client.rs`（薄客户端：CLI 子命令与测试共用）、`events.rs`（agent/chat/channels/health/shutdown） |
| `tests/e2e.rs` | 引擎集成测试（mock 渠道 + ScriptedProvider 全离线，18 例）：端到端回复、去重、群 allowlist + @ 门、聊天命令、群会话 owner-only、full-access 别名天花板（P0-1）、忙时 /new 拒绝、跨会话防抖冲刷不阻塞、四模式队列、cap 溢出、pairing 全流程、审批往返 owner DM、/approve owner-only、state.json 重启恢复 |
| `tests/telegram_approval.rs` | telegram 全链路审批回归（真 TelegramChannel + 假 Bot API 服务器 + ScriptedProvider）：confirm 审批往返（请求 → owner /approve → 工具执行 → 回复）、owner 未配对 DM 免门禁、陌生人仍走配对、start_enabled `$ENV` 凭据物化端到端 |
| `tests/control.rs` | 控制面集成测试（9 例）：握手 hello-ok、错误 token 拒绝、首帧强制 connect、chat.send 走同一管线、sender 字段拒绝、pairing 方法、事件 seq 单调、1MiB 断连（帧上限前置，发送中途被重置也算断连）、config redact |

### crates/latent-sandbox — 沙箱（零内部依赖）

| 文件 | 说明 |
|---|---|
| `src/lib.rs` | `SandboxPolicy`（ReadOnly / WorkspaceWrite / DangerFullAccess）；`detect_availability()`：macOS 真实执行探测 sandbox-exec、Linux 先 bwrap 后 Landlock；策略要求沙箱但平台不可用 → 报错降级审批，**绝不静默裸跑**；职责边界：只做命令包装，不做审批/拒绝 |
| `src/seatbelt.rs` | macOS Seatbelt 后端：固定 `/usr/bin/sandbox-exec` 防 PATH 注入；可写根内 `.git`/`.latent`/`.env` 显式只读 |
| `src/bwrap.rs` | Linux bubblewrap 后端：`--unshare-all` + 只读根 + 可写根 bind + 后置 ro-bind 保护 |
| `src/landlock.rs` | Linux Landlock+seccomp 后端（bwrap 备选）：以本可执行文件为 helper（`--latent-landlock-helper`，由 CLI main 自检调用），仅 Linux 编译 |

### tests/e2e/ — Python E2E 测试（详见该目录 README.md）

| 文件 | 说明 |
|---|---|
| `harness.py` | 驱动核心 `LatentApp`：隔离临时 HOME + pexpect 真 PTY 启动 latent + pyte 解析屏幕；API：`wait_ready`/`sendline`/`send_key`/`expect_text`（正则、忽略空白）/`expect_absent`/`visible_text`/`transcript`/`wait_for_requests`/`quit`；`finally` 必须 `close()` |
| `mock_llm.py` | 本地 mock LLM：伪装 anthropic-messages SSE 端点，按场景 JSON 逐 turn 返回（`{"text":…}` / `{"tool_calls":[…]}` / `{"error":…, "status":500}` 三种 turn；`delay_ms` 响应前延迟、`chunk_delay_ms` delta 间逐块延迟模拟慢速流式），记录请求体供反向断言 |
| `conftest.py` / `pytest.ini` / `requirements.txt` | sys.path 注入 / DeprecationWarning 过滤 / pexpect+pyte+pytest（装全局环境，不建 venv） |
| `test_*.py`（32 个场景） | startup 横幅、ask_and_reply 问答、tool_roundtrip 工具闭环、abort/abort_then_continue、ctrl_c 双击退出、steering 注入、continue 恢复、provider_error 重试、session_half_line 崩溃恢复、bash_tool 截断、bash_sanitize 净化对齐、parallel_tools 源序、tool_validation 非法参数、ctrl_o 折叠、write_edit 落盘、compact 空对话回归、new_session、plan_mode 审批流、theme、output_display CJK 回归、slash_commands、shift_enter 多行输入、session_resume（-r/-l//session 切换）、fullscreen（钉底/翻页/视口冻结/模式切换）、fullscreen_stream_scroll（流式进行中滚动，正在输出的内容随滚动移动）、file_mention（@ 文件弹窗与纯文本提交）、latentignore（.latentignore 项目/全局规则过滤 @ 弹窗候选 + `!` 反选恢复）、subagent_session（子会话谱系 /session 恢复 + 与主会话互不可见）、quit |
| `scenarios/*.json` | 26 个 mock 响应脚本（格式见 `scenarios/README.md`） |

## 配置文件体系

**用户数据目录**：全局配置/数据的根目录，解析优先级 `LATENT_HOME` 环境变量 → 旧版 `~/.latent`（存在即沿用，启动时 stderr 提示一次）→ 默认 `~/.config/latent`；解析唯一权威在 `latent-core/src/paths.rs`，入口层解析后作参数下传。下表记作 `<数据目录>`。

| 文件 | 位置（项目优先，逐字段覆盖全局） | 内容 |
|---|---|---|
| settings.json | `.latent/settings.json` / `<数据目录>/settings.json` | `mcpServers`（MCP 扩展声明）、`commandPrefix`、`bashTimeoutSecs`（bash 默认超时，默认 120）、`backgroundAfterSecs`（bash 自动转后台阈值，默认 60）、`tools`（空数组 = 不激活任何工具）、`toolResultMaxChars`（默认 20000）、`compaction.reserveTokens`（≥1 绝对值，<1 窗口百分比）、`sessionMode`、`headlessApproval`/`subagentAsyncApproval`（默认 deny，fail-closed）、`sandbox`、`approval`、`theme`、`tuiMode`（fullscreen = 默认 alternate screen 输入区钉底；regular = 终端 scrollback；`--tui-mode` 参数优先）、`ctrlXCopy`（Ctrl+X 复制开关,默认 true）、`copyOnSelect`（选中后自动复制,默认 false;选择/高亮/Ctrl+X 复制互不影响） |
| .latentignore | 项目 `<cwd>/.latentignore` / 全局 `<数据目录>/.latentignore` | AI 检索忽略规则（gitignore 语法，格式同 .gitignore）：grep/find/ls/@文件弹窗过滤 + 系统提示词规则。全局在前、项目在后拼接，gitignore 语义 last-match-wins——项目可用 `!` 反选全局规则。文件缺失静默跳过，坏行诊断后跳过；无任何规则 = 不做额外过滤（`.git` 由工具层恒排除，遍历仍自带 .gitignore 感知）。启动时读一次，中途修改不生效；不支持子目录级 `.latentignore` |
| models.json | `.latent/models.json` / `<数据目录>/models.json` | 自定义 provider/model 覆盖（baseUrl、定价、compat）；apiKey 值优先按环境变量名解析；顶层 `showBuiltinModels: false` 时 /model 候选不追加内置 provider 默认表（缺省 true）；/model 选择器末尾内置「添加模型」表单与「编辑 models.json」（$EDITOR：LATENT_EDITOR > VISUAL > EDITOR > vi）两个配置入口，写回后热重载 |
| web-search.json | `.latent/web-search.json` / `<数据目录>/web-search.json` | 各搜索 provider key（支持 `$ENV`/`!shell` 来源）、searchRouting fallback、maxInlineContentChars、proxy、cache |
| gateway.json | `.latent/gateway.json` / `<数据目录>/gateway.json` | 聊天网关配置（GATEWAY_PLAN §4.2）：`agents.defaults`（workspace/model/typingMode）、`gateway`（port 18789/bind 127.0.0.1/auth.token **必填**，缺失 → 退出码 78）、`channels`（qq/wecom/telegram/mock 各自 enabled/凭据/textChunkLimit/dmPolicy/allowFrom；渠道扩展键已类型化：telegram `apiBase`/`pollTimeoutSecs`、qq `path`、wecom `wsEndpoint`/`heartbeatIntervalSecs`）、`session`（dmScope=per-channel-peer/groupScope=per-group/reset）、`messages`（queue: steer/500ms/cap 20/summarize + debounceMsByChannel；groupChat: requireMention/groupPolicy=allowlist/groupAllowFrom/mentionPatterns/unmentionedInbound —— **群策略唯一定义点**）、`commands.ownerAllowFrom`。全 deny_unknown_fields，未知键 → 退出码 78；**项目级配置拒绝 `!shell` 凭据来源**（只扫项目文件自身子树，全局合法 `!shell` + 项目文件存在不误拒）；项目级覆盖安全敏感字段（gateway.auth/ownerAllowFrom/groupChat/session）→ stderr 警告 |
| skills | `.latent/skills/<name>/SKILL.md` / `<数据目录>/skills/…` | frontmatter name/description（必填）+ 正文；经 `load_skill` 工具按需加载 |
| agents | `.latent/agents/<name>.md` / `<数据目录>/agents/…` | frontmatter name/description/model/tools + 正文即 system prompt；驱动 `subagent` 工具与 `/subagent` 命令 |
| system-prompt.md | `.latent/system-prompt.md` / `<数据目录>/system-prompt.md` | 块外内容替换系统提示词身份句（动态节保留），`<rules>...</rules>` 标记块内容追加进 `<rules>` 节（无标记块 = 全文是身份句） |

会话文件写 `<数据目录>/sessions/<项目前缀>/`（目录名 = cwd 编码；文件名 `<时间>__<tag>__<id>.jsonl`，时间为本地 %Y%m%d-%H%M%S）；`contextSnapshot` 开启时请求快照落旁路 `.ctx/` 目录，不进模型上下文。

## 测试方针

### 1. Rust 单元/集成测试

```bash
cargo test --workspace                  # 全部测试二进制须全绿（含 property test）
cargo clippy --workspace --all-targets  # 必须零警告
```

- 单测写在源文件 `#[cfg(test)] mod tests` 内；跨 crate 行为写在 `crates/*/tests/` 集成测试。
- **不得破坏的不变量**（改动主循环/工具/会话时重点回归）：
  - 每个toolCall恰好一个toolResult：`Vec<ToolOutcome>` 长度恒等于 toolCall 数，abort/panic/截断路径无配对缺口（proptest 钉死）；
  - 转录即真相：回放转录 = 重建完整请求状态；循环不向转录注入合成消息；
  - I3 硬退出不碰队列：error/aborted 不消费注入通道，取出未消费的注入经 requeue 交还宿主；
  - I4 并行双保序：toolcall_end 事件按完成序、tool result 消息按源序；
  - I5 截断防振荡：连续 length 截断 >3 轮 → `BudgetExhausted(TruncationRetries)`；
  - 流式重试只在提交点（首个内容 delta）之前静默整轮重试，之后直通；低层循环无内建 provider 重试（I2），重试经装饰器注入；
  - append-only 会话树：分支/压缩/上下文修改都是追加 entry，从不改写历史；切点绝不在 toolResult 处。
- 改动核心逻辑须附带边界条件测试；集成测试用 `ScriptedProvider`/`MockProvider`（`latent-ai/src/mock.rs`），不联网。

### 2. E2E 测试（Python，详细规范见 tests/e2e/readme.md）

像人一样测试：真 PTY 启动 latent 二进制、模拟键盘输入、pyte 把 ANSI 解析回"用户看到的屏幕"断言；LLM 由本地 mock 替代，响应完全确定。

```bash
cargo build --bin latent                       # 1. 编译被测二进制（LATENT_BIN 可覆盖路径）
pip install -r tests/e2e/requirements.txt   # 2. 首次：pexpect/pyte/pytest（全局环境，不建 venv）
cd tests/e2e && pytest -v                   # 3. 运行（仅 macOS/Linux）
```

- 新增用户可见行为（TUI 交互、斜杠命令、审批流、模式切换等）应补一个场景：`scenarios/` 加 JSON 响应脚本 + 新建 `test_xxx.py`。
- 关键坑：`expect_text` 剥 ANSI 且忽略全部空白（markdown 会在 CJK 间插空格），pattern 不要依赖排版；不要绕过 harness 直接读 child 输出（光标查询需 harness 应答）；默认 `session_mode="full-access"`，测 Plan/审批流须传 `session_mode=None`；`finally` 必须 `app.close()`；重试/延迟场景显式加大 timeout（20–40s）。

## Rust 开发规范

- **`unsafe` 全仓禁止**：`[workspace.lints.rust] unsafe_code = "forbid"`，不得引入 unsafe 块或局部放宽。
- **外部依赖一律 pin 精确版本**（如 `rmcp =3.4.1`、`jsonschema =0.58.0`、`nix =0.31.3`、`proptest =1.9.0`），统一登记在根 `Cargo.toml` 的 `[workspace.dependencies]`，crate 内 `xxx.workspace = true` 引用；新增依赖须先在此登记并说明用途。
- **依赖严格单向**：不得反向/同层依赖；可选组件经 trait 在装配期注入，不得在业务核直接依赖可选 crate 的具体类型（如 latent-core 只认识 `SessionSink`/`SandboxPolicy` trait，不认识 latent-session/latent-sandbox）。
- **crate 对外只暴露工厂、trait、类型三样**；实现类型不 `pub`（如 latent-ai 的 adapters/sse/json_parse 全部 `pub(crate)`，只能经工厂出厂）。
- **无可变全局状态；trait 方法不 panic**：错误用 `thiserror` 定义类型化错误向上传播；工具错误走 `Err(ToolError)` 交给循环转错误 tool result；观察回调/扩展错误一律吞掉 + 诊断，绝不击穿宿主（fail-closed 优先，fail-open 须注册时显式声明）。
- **异步统一 tokio**（workspace 固定 features）；trait 异步方法用 `async-trait`；取消经 `CancellationToken`/select 竞速，不用强制杀任务。
- **协议与序列化约定**：对外 JSON 字段 camelCase、entry `type` 判别符 snake_case；时间戳用毫秒整数；serde 判别符有单测钉死（改名会破坏会话文件兼容）。
- **输出预算纪律**：任何进模型上下文的工具输出必须经统一截断（`latent-tools/src/truncate.rs`），且"输出+提示文本"总预算不得触发 agent 层二次裁剪；web 类输出预算与 agent `tool_self_output_limit` 派生值取较小。
- **错误处理 fail-closed**：schema 非法拒绝执行；审批 UI 通道关闭 = Deny；沙箱不可用降级审批而非裸跑。
- 提交前检查：`cargo build --workspace` + `cargo test --workspace` + `cargo clippy --workspace --all-targets` 全通过零警告；E2E 影响到的场景跑 `pytest` 对应用例。

## 参考：pi 上游实现

本项目参考 [pi 的官方仓库](https://github.com/earendil-works/pi)（TypeScript 实现）。当行为语义不确定时（主循环阶段机、会话树 entry 语义、compaction 切点、MCP 扩展事件集、RPC 命令、markdown 渲染等），以上游 pi 对应实现为准对齐；记录在案的偏离：时间戳用毫秒整数（pi 用 ISO 字符串）、压缩单请求、不追求会话文件逐字节兼容、latent 自有扩展 entry（tool_set_change/mode_change/context_ref）与权限/沙箱/子代理/web 搜索为 latent 新增能力。代码注释中偶见的「XX 文档 §N」编号是已移除的设计文档遗留，遇行为描述与代码冲突时以代码实际行为与本文件为准。

## 参考：OpenClaw 上游实现（聊天网关）

latent-gateway 是 [OpenClaw](https://github.com/openclaw/openclaw)（MIT，个人 AI 助手网关）的架构级复刻：常驻 Gateway 进程拥有所有聊天渠道连接与所有 agent 会话，暴露 WebSocket 控制面。**行为语义、常量、协议格式一律先查 OpenClaw 对应源文件**（对照表见 `GATEWAY_PLAN.md` §6：防抖窗口语义、分段断点、出站保序、echo 关联、去重先于 ACK 等坑位上游已解决，直接继承）；概念命名（session key、QueueSettings、mention gating、typingMode、pairing、hello-ok 等）照抄上游。已登记偏离见 `GATEWAY_PLAN.md` §8（12-14 为实施后新增），要点：会话存储用 latent-session JSONL（上游 sqlite）、单 agent MVP、**安全模型更严**（full-access 切换与审批应答均 owner-only，群会话修改命令仅 owner；**owner 私聊免 dmPolicy 门禁** —— 审批应答必须可达）、`chat.send` 注入消息固定 operator 身份、QQ/企微渠道为按 ChannelPlugin 接口语义新写、凭据解析器在 latent-channel 内置 L1 自包含副本、`channels.mock` 测试基建配置节。
