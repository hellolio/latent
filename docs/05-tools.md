# 05 — 内置工具详解

> **一句话**:8 个内置工具(read/bash/powershell/edit/write/grep/find/ls),默认只装 read/bash/edit/write;全部走 ToolDefinition 五合一;输出统一双限截断(2000 行 / 50KB,先到为准),渲染器与执行分离;每个工具的文件操作后端(`Operations` 接口)可替换以支持 SSH/沙箱。

## 1. 工具清单与默认集(`src/core/tools/index.ts`)

- `ToolName`(@ index.ts:95)= `"read" | "bash" | "powershell" | "edit" | "write" | "grep" | "find" | "ls"`。
- 默认装 read/bash/edit/write(`createCodingToolDefinitions` @ index.ts:164-171);只读组合 `createReadOnlyToolDefinitions`(@173-180,read/grep/find/ls);全量 `createAllToolDefinitions`(@182-193)。
- 每个 `xxx.ts` 导出两套:`createXxxToolDefinition`(ToolDefinition,带渲染器)与 `createXxxTool`(AgentTool,包装去 UI)。

## 2. 统一截断(`src/core/tools/truncate.ts`,276 行)

```ts
const DEFAULT_MAX_LINES = 2000;         // @ truncate.ts:11
const DEFAULT_MAX_BYTES = 50 * 1024;    // @ truncate.ts:12
const GREP_MAX_LINE_LENGTH = 500;       // @ truncate.ts:13
```

`TruncationResult`(@ truncate.ts:17-40):`content/truncated/truncatedBy("lines"|"bytes")/totalLines/totalBytes/outputLines/outputBytes/lastLinePartial/firstLineExceedsLimit/maxLines/maxBytes`。**永不返回半行**(bash tail 截断边界除外)。函数:`truncateHead`(read 用,保留头部)/ `truncateTail`(bash 用,保留尾部)/ `truncateLine`。

## 3. read(`read.ts`,203 行)

- 参数(@ read.ts:14-18):`path`(必需)、`offset?`(1 起始行号)、`limit?`。
- 行为:
  - 图片(jpg/png/gif/webp/bmp)→ 读二进制,按模型 `inputLimits.images.resize` 自动缩放(`processImage`),返回 text note + ImageContent;非视觉模型附加提示 "image will be omitted"(@59-64);
  - 文本:先按 offset/limit 切片,再 `truncateHead` 双限截断;截断时追加 `[Showing lines X-Y of N. Use offset=N+1 to continue.]` 续读提示(:163-172);
  - 单行超 50KB → 提示改用 bash:`sed -n 'Np' path | head -c 51200`(:158-162);
  - offset 越界 → 抛错;用户 limit 提前结束且还有剩余 → 提示剩余行数(:174-178)。
- `ReadOperations`(@35-42):`readFile`/`access`/`detectImageMimeType` 可替换。
- details:`{truncation?}`。

## 4. bash(`bash.ts`,408 行)

- 参数(@ bash.ts:38-41):`command`、`timeout?`(秒;无默认超时,上限 `MAX_TIMEOUT_MS=2_147_483_647` ms @22)。
- 执行(`createLocalShellOperations` @81-150):
  - `spawn(shell, args, {detached: process.platform !== "win32"})` —— **进程组隔离**,abort/超时走 `killProcessTree`(杀整棵树);
  - stdout/stderr 都流入 `onData`;
  - 信号杀死无 exit code 时按 shell 惯例换算 `128 + signal`(:141-142);
  - shell 命令可经 stdin 传输(`commandTransport === "stdin"`)。
- 输出(`OutputAccumulator` @ `tools/output-accumulator.ts`,222 行):流式聚合,`truncateTail` **保留末尾** 2000 行/50KB;截断时完整输出落临时文件 `details.fullOutputPath`(前缀 `pi-bash`);提示 `[Showing lines X-Y of N. Full output: <tmpfile>]`;
- onUpdate 节流:`BASH_UPDATE_THROTTLE_MS`(`renderers/bash.ts`),每 tick 发 `tool_execution_update` 让 TUI 实时显示;
- 环境:`PI_SESSION_ID/PI_SESSION_FILE/PI_PROVIDER/PI_MODEL/PI_REASONING_LEVEL` 按 `exposeSessionEnvironment` 注入(@183-193);`spawnHook` 可改写 command/cwd/env;`commandPrefix` 前置;
- 非零 exit → 抛错,错误消息 = 输出 + `Command exited with code N`(appendStatus @341);abort → `Command aborted`;timeout → `Command timed out after N seconds`;
- `BashOperations.exec`(@59-78)接口可替换(SSH/沙箱/扩展 user_bash 劫持)。
- details:`{truncation?, fullOutputPath?}`。

### powershell(`powershell.ts`,67 行)

bash 的 Windows 等价物:同一 `createShellToolDefinition` 工厂(@ bash.ts:227),只换 `ShellToolConfig`(name/prompt/`$`)与本地 PowerShell 操作。

## 5. edit(`edit.ts`,220 行)

- 参数(@ edit.ts:21-41):`path`、`edits[]: {oldText, newText}`。
- **多点精确替换**规则(description 与 promptGuidelines 反复强调):
  - 每个 `oldText` 必须在**原始文件**中唯一且互不重叠(所有编辑都针对原文件匹配,非增量);
  - 附近多处修改应合并进一次调用的多个 edits;
  - `oldText` 尽量小但唯一,不要垫大段未变内容。
- `prepareEditArguments`(@103-134)兼容垫片:Opus 4.6/GLM-5.1 会把 edits 发成 JSON 字符串、或发单个 edit 对象、或用旧版顶层 oldText/newText —— 全部归一成 `edits[]`。
- 执行流程(@159-213):`withFileMutationQueue`(按路径串行化,`file-mutation-queue.ts` 61 行)→ 读文件 → `splitBom` 剥 BOM(模型不会在 oldText 里带 BOM)→ `detectLineEnding` + `normalizeToLF` → `applyEditsToNormalizedContent`(`edit-diff.ts` 556 行:唯一性/重叠校验)→ 写回时**恢复原 BOM 与原行尾** → details 返回 `{diff, patch, firstChangedLine}`(display diff + unified patch,来自 `generateDiffString`/`generateUnifiedPatch`)。
- abort 处理细节:@164-170 注释 —— 不用 abort listener 拒绝,而是在每个 await 后检查 `signal.aborted`,避免释放 mutation queue 时 in-flight 文件操作还没落地。
- `renderShell: "self"`:TUI 里用自绘 diff 组件。

## 6. write(`write.ts`,97 行)

- 参数(@ write.ts:8-11):`path`、`content`。整文件写入(新建或完整重写);guideline:"Use write only for new files or complete rewrites."。

## 7. grep(`grep.ts`,323 行)

- 参数(@ grep.ts:20-30):`pattern`、`path?`、`glob?`(如 `*.ts`)、`ignoreCase?`、`literal?`(按字面而非正则)、`context?`(前后文行数)、`limit?`(**默认 100**)。
- 尊重 .gitignore;匹配行超长截到 `GREP_MAX_LINE_LENGTH=500` 字符;details:`{truncation?, matchLimitReached?, linesTruncated?}`。

## 8. find(`find.ts`,318 行)

- 参数(@ find.ts:26-30):`pattern`(glob,如 `**/*.spec.ts`)、`path?`、`limit?`(**默认 1000**)。
- glob 文件查找,尊重 .gitignore;目录匹配保留尾 `/`;details:`{truncation?, resultLimitReached?}`。

## 9. ls(`ls.ts`,175 行)

- 参数(@ ls.ts:10-13):`path?`、`limit?`(**默认 500**)。
- 目录列表,目录加 `/` 后缀,含 dotfiles;details:`{truncation?, entryLimitReached?}`。

## 10. 横切约定

| 约定 | 说明 |
|---|---|
| `constrainedSampling` | 全部内置工具 `{type:"json_schema", strict:"prefer"}` |
| prompt 贡献 | 每个工具的 `xxxToolSystemPromptContribution`(`snippet` + `guidelines`)进系统提示词(见 04 文档 §3.1) |
| `Operations` 后端接口 | read/basb/edit/write 均有可替换的 IO 后端(SSH/沙箱) |
| 渲染器分离 | `tools/renderers/`(bash 171/edit 238/find 77/grep 82/ls 70/read 175/write 181 行)只管 TUI;执行层不知道渲染 |
| `render-utils.ts`(85)/`path-utils.ts`(118) | 路径解析(`resolveToCwd`/`resolveReadPathAsync`)与渲染辅助 |
| 错误表达 | 一律抛异常(由循环转为错误 tool result),details 只放成功数据 |

## 11. 源码文件索引

| 文件 | 行数 | 职责 | 关键符号 | 优先级 |
|---|---|---|---|---|
| `packages/coding-agent/src/core/tools/index.ts` | 224 | 工具清单与工厂 | `ToolName`:95, `createToolDefinition`:118, `createCodingToolDefinitions`:164, `createAllToolDefinitions`:182 | P0 |
| `packages/coding-agent/src/core/tools/truncate.ts` | 276 | 统一截断 | 常量:11-13, `TruncationResult`:17, `truncateHead`/`truncateTail`/`truncateLine` | P0 |
| `packages/coding-agent/src/core/tools/bash.ts` | 408 | shell 执行 | schema:38, `createLocalShellOperations`:81, `createShellToolDefinition`:227 | P0 |
| `packages/coding-agent/src/core/tools/read.ts` | 203 | 读文件/图片 | schema:14, `createReadToolDefinition`:66 | P0 |
| `packages/coding-agent/src/core/tools/edit.ts` | 220 | 多点精确替换 | schema:21-41, `prepareEditArguments`:103, `createEditToolDefinition`:143 | P0 |
| `packages/coding-agent/src/core/tools/edit-diff.ts` | 556 | diff/patch 与编辑应用 | `applyEditsToNormalizedContent`, `normalizeToLF`, `detectLineEnding`, `generateDiffString`, `generateUnifiedPatch` | P0 |
| `packages/coding-agent/src/core/tools/output-accumulator.ts` | 222 | 流式输出聚合 + spill | `OutputAccumulator` | P1 |
| `packages/coding-agent/src/core/tools/file-mutation-queue.ts` | 61 | 按路径串行化 | `withFileMutationQueue` | P1 |
| `packages/coding-agent/src/core/tools/write.ts` | 97 | 写文件 | schema:8, `createWriteToolDefinition` | P1 |
| `packages/coding-agent/src/core/tools/grep.ts` | 323 | 内容搜索 | schema:20, DEFAULT_LIMIT=100 | P1 |
| `packages/coding-agent/src/core/tools/find.ts` | 318 | glob 查找 | schema:26, DEFAULT_LIMIT=1000 | P1 |
| `packages/coding-agent/src/core/tools/ls.ts` | 175 | 目录列表 | schema:10, DEFAULT_LIMIT=500 | P1 |
| `packages/coding-agent/src/core/tools/powershell.ts` | 67 | Windows shell | `createPowerShellToolDefinition` | P2 |
| `packages/coding-agent/src/core/tools/path-utils.ts` | 118 | 路径解析 | `resolveToCwd`, `resolveReadPathAsync` | P1 |
| `packages/coding-agent/src/core/tools/render-utils.ts` | 85 | 渲染辅助 | — | P2 |
| `packages/coding-agent/src/core/tools/renderers/*.ts` | 63-238 各 | TUI 渲染 | `renderers/bash.ts`(171, 含 `BASH_UPDATE_THROTTLE_MS`)、`renderers/edit.ts`(238, diff 渲染) | P2 |

阅读顺序:`index.ts` → `truncate.ts` → `read.ts` → `bash.ts`(+`output-accumulator.ts`)→ `edit.ts`(+`edit-diff.ts`)→ grep/find/ls/write → `renderers/`。

## 踩坑记录

- **2026-09-25 `ignore` crate 的 override 白名单会把父目录也当匹配结果带进 find 输出**:现象是 `*.zig` 这类 pattern 会多输出 `src/`、`src/nested/`。原因:walker 级 overrides 对目录返回 Whitelist(放行下降),目录 entry 本身也被产出。解法:override 不交给 walker,改为逐 entry 调 `overrides.matched(path, is_dir)` 只保留 Whitelist。(关联文件:`crates/rpi-tools/src/find.rs`)
- **2026-09-25 非 git 目录里 .gitignore 不生效**:现象是临时目录测试中 `node_modules/` 未被排除。原因:`ignore` crate 默认 `require_git(true)`,仓库外不应用 .gitignore。解法:grep/find 均 `require_git(false)`(等价 pi 里 fd 的 `--no-require-git`);另因关闭了 hidden 过滤,`.git` 目录需显式 `filter_entry` 排除。(关联文件:`crates/rpi-tools/src/grep.rs`、`find.rs`)
- **2026-09-25 截断层字符/字节混用突破 50KB 硬上限**:现象是单行超限时 `chars().take(max_bytes)` 实际按字符数截,多字节行输出可达 ~4×上限;且 `truncatedBy` 恒报 `"lines"`,字节限先触发时也误报。解法:字节截断在 `is_char_boundary` 上对齐;`truncated_by` 在循环内区分 lines/bytes 触发源。(关联文件:`crates/rpi-tools/src/truncate.rs`)
- **2026-09-25 bash 超时路径丢弃已捕获输出**:现象是 `sleep 30` 超时后错误只有一句 "Command timed out",长输出全部丢失。原因:`select!` 的 timeout 分支直接返回 Err,未走 `finish()/snapshot()`。解法:timeout 分支先聚合快照(`snapshot(true)` 落盘),按 appendStatus 组装"输出 + 状态 + Full output 提示"进错误。(关联文件:`crates/rpi-tools/src/bash.rs`)
- **2026-09-25 有意保留的取舍**:①pi 的 grep/find 依赖外部 rg/fd 二进制(ensureTool 下载),rpi 用 `ignore` crate 原生实现,语义对齐但无外部依赖;②进程组隔离简化为 `kill_on_drop` 直接杀子进程,孙进程清理待引入 libc/nix(bash.rs 头部注释);③`PI_*` 会话环境变量注入、`spawnHook`/`commandPrefix` 依赖会话上下文与扩展管线,M5+ 补;④read 的图片读取依赖多模态模型参数,仍按 M4 前的取舍延后。
- **2026-09-26 T11 收尾：②取舍作废（进程组杀灭落地）**：rpi-tools 加 `nix =0.31.3`（仅 `cfg(unix)` target 依赖），spawn 前 `Command::process_group(0)` 使子进程自成组长（pgid = 子 pid），超时/中止分支先 `kill(-pgid, SIGKILL)` 清整棵树再 `wait()` 收尸，ESRCH（进程已死）静默忽略；`kill_on_drop(true)` 保留兜底；非 Unix 保持现状。坑：select 的 timeout/cancel 分支里不能直接 `&mut child`——`work` future（内含 `child.wait()`）对 child 的借用要到内层作用域结束才释放，故 select 连同 work 收进内层块，kill 放在块外。（关联文件：`crates/rpi-tools/src/bash.rs`）
- **2026-09-26 T9/T10 收尾：③取舍作废**：bash 执行时注入 `PI_SESSION_ID/PI_SESSION_FILE/PI_PROVIDER/PI_MODEL/PI_REASONING_LEVEL`（装配期 `Weak<AgentSession>` 共享 cell，session 建好后回填，工具执行时按需快照；空 cell = 行为同未配置）；`ShellSpawnOptions{session_env, command_prefix, spawn_hook}` 经工厂参数进工具（无可变全局状态）。**与 pi 的有意偏差**：pi 的 bash.ts 先 delete 继承的 PI_* 再无条件写入会话权威值（总是覆盖）；rpi 按 11 计划决策"用户进程环境已有同名变量则不覆盖"实现——用户 shell 残留旧 PI_* 时工具会拿到陈旧值，此为拍板取舍非 bug，勿反复提出。spawnHook/commandPrefix 语义：hook 先改写（检查的是用户原始命令）、prefix 最后以**换行**前置（pi 用换行，prefix 可为多条 shell setup 语句）；hook 返回 Err = 拒绝执行且不 spawn（错误文案含 hook 信息，与 07 §8.5 扩展错误语义一致）。commandPrefix 来自 settings（项目 `.rpi/settings.json` 优先于全局，空白跳过）。（关联文件：`crates/rpi-tools/src/bash.rs`、`crates/rpi-tools/src/lib.rs`、`crates/rpi-cli/src/assembly.rs`）
- **2026-09-27 settings `tools` 键控制激活工具集**：settings（项目 `.rpi/settings.json` 优先于全局，首个配置生效）的 `tools` 为名字列表（如 `["bash"]`）：**键未配置 = 全部激活（现状不变）；配置了空数组 `[]`（或全空白条目）= 显式不激活任何工具**；条目去首尾空白。配置了候选工具（内置 + 扩展）之外的名字直接**装配报错**并列出可用名单（配置错误显式暴露，不静默降级）。settings 解析在 CLI 入口（main）一次完成、经 `SessionSettings` 显式传入装配层，`build_session` 不读用户配置文件（测试不依赖本机配置）。激活子集决定 API 请求的 JSON schema 声明、session system baseline 的 toolsAdded 与内置提示词 `<tools>` 节（Forced 外置提示词时后者由用户文件自管）。测试/嵌入方可经 `BuildOptions.active_tools: Option<Vec<String>>` 显式指定。与 pi 的偏差：pi 无此 settings 键（工具开关走运行时扩展），rpi 在装配期一次性收窄、会话中途不变。（关联文件：`crates/rpi-cli/src/assembly.rs`）
