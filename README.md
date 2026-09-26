# rpi — pi agent 的 Rust 重写

> 一个终端 AI 编码 agent:流式驱动 LLM、并行执行工具、append-only 会话树、进程外扩展系统、四种运行模式共享同一业务核。

本仓库是对 [earendil-works/pi](https://github.com/earendil-works/pi) 的 Rust 复刻,`docs/` 下的 12 篇分析文档是唯一设计权威(核心循环见 `docs/03-agent-loop.md`,模块接线与 Rust 映射见 `docs/09-wiring-and-rust.md`,实现纪律见 `docs/10-implementation-policy.md`)。

---

## 核心特性

- **流式优先**:逐 delta 流式渲染回复、思考过程(thinking)与工具调用参数;真实的 SSE 帧级流式重试——失败在内容提交前静默换轮重试,提交后失败按契约编码进事件流,绝不悄悄退化成伪流式。
- **转录即真相**:系统提示词与工具集声明以"转录中的 system 消息"形态存在,回放转录 = 重建完整请求状态;工具集变更由循环的 `declare_tool_changes` 自动 diff 公告给模型。
- **结构化循环**:双层 while 主循环 + 显式 `Phase` 状态机 + mpsc 推送式注入(steering/follow-up),配 6 条不变量的 property test(I1–I6:转录单一真相、错误进转录、硬退出语义、并行双保序、length 截断防御、工具声明重放一致性)。
- **工具完整性**:每个 toolCall 恰好一个 toolResult 由 `Vec<ToolOutcome>` 类型不变量保证(abort/panic/截断路径均不产生配对缺口);完整 JSON Schema 参数校验(`jsonschema` crate),非法参数在执行前拦截。
- **护栏内建**:max_turns / max_tool_calls / max_total_tokens / deadline 预算护栏与 length 截断防振荡,终止原因可区分(`BudgetExhausted` 不伪装成 error)。
- **会话可恢复**:JSONL append-only 会话树、分支、自动 compaction(上下文超限时裁剪重放)、overflow 自动恢复。
- **扩展即进程**:MCP 扩展以独立进程运行,连接失败/运行期错误 = 诊断 + 跳过,绝不击穿宿主;扩展可注册工具、订阅约 40 种事件、经反向通道调用 UI(select/confirm/input/notify)。
- **crate 级可拆卸**:7 个 crate 四层单向依赖,rpi-session / rpi-tools / rpi-tui 移除任意一个其余 crate 仍零警告编译;可选组件一律经 trait 对象在装配期注入。

---

## 快速开始

```bash
# 构建
cargo build --release -p rpi-cli

# 无需 API key:mock provider 走通全链路
cargo run -p rpi-cli -- --mock "你好"

# 接真实模型(以 anthropic 为例)
export ANTHROPIC_API_KEY=sk-ant-…
rpi --provider anthropic "列出当前目录结构"
```

帮助:`rpi --help`。

---

## 四种运行模式

四种模式只是同一业务核(`AgentSession`)的不同 I/O 壳;不指定 `--mode` 时自动判定——交互终端进 TUI,否则进 print。

### print(单次执行)

```bash
rpi "修复这个 bug"                     # 非交互 shell 中默认进入
cat prompt.txt | rpi                   # prompt 可经 stdin 管道输入
```

流式输出回复到 stdout,工具调用打印一行状态,结束后输出终止原因;重试事件(`[retry #N in Xms]`)打 stderr。

### interactive(TUI 聊天)

```bash
rpi --mode interactive                 # 需要终端
```

- 差分渲染 TUI:编辑器、消息流、footer 状态栏(当前模型 · thinking 级别 · context% · 会话累计用量;context 超 70%/90% 变色);
- **流式可见思考过程与工具参数逐块增长**(delta 为主 + partial 快照读口);
- 每回合结束显示用量行(输入/输出/缓存读/缓存写 token + 费用);错误/中止回合红字上屏(`Error: …` / `Operation aborted`),自动重试最终失败同样可见;
- 启动/续聊时回放当前转录:user 反色块、assistant 正文、工具调用与结果、压缩摘要(`--continue` 恢复历史可见);
- 斜杠命令:`/help` `/model [provider/model]` `/thinking [level]` `/compact` `/session` `/quit`;未识别的 `/xxx` 本地警告(不发给模型);
- `Enter` 发送;run 进行中输入自动转为 **steering**(当前 turn 结束后注入);`Ctrl+C` 中断当前 run(空闲时 500ms 内双击退出),`Ctrl+D` 退出。

### json(事件 JSONL)

```bash
rpi --mode json "…" > events.jsonl
```

AgentSession 事件逐行序列化到 stdout(delta、toolcall_start 附 id/toolName、turn_end 携带 usage、extension_notify 等),适合被程序消费;UI 交互类调用返回默认值(headless)。

### rpc(stdio JSONL 协议)

```bash
rpi --mode rpc
```

每行 stdin 一个命令(serde tagged JSON,`type` 字段 camelCase),stdout 返回 `{"type":"response","id":N,"ok":…}`。命令集:

| 命令 | 说明 |
|---|---|
| `prompt` / `steer` / `followUp` | 发起 run / 中途注入 / 停止点注入(prompt 是长命令,流式中入队返回 `"stopReason":"enqueued"`) |
| `abort` | 中断当前 run |
| `getState` | 模型、thinking 级别、消息数、pending 工具、队列深度、错误等快照 |
| `setModel` / `setThinkingLevel` | 运行期切换模型 / 思考级别 |
| `getMessages` / `getEntries` / `getTree` | 消息转录 / 会话 entries / 会话树查询 |
| `getCommands` / `bash` | 命令查询 / 裸 shell 执行 |
| `extensionUiResponse` | 扩展 UI 反向通道应答(select/confirm/input 的用户选择) |

prompt 等长命令异步执行,abort 等控制命令在 run 期间仍可送达。

---

## Provider 支持

`--provider <id> [--model <id>]`,API key 从环境变量读取,`--model` 缺省时用各 provider 的默认模型。当前接入两个通用适配器(OpenAI 风格 completions + Anthropic messages),覆盖以下 provider(其余 8 个 provider 适配器为**记录在案的推迟项**,通用适配器已覆盖主流):

| provider | 环境变量 |
|---|---|
| `anthropic` | `ANTHROPIC_API_KEY` |
| `openai` / `openai-codex` | `OPENAI_API_KEY` |
| `deepseek` | `DEEPSEEK_API_KEY` |
| `google` | `GOOGLE_API_KEY` |
| `groq` | `GROQ_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` |
| `zai` / `zai-coding-cn` | `ZAI_API_KEY` |
| `moonshotai` / `kimi-coding` | `MOONSHOT_API_KEY` |
| `xai`、`mistral`、`together`、`fireworks`、`cerebras`、`azure-openai-responses`、`minimax`、`xiaomi`、`radius` | 各自 `_API_KEY` |

可靠性(经重试装饰器,默认装配):可重试错误(429/5xx/断连)指数退避自动重试,配额类错误不重试;重试调度/结束以 `AutoRetryStart/End` 事件上报。

---

## 内置工具

8 个工具走统一的 ToolDefinition 五合一(schema + 执行 + 系统提示词贡献 + 渲染 + 约束采样);输出统一双限截断(**保留末尾** 2000 行 / 50KB,先到为准,截断时完整输出落临时文件并在结果里给出 `[Full output: <path>]` 提示);错误一律抛给循环转错误 tool result。

| 工具 | 默认集 | 说明 |
|---|---|---|
| `read` | ✅ | 读文件,1 起始 offset/limit 切片,截断时附续读提示;单行超 50KB 提示改用 bash |
| `bash` | ✅ | shell 执行,流式输出(onUpdate 实时可见),非零 exit 报"输出 + Command exited with code N",abort/超时杀整棵进程树 |
| `edit` | ✅ | 多点精确替换(每个 oldText 必须在原文件中唯一),BOM/行尾保持,diff + unified patch 进 details |
| `write` | ✅ | 整文件写入(新建或完整重写) |
| `grep` | — | 内容搜索(glob 过滤、ignoreCase/literal、context 行数,limit 默认 100),尊重 .gitignore,原生实现无外部 rg 依赖 |
| `find` | — | glob 文件查找(limit 默认 1000),尊重 .gitignore |
| `ls` | — | 目录列表(目录加 `/` 后缀,含 dotfiles,limit 默认 500) |
| `powershell` | — | bash 的 Windows 等价物,共用同一工厂 |

默认激活 read/bash/edit/write;参数经完整 JSON Schema 校验(类型/required/enum/数值范围/数组元素/嵌套对象),schema 本身非法时 fail-closed 拒绝执行。

### bash 工具高级配置

- **PI_\* 会话环境注入**:bash 执行时自动注入 `PI_SESSION_ID`、`PI_SESSION_FILE`、`PI_PROVIDER`、`PI_MODEL`、`PI_REASONING_LEVEL`(取自会话运行时快照);用户进程环境已有同名变量则**不覆盖**。
- **commandPrefix**:settings 里配置统一命令前缀(见下),以换行前置,可含多条 shell 语句;hook 先改写、prefix 最后前置。
- **spawnHook**:spawn 前的命令改写钩子(`trait ShellSpawnHook`),可检查/改写命令;返回 Err = 拒绝执行且**不 spawn**,错误信息进工具结果。
- **进程组杀灭**:Unix 下子进程自成进程组(`process_group(0)`),超时/中断时 `kill(-pgid)` 清理整棵进程树(含孙进程);非 Unix 保持 kill_on_drop 兜底。

---

## 会话持久化与压缩

- **JSONL append-only 会话树**:每条 entry(消息/模型切换/压缩/标签…)是一个树节点,只追加不改写历史;`--mode rpc` 的 `getTree`/`getEntries`/`fork` 可查询/分支。
- **投影**:活动分支投影成模型上下文;重启续聊。
- **自动 compaction**:上下文接近窗口上限时自动裁剪 + 摘要(切点选择、向前吞并相邻元数据 entry、usage 估算失真回退全量估算);每次 run 的 overflow 恢复只尝试一次。
- **overflow 自动恢复**:context overflow → 丢被中断 turn 的错误 assistant → 裁最老上下文 → continue 重放,以 `AutoRetryStart/End` 事件上报。

---

## 扩展系统(MCP)

在项目根 `.rpi/settings.json` 或全局 `~/.rpi/settings.json` 声明(项目优先):

```json
{
  "mcpServers": [
    {
      "name": "my-ext",
      "command": "node",
      "args": ["ext.js"],
      "env": { "FOO": "bar" }
    }
  ],
  "commandPrefix": "timeout 300"
}
```

- 扩展是**独立进程**,通讯复用 MCP(rmcp,pin `=3.4.1`);`name` 缺省取 command 文件名,同时用作扩展注册工具的名字前缀(如 `my-ext_tool`)。
- 扩展可以:**注册工具**(进全量候选集)、**订阅事件**(tool_call/tool_result 事件支持改参/拦截:返回 `block=true` 拦截、`args` 改参后循环重新过 schema 校验再执行)。
- **错误语义**:init 失败 = 跳过该扩展 + 收集诊断(打 stderr),一个坏扩展拖不死整体;运行期失败/超时/断连 = 诊断 + 跳过该次分发,绝不击穿宿主。
- **UI 反向通道**:扩展可调用 select/confirm/input/notify——TUI 模式弹真组件,json 模式落 `extension_notify` 事件,rpc 模式经 `extensionUiResponse` 上行等待宿主应答。
- `rpi --mcp-mock-server`:扩展开发用的自检 mock 服务端。

---

## 架构

7 个 crate、四层严格单向依赖(依赖图与接缝纪律见 `docs/10-implementation-policy.md`):

```
L4  rpi-cli (bin)  ──── rpi-tui ┐
L3  rpi-core ───────────────────┤  依赖下方全部
L2  rpi-agent ─── rpi-session ─┤  session/tools 只依赖 agent 的类型/trait
L1  rpi-ai ─────────────────────┘  零内部依赖
```

| crate | 职责 | 可拆卸性 |
|---|---|---|
| `rpi-ai` | Provider trait、适配器、事件流协议、流式重试、overflow 检测 | 基座 |
| `rpi-agent` | 主循环 + `Agent` + 消息/事件类型 + `LoopHooks`/`Tool` trait + 护栏 | 基座 |
| `rpi-session` | JSONL 会话树、投影、compaction | 可选组件 |
| `rpi-tools` | 内置 8 工具 | 可选组件 |
| `rpi-core` | `AgentSession`、系统提示词 sections、扩展 registry、模型解析 | 业务核 |
| `rpi-tui` | 差分渲染终端 UI | 可选组件 |
| `rpi-cli` | main + 四种模式 + 装配 | 可执行壳 |

六个稳定接缝:循环↔provider(`trait Provider`)、宿主↔循环(`trait LoopHooks`)、循环↔工具(`trait Tool`)、core↔扩展(`ExtensionActions`/`ExtensionUi`)、core↔mode(事件订阅)、事件汇(`Vec<Arc<dyn Subscriber>>` 串行 await 保序)。每个 crate 对外只暴露**工厂、trait、类型**三样;实现类型不 `pub`;依赖严格单向;无可变全局状态;trait 方法不 panic。

---

## 开发

```bash
cargo test --workspace                  # 27 个测试二进制全绿(含 property test)
cargo clippy --workspace --all-targets  # 零警告
```

- **改代码前必读**:`docs/README.md` 的阅读顺序(00 总览 → 03 循环 → 01 类型);`docs/10-implementation-policy.md` 是开发规则。
- **开发流程**:`.agents/skills/rpi-dev-workflow` 强制流程——读文档 → 编码+边界测试 → 全仓测试与 clippy 零警告 → 独立 reviewer 子代理审查 → 接缝签名变更在 10 §3 登记 → 功能文档追加踩坑记录。
- **外部依赖一律 pin 精确版本**(如 `rmcp =3.4.1`、`jsonschema =0.58.0`、`nix =0.31.3`、`proptest =1.9.0`)。
- 各模块的设计细节与踩坑史:`docs/02-ai-provider.md`(provider/retry/overflow)、`docs/03-agent-loop.md`(循环/Agent/注入)、`docs/04-coding-agent.md`(AgentSession)、`docs/05-tools.md`(工具)、`docs/06-session-compaction.md`(会话树)、`docs/07-extensions.md`(扩展)、`docs/08-modes-tui.md`(模式/TUI/RPC)。

### 明确不做(2026-09-26 决策记录)

OAuth 登录、其余 8 个 provider 适配器、模型目录 models-store;OpenAI 侧冷门细节(tool result 图片转发、grammar/custom 工具、reasoning_details 回放);read 读图、fork 跨文件复制历史、TUI 双屏/图片、RPC 剩余约 18 个命令、实验性 CBOR 栈;时间戳用毫秒整数、压缩单请求(不追求与 pi 会话文件逐字节兼容)。
