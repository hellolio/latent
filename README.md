# latent

> 终端里的 AI 编码 agent：在命令行中与 AI 对话，让它读写文件、执行命令、搜索代码，直接帮你完成开发任务。

latent 以流式方式驱动大语言模型，并行执行工具调用，支持会话持久化、自动上下文压缩、MCP 扩展、子代理与技能系统，并提供权限与沙箱护栏。

---

## 功能特性

- **终端交互界面**：全屏 / 滚动两种渲染模式，Markdown 渲染、语法高亮、流式输出回复与思考过程、工具调用实时可见。
- **内置工具**：`read` / `bash` / `edit` / `write` / `grep` / `find` / `ls` / `powershell` 八个工具，AI 可以直接读代码、跑命令、改文件。
- **多 Provider 接入**：支持 Anthropic、OpenAI、DeepSeek、Google、Z.ai、Moonshot 等 17 家 provider，统一从环境变量读取 API key，也支持自定义模型接入。
- **会话管理**：对话自动保存为 JSONL 会话文件，`--continue` 一键续聊；上下文接近模型窗口上限时自动压缩摘要，长任务不中断。
- **权限模式**：Plan（只读规划）/ Confirm（写操作需确认）/ FullAccess（全自动）三种模式，配合可选的 macOS Seatbelt / Linux bubblewrap / Landlock 沙箱。
- **扩展系统**：通过 MCP 协议接入外部扩展进程，扩展可以注册新工具、订阅事件、弹出交互 UI。
- **子代理与技能**：`.latent/agents/*.md` 定义子代理（独立系统提示词并行干活），`.latent/skills/*/SKILL.md` 定义可按需加载的技能。
- **网页搜索与抓取**：内置 `web_search` / `fetch_content` 等四个联网工具，支持 Brave / Exa / Tavily / SearXNG / DuckDuckGo 多引擎路由，配置任一 key 即可用，没 key 也有免费引擎兜底。
- **四种运行模式**：interactive（TUI）/ print（单次执行）/ json（事件流）/ rpc（编辑器集成），共享同一业务核。

---

## 安装

### 从源码构建（需要 Rust 1.85+）

```bash
git clone https://github.com/hellolio/latent.git
cd latent
cargo build --release -p latent-cli
# 产物在 target/release/latent，可复制到 PATH：
cp target/release/latent /usr/local/bin/
```

> 没有 API key 也能先体验：`cargo run -p latent-cli -- --mock "你好"` 会用内置 mock provider 走通全链路。

---

## 快速上手

```bash
# 1. 设置 API key（以 Anthropic 为例）
export ANTHROPIC_API_KEY=sk-ant-…

# 2. 直接提问（在交互终端会自动进入 TUI）
latent "列出当前目录结构"

# 3. 指定 provider 和模型
latent --provider anthropic --model claude-sonnet-4-5 "修复这个 bug"

# 4. 交互模式
latent

# 5. 续聊上次会话
latent --continue
```

也可以在项目根目录的 `.latent/settings.json` 里配置默认 provider/model，之后无需每次传参：

```json
{
  "defaultProvider": "anthropic",
  "defaultModel": "claude-sonnet-4-5"
}
```

帮助：`latent --help`。

---

## 使用方式

### 命令行参数

| 参数 | 说明 |
|---|---|
| `"prompt"` | 位置参数，直接给出任务；非交互环境下自动进入 print 模式 |
| `--provider <id>` | 指定 provider（如 `anthropic`、`openai`、`deepseek`） |
| `--model <id>` | 指定模型；也可写 `provider/model` 形式省略 `--provider` |
| `--mode <m>` | `interactive` / `print` / `json` / `rpc`（不指定时自动判定） |
| `--tui-mode <m>` | `fullscreen`（默认，alternate screen）/ `regular`（滚入终端 scrollback） |
| `--continue` / `-c` / `-r [序号]` | 续聊当前项目最近的会话（序号对应 `-l` 列表） |
| `--list` / `-l` | 列出当前项目的历史会话 |
| `--session-mode <m>` | `plan` / `confirm` / `full-access` |
| `--plan` | 等价于 `--session-mode plan`（只读规划模式） |
| `--yolo` | 等价于 `--session-mode full-access`（全自动，慎用） |
| `--theme <t>` | 指定主题（Tokyo Night / Catppuccin / Dracula 等） |
| `--sandbox-write <dir>` | 附加沙箱可写目录 |
| `--sandbox-network` | 允许沙箱内联网 |
| `--mock` | 使用内置 mock provider（无需 API key，测试用） |

### 四种运行模式

- **interactive**：终端 TUI 聊天（默认，两端都是 TTY 时自动进入）。流式渲染回复与思考过程、工具卡片实时显示、footer 状态栏展示模型 / context 占用 / 花费。
- **print**：单次执行，流式输出到 stdout 后退出。适合脚本与管道：`cat prompt.txt | latent`。
- **json**：事件逐行 JSONL 输出，适合被程序消费：`latent --mode json "…" > events.jsonl`。
- **rpc**：stdio JSONL 协议，prompt/steer/abort/getState/setModel 等命令集，适合编辑器/IDE 集成。

### 交互模式常用操作

| 按键 / 命令 | 说明 |
|---|---|
| `Enter` | 发送；`Shift+Enter` / `Ctrl+J` 换行 |
| `@文件名` | 文件引用：输入 `@` 弹出文件/目录候选，选中补全为文件**全路径**（手输的路径则原样发出）；`@路径` 随消息发给模型，内容由模型按需读取 |
| `Ctrl+C` | 中断当前任务；空闲时 500ms 内双击退出 |
| `Ctrl+D` | 退出 |
| `Shift+Tab` | 循环切换权限模式（plan → confirm → full-access） |
| `!命令` | 直接执行 shell 命令（不经过模型） |
| `/help` | 查看帮助 |
| `/model` | 切换模型 / 添加自定义模型 |
| `/thinking [level]` | 调整思考级别 |
| `/compact` | 手动压缩上下文 |
| `/session` | 查看 / 切换会话 |
| `/fullscreen [on\|off]` | 切换全屏渲染模式 |
| `/setting` | 打开设置（主题、全屏、复制行为等，写回 settings.json） |
| `/subagent` | 打开子代理选择器 |
| `/quit` | 退出 |

全屏模式下 `PageUp` / `PageDown` / `Home` / `End` / 鼠标滚轮翻阅历史；鼠标拖选文字后 `Ctrl+X` 复制到系统剪贴板。

### 权限模式

| 模式 | 行为 |
|---|---|
| **Plan**（默认） | AI 只能读，不能写文件 / 执行有副作用的命令，适合先让它出方案 |
| **Confirm** | 写操作逐条弹窗确认，可对单个命令选「本会话不再询问」 |
| **FullAccess** | 全自动执行，不再确认（`--yolo`） |

---

## 配置

配置目录按「项目 `.latent/` 优先，逐字段覆盖全局数据目录」合并。全局数据目录默认 `~/.config/latent`，可用环境变量 `LATENT_HOME` 指定（旧版 `~/.latent` 存在时自动沿用并提示）：

| 文件 | 作用 |
|---|---|
| `.latent/settings.json` | 默认模型、权限模式、bash 超时、MCP 扩展声明（`mcpServers`）、主题等 |
| `.latent/models.json` | 自定义 provider / model（baseUrl、定价、兼容开关） |
| `.latent/skills/<name>/SKILL.md` | 技能定义，AI 通过 `load_skill` 工具按需加载 |
| `.latent/agents/<name>.md` | 子代理定义（frontmatter 声明 name/model/tools，正文即系统提示词） |
| `.latent/system-prompt.md` | 自定义系统提示词 |

MCP 扩展示例（`.latent/settings.json`）：

```json
{
  "mcpServers": [
    {
      "name": "my-ext",
      "command": "node",
      "args": ["ext.js"],
      "env": { "FOO": "bar" }
    }
  ]
}
```

扩展以独立进程运行，崩溃或出错只会被跳过并打印诊断，不影响 latent 本体。

---

## Provider 支持

API key 从环境变量读取，`--model` 缺省时使用各 provider 默认模型：

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
| `xai`、`mistral`、`together`、`fireworks`、`cerebras`、`azure-openai-responses`、`minimax`、`xiaomi` 等 | 各自 `_API_KEY` |

网络错误（429 / 5xx / 断连）会指数退避自动重试；配额类错误不重试。

---

## 内置工具

| 工具 | 默认启用 | 说明 |
|---|---|---|
| `read` | ✅ | 读文件（支持 offset/limit 切片） |
| `bash` | ✅ | 执行 shell 命令，流式输出，超时自动清理整棵进程树 |
| `edit` | ✅ | 多点精确文本替换 |
| `write` | ✅ | 写入整个文件（自动创建父目录） |
| `grep` | — | 内容搜索，尊重 .gitignore |
| `find` | — | glob 文件查找 |
| `ls` | — | 目录列表 |
| `powershell` | — | Windows 下的 bash 等价物 |

默认启用 `read` / `bash` / `edit` / `write`；可在 settings.json 的 `tools` 字段调整激活集合。

---

## 开发

```bash
cargo clean && cargo build --release -p latent && cp target/release/latent ~/.local/bin/
cargo build --release -p latent-cli         # 构建
cargo test --workspace                   # 运行全部测试
cargo clippy --workspace --all-targets   # lint（要求零警告）
```

### todo list
 - [x] .latentignore文件独立（gitignore 语法，全局数据目录 `.latentignore` + 项目 `.latentignore`，项目优先可反选）
 - [ ] 子agent调用和显示优化
 - [ ] harness适配微信qq，如何保证长时间工作不中断，定时任务
 - [ ] jev决策小模型引入
 - [ ] 文件检索如何过滤噪音（启用小模型摘要？或者引入第三方库实现？阿里rg？）
 - [x] mac沙箱好像不生效
 - [ ] 无效模型清理
 - [ ] 实现一个扩展用于测试扩展功能（文件搜索加强？）
 - [ ] 实现可配置追加系统提示词（当前仅可替换）
 - [ ] ui颜色调整优化
 - [x] @符号添加文件到上下文
 - [x] 当前全屏模式下，如果模型正在输出，我滚动屏幕，正在输出的部分不会跟着滚动，但是我需要输入框以上的部分全部跟随滚动

架构设计、目录索引与开发规范见 [AGENTS.md](AGENTS.md)；E2E 测试说明见 [tests/e2e](tests/e2e)。

---

## License

MIT
