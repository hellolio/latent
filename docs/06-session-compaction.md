# 06 — 会话持久化与压缩

> **一句话**:会话 = append-only 的 JSONL 文件,每个 entry 带 `id/parentId` 构成树;分支 = 移动 leaf 指针继续追加;compaction/context_edit 都是追加 entry 而非改写;模型上下文由"leaf→root 回溯 + compaction 虚拟展开 + context_edit 投影"三算法重建;压缩切点绝不在 toolResult 处。

## 1. 存储格式(`src/core/session-manager.ts`,2010 行)

### 1.1 文件与位置

- `CURRENT_SESSION_VERSION = 3`(@ session-manager.ts:41);版本迁移在内存做(v1→v2 加 id/parentId 树,v2→v3 hookMessage 改名 custom,@287-331)。
- 文件首行 `SessionHeader`(@43-50):`{type:"session", version, id(uuidv7), timestamp, cwd, parentSession?}`。
- 存放路径:`~/.pi/agent/sessions/--<编码后的 cwd>--/*.jsonl`;编码规则:`/` 与 `:` 替换为 `-`,前后加 `--`(`getDefaultSessionDirPath` @589-594)。
- 写入:`appendFileSync` 同步追加(`SessionManager` 类 @987);`_buildIndex` 建 byId Map;`getTree()` 返回防御性拷贝的 `SessionTreeNode[]`(@200-207,含 resolved label)。

### 1.2 entry 树(每个 entry:`{type, id(8位hex), parentId, timestamp}`)

`SessionEntry` 联合(@183-194),共 11 种:

| entry 类型 | 行号 | 字段要点 | 进模型上下文? |
|---|---|---|---|
| `message` | @64-67 | `message: AgentMessage` | 是(经 convertToLlm) |
| `thinking_level_change` | @69-72 | `thinkingLevel` | 设置态 |
| `model_change` | @74-78 | `provider, modelId` | 设置态 |
| `usage` | @80-89 | `kind`(如 "cache_warm"), `provider, model, usage, note?` | 否(核算用) |
| `compaction` | @91-104 | `summary, firstKeptEntryId, tokensBefore, details?, usage?, fromHook?, **systemMessage?**(完整 prompt 与工具状态快照)` | 以摘要形式 |
| `branch_summary` | @106-116 | `fromId, summary, details?, usage?, fromHook?` | 是(摘要消息) |
| `custom` | @128-132 | `customType, data?` —— **扩展持久状态,不进 LLM 上下文** | 否 |
| `custom_message` | @159-165 | `customType, content, details?, display` —— 扩展注入 LLM 上下文的消息 | 是(变 user) |
| `context_edit` | @175-180 | `targetId, replacement: {content} \| null` —— **append-only 上下文修改**,null=从上下文剔除 | 投影规则 |
| `label` | @135-139 | `targetId, label?` —— 书签 | 否 |
| `session_info` | @142-145 | `name?` —— 会话名 | 否 |

### 1.3 分支与 fork

- `branch(branchFromId)`(@1572):把 leafId 移到树中较早节点继续 append —— **同一文件内分叉**,零拷贝;
- `branchWithSummary`(@1593):导航离开分支时生成 `branch_summary` entry;
- `createBranchedSession`(@1625)/fork:复制选定历史到新文件(带 `parentSession` 指针);
- 扩展暴露的只读视图:`ReadonlySessionManager`(@245,Pick 类型)。

## 2. 上下文重建三算法

```
SessionEntry[](整棵树)
   │  ① buildContextEntries(@476-512):沿 leaf→root 回溯得路径 path;
   │     找 path 上最新的 compaction;输出 = [compaction, firstKeptEntryId 起的被保留项
   │     (剔除其中的 system 消息), compaction 之后的项];无 compaction 则原样 path
   ▼
contextEntries
   │  ② buildSessionProjection(@543-573):收集路径上的 context_edit(按 targetId 建 Map);
   │     每个 entry 经 projectContextEntry(@519-540)投影:
   │       replacement=null → 消息剔除;
   │       replacement.content → 只替换 content(assistant/toolResult 的字符串内容包成 text 块);
   │     **只投影 index==0 的 compaction**(多个 compaction 时旧的忽略,@558-565);
   │     同时从设置态 entry 提取 thinkingLevel/model
   ▼
SessionProjection{entries, messages, thinkingLevel, model}
   │  ③ buildSessionContext(@576-583):取 messages + 设置
   ▼
{messages, thinkingLevel, model}  →  agent prompt 的初始上下文
```

注意:失败的 assistant 消息(`stopReason` 为 error/aborted/deferred)是否过滤,由 `sessionEntryToContextMessages` 决定(coding-agent 世界保留原文;**harness 世界明确过滤**,见 08 文档附注)。

## 3. Compaction(`src/core/compaction/compaction.ts`,1141 行)

### 3.1 触发与设置

```ts
interface CompactionSettings { enabled; reserveTokens; keepRecentTokens; }   // @ compaction.ts:142
DEFAULT = { enabled: true, reserveTokens: 16384, keepRecentTokens: 20000 }   // @ compaction.ts:148-152
shouldCompact(contextTokens, contextWindow, settings) =                       // @ compaction.ts:289-292
  contextTokens > contextWindow - reserveTokens
```

### 3.2 token 估算

- 优先真实数据:`calculateContextTokens(usage)`(@162)= `totalTokens || 四项相加`;取最近一条**有效**(非 aborted/error、非全零)assistant usage;
- 其后的消息用 chars/4 启发式(`estimateTokens` @320-371,保守高估):text/thinking 按字符数,toolCall 按 `name + JSON(arguments)`,**image 每张按 `ESTIMATED_IMAGE_CHARS = 4800`**(@298);
- `estimateContextTokens`(@218-246)= usageTokens + trailing(其后消息逐条估算);
- `estimateProjectedContextTokens`(@249-284):context_edit/compaction 之后旧 usage 失真 → 放弃 usage,全量估算。

### 3.3 切点算法(`findCutPoint`,@468-523)

- 有效切点 = user/assistant/bashExecution/custom/branchSummary/compactionSummary 消息;**绝不在 toolResult 处切**(`isCutPointMessage` @373-386)—— 切在带工具调用的 assistant 后时,其 tool results 随后且保留;
- 从最新往回累积估算 token,达 `keepRecentTokens` 停;取**不早于当前位置的最近有效切点**(@494-500);
- 再向前吞并相邻元数据 entry(不产生上下文消息的)(@504-511);
- 若切点落在 turn 中间(`isSplitTurn`,@513-522),记录 `turnStartIndex`(该 turn 的 user 消息,`findTurnStartIndex` @434-441)供摘要请求分割。

### 3.4 摘要

- 对话序列化 `serializeConversation`(`compaction/utils.ts:109`):`[User]: ... / [Assistant thinking]: ... / [Assistant tool calls]: name(args) / [Tool result]: ...`(tool result 截 2000 字符);
- 固定模板 `SUMMARIZATION_PROMPT`(@529-560):**Goal / Constraints & Preferences / Progress(Done/In Progress/Blocked)/ Key Decisions / Next Steps / Critical Context**,要求保留精确文件路径/函数名/错误消息;
- 增量更新 `UPDATE_SUMMARIZATION_PROMPT`(@562-601):合并旧摘要(保信息、进度归位、只更新变化);
- 摘要调用 `completeSummarization`:`cacheRetention:"none"` + 重试包装;**`stopReason === "length"` 的摘要不可入库**(`getSummarizationFailure` @607-609);
- 分割 turn 时 history 与 prefix 两次 LLM 请求;
- `CompactionEntry.details` 存 `readFiles/modifiedFiles` 文件清单(从工具调用提取,`compaction/utils.ts:29`),供下次摘要延续;
- `CompactionEntry.systemMessage` 携带压缩边界处的完整 prompt + 工具状态快照(下一轮上下文从这里恢复);
- 分支摘要:`compaction/branch-summarization.ts`(382 行)。

## 4. 与 harness 世界 compaction 的差异

`packages/agent/src/harness/compaction/compaction.ts`(865 行)是 harness 持久化世界的另一套(设置同构:`reserveTokens=16384`/`keepRecentTokens=20000`;切点同为"不在 toolResult 切")。**coding-agent 用的是自己 core/ 下这套**;重写时二选一即可,算法本质相同。

## 5. 源码文件索引

| 文件 | 行数 | 职责 | 关键符号 | 优先级 |
|---|---|---|---|---|
| `packages/coding-agent/src/core/session-manager.ts` | 2010 | **会话树全部** | `CURRENT_SESSION_VERSION`:41, `SessionHeader`:43, entry 类型:64-194, `SessionTreeNode`:200, `buildContextEntries`:476, `buildSessionProjection`:543, `buildSessionContext`:576, `getDefaultSessionDirPath`:589, `SessionManager`:987, `branch`:1572, `branchWithSummary`:1593, `createBranchedSession`:1625 | P0 |
| `packages/coding-agent/src/core/compaction/compaction.ts` | 1141 | **压缩全部** | `CompactionSettings`:142, DEFAULT:148, `calculateContextTokens`:162, `estimateContextTokens`:218, `shouldCompact`:289, `estimateTokens`:320, `isCutPointMessage`:373, `findTurnStartIndex`:434, `findCutPoint`:468, `SUMMARIZATION_PROMPT`:529, `getSummarizationFailure`:607 | P0 |
| `packages/coding-agent/src/core/compaction/utils.ts` | 158 | 序列化与文件清单 | `serializeConversation`:109, `computeFileLists`:62, `formatFileOperations`:72 | P0 |
| `packages/coding-agent/src/core/compaction/branch-summarization.ts` | 382 | 分支摘要 | — | P1 |
| `packages/coding-agent/src/core/compaction/index.ts` | 7 | 导出 | — | P2 |
| `packages/coding-agent/src/core/messages.ts` | 196 | CompactionSummary/BranchSummary 消息与 convertToLlm | `convertToLlm`:148 | P1 |
| `packages/coding-agent/src/core/session-export.ts` / `export-html/` | 44/~700 | 会话导出 | — | P2 |
| `packages/agent/src/harness/compaction/compaction.ts` | 865 | harness 世界另一套压缩 | `shouldCompact`:246, `findCutPoint`:370 | P2 |

阅读顺序:`session-manager.ts` 前 600 行(类型+三算法)→ `compaction.ts`(设置/估算/切点/摘要四段)→ `compaction/utils.ts` → `branch-summarization.ts`。

## 6. rpi 实现差异:分项目管理与上下文快照

pi 按 cwd 分目录存放;rpi 采用**扁平目录 + 文件名项目前缀**(`crates/rpi-session/src/manager.rs` `project_prefix`):

- 文件名:`~/.rpi/sessions/<项目前缀>__<session-id>.jsonl`;前缀编码规则:路径分隔符 `/` `\` → `-`,空白与文件名非法字符(`: * ? " < > |` 及控制字符)→ `_`,其余字符(含非 ASCII)保留,去除首尾 `-`,超长按 char 边界截断(100 字符),空结果回退 `session`;
- `--continue` 不依赖文件名:`find_latest_session_file` 仍按 header `cwd` 精确匹配,对有/无前缀的新旧文件都成立;旧版二进制读新文件时,前缀文件名照常可打开(文件名不参与解析);
- header `cwd` 字段始终保存完整未编码路径。

rpi 扩展 entry:`context_ref`(第 12 种;`CURRENT_SESSION_VERSION = 4`)—— 每次向模型提交请求的上下文快照引用:

- **开关(settings `contextSnapshot`,默认关)**:项目 `.rpi/settings.json` 优先于全局 `~/.rpi/settings.json`,首个配置生效;未配置 = 关闭,不产生快照、不建 `.ctx` 目录。settings 解析在 CLI 入口(main)一次完成、经 `SessionSettings`/`BuildOptions.context_snapshot` 显式传入,装配层不读用户配置文件。
- 字段:`{id, parentId, path, timestamp}`;`path` 指向快照文件(绝对路径);
- 快照文件:session 文件旁 `<file-stem>.ctx/<uuid>.json`,内容 = **发送前的原始 HTTP 请求体**(`StreamOptions.on_payload` 观测的第一手数据,原样落盘:provider 特定格式,含 model/messages/tools 等字段);
- 产生时机:每次真实 wire 请求一条 —— **provider 内部重试的每次尝试也各记一条**(每次都是真实提交);compaction 摘要请求不经 agent 的 stream_options,不产生快照;纯内存会话跳过;
- **不进模型上下文**:`session_entry_to_context_messages` 显式排除,投影/恢复/压缩管线不受影响;旧版二进制读新文件把该行按损坏行静默跳过(不致命)。

rpi 还支持**系统提示词外置**(用户可编辑):约定文件 项目 `.rpi/system-prompt.md` → 全局 `~/.rpi/system-prompt.md`,首个存在且非空(空白视为未配置)的生效;内容经 `SystemPromptOptions.custom_prompt` **替换身份句(preamble)**,`<cwd>`/`<tools>`/`<rules>` 等动态节仍自动注入(工具集变化照常 diff 更新)。仅在程序启动/新建会话(装配期)读取一次,会话中途修改文件不生效。

## 踩坑记录

- **2026-09-25 `estimate_projected_context_tokens` 与 pi 的边界语义相反**:现象是正常会话(从未压缩/编辑)永远不走真实 usage、系统性高估。原因:pi 中无 context_edit/compaction 时 `latestInvalidatingEntryIndex = -1`,`usageEntryIndex > -1` 恒真;Rust 版把 `rposition` 返回 `None` 当作"无失效 entry 可比",跳过了信任分支。解法:`(Some(_), None) => true` 显式表达 -1。(关联文件:`crates/rpi-session/src/compaction.rs`)
- **2026-09-25 摘要截断按字节切片切进多字节字符而 panic**:现象是 tool result 含中文且超 2000 字节时 `&text[..2000]` 直接 panic。原因:Rust 字节切片 ≠ JS 的 UTF-16 substring。解法:按 `chars()` 计数与截断。(关联文件:`crates/rpi-session/src/compaction.rs` `truncate_for_summary`)
- **2026-09-25 get_tree 单遍挂接把"既是 child 又是 parent"的节点提前 take**:现象是三条线性链就 panic "parent not yet consumed"。原因:parent 槽位在自己挂接时已被 take。解法:逆序两阶段构建(children 先定型,槽位保持可访问,最后 reverse 恢复文件顺序);parentId 指向后方 entry 的损坏数据按根处理,不 panic。(关联文件:`crates/rpi-session/src/manager.rs`)
- **2026-09-25 context_edit 的"null 替换"与"无 edit entry"被 flatten 混淆**:现象是 `replacement=null` 的剔除不生效。原因:`Option<Option<&T>>` 被 `.flatten()` 压成一层,丢失"edit 存在但替换为 null"的语义。解法:edits 表存 `Option<&ContextReplacement>`,查表后先判外层(Some = 有 edit entry)再判内层。(关联文件:`crates/rpi-session/src/projection.rs`)
- **2026-09-25 有意保留的取舍**:①摘要的 LLM 调用经 `Summarizer` trait 注入,provider 支撑的实现由装配方提供 —— 保持 rpi-session 只依赖 rpi-agent 类型(09 B1);②entry timestamp 用毫秒整数(pi 用 ISO 字符串),rpi 会话文件自用、未承诺与 pi 逐字节兼容;③切点分割 turn 时不做 history/prefix 两次请求,单请求覆盖全范围,`turnStartEntryId` 记录在 details;④`createBranchedSession`/fork(复制历史到新文件)属 M4+ 的 AgentSession 层能力,本 crate 只提供同文件内 branch;⑤`serialize_conversation` 对 BranchSummary/BashExecution/CompactionSummary 按 convertToLlm 的折叠结果(user 文本)进摘要,与 pi 的"先 convert 再序列化"等价。
