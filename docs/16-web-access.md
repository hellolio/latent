# 16 — web 访问扩展(rpi-web)

> **一句话**:web-access(pi 的 TS 网络搜索扩展)的 Rust 核心重实现 —— 新 crate `crates/rpi-web`,提供 `web_search / get_search_content / fetch_content / source_check / web_access` 五个工具,核心设计是**有界输出 + 存储 + responseId 二次检索**,provider 可用性检查纯本地零开销、auto 链按"有 key 用 key、没 key 用免费"降级。移植基准与逐字契约见 `web-search-principles-and-prompts.md`(下称"分析文档")。

## 1. 定位与分层

- 新 crate 与 rpi-tools 同层:仅依赖 `rpi-agent`(Tool trait)+ `rpi-ai`(LLM 注入),**不依赖 rpi-core/rpi-cli**(docs/10 分层纪律)。workspace 成员与 `rpi-web.workspace = true` 依赖已加。
- 对 rpi-core 的两个能力以 trait 注入,实现留在 `rpi-cli/assembly.rs`:
  - `ToolSetActivator`:web_access → `AgentSession::set_active_tools_by_name`(在现有激活集上追加);
  - `BackgroundNotifier`:includeContent 后台抓取完成 → `wait_idle → follow_up → continue_run`(仿 subagent supervisor,14 §4.3)。
- 会话/agent 弱引在 session 建好后回填(与 `session_cell` / `subagent_parent_cell` 同点)。

## 2. 工具与懒激活

| 工具 | 功能 | 权限 |
|---|---|---|
| `web_search` | 多 query 并发(限 3)搜索,provider 路由 fallback,有界输出 + responseId | ReadOnly |
| `get_search_content` | 按 responseId 分页切片 / findText 定位(exact / case-insensitive / fuzzy) | ReadOnly |
| `fetch_content` | URL 抓取,readable / raw / answer 三模式(readable = 正文提取 → markdown) | ReadOnly |
| `source_check` | 论断取证,ResearchArtifact(passage 级引用 + sha256 content_hash) | ReadOnly |
| `web_access` | 懒加载激活器:把上面四个拉入激活集,并经 follow_up 唤醒新 run(工具集在 run 起点快照,新 run 生效) | ReadOnly |

- **默认懒激活**:装配期候选池含全部 web 工具;未显式配置激活集时默认剔除四个可激活工具、只留 `web_access`(模型经 promptSnippet 知道先调用它)。**settings `tools` 显式配置 = 精确集合,完全尊重** —— 需要 web 的用户把 `web_access`(或具体工具名)加进自己的 tools 列表。
- `classify_tool`(13 文档)五个名字归 ReadOnly:无本地写副作用(缓存写专属目录 `~/.rpi/web-search-cache/`),目标 URL 过 SSRF 私网封锁。

## 3. 关键机制(与分析文档对照)

- **凭据金字塔**:`is_available()` 纯本地(配置/env 存在性,不发请求零开销);auto 链序 `searxng → exa → brave → tavily → duckduckgo`(免费/自托管优先,duckduckgo 零配置兜底);`all` 合资格集合 `searxng/exa/brave/tavily`(duckduckgo 是显式点名制,不进 all —— 上游"opt-in 不被扇出"原则)。任何一层缺失只跳过、不报错。
- **provider trait**:`SearchProvider { id/label/is_available/search }`;精选 5 家(取舍已确认):duckduckgo(HTML 解析)、searxng(自托管 JSON)、brave(GET-JSON)、tavily(POST-JSON + `TAVILY_API_KEY_1..20` key-pool failover)、exa(key REST:/answer 或 /search)。
- **错误分类**:11 种 kind(`error.rs`,状态码 + 消息正则,与上游 classifyProviderError 对齐);配置路由 `searchRouting.providers + fallbackOn` 只在命中类别时换下一家。
- **有界输出**:默认 30k 字符(上限 200k);截断 marker + get_search_content 检索指引(文案逐字,`prompts.rs`)。注意 rpi loop 还有 20k head/tail 裁剪 —— 检索指引在尾部可保留。
- **存储**(`storage.rs`):内存 HashMap(responseId 协议);fetch 全文落盘 `~/.rpi/web-search-cache/`,原子写(tmp+rename)、0700/0600、TTL 1h、LRU 128 条/128MB。与上游差异:**session journal 重放不做**(rpi 无工具面 appendEntry,取舍已确认)。
- **摘要(auto-summary)**:摘要 prompt(防编造锚点逐字)→ 30s deadline 竞速 → 失败/超时回退确定性摘要(240 字符预览 + 计数 + ≤12 去重 URL)。模型来源:`summaryModel` 配置 → 当前主模型(取舍已确认,上游硬编码候选链不移植)。**summary-review 浏览器 UI 省略**(workflow 枚举只有 `none | auto-summary`)。
- **fetch 安全**(`content/ssrf.rs`):协议白名单、DNS 解析断言公网、私网/保留段封锁(含 IPv4-mapped IPv6、fake-IP 198.18.0.0/15 提示)、`ssrf.allowRanges` 豁免、`fetchContent.domainPolicy` allow/deny;**重定向手动跟随、每一跳重新校验**(http.rs 单跳原语 + extract.rs 循环)。
- **findText**(content/find.rs):content-find.ts 全量移植 —— fuzzy = NFD 去变音符 + 编辑距离容忍(≥5 字符容 1、≥9 容 2)+ 60% token 命中;命中点前后 400 字符上下文;20k 硬上限下的贪心 witness 摘排算法。

## 4. 配置:`~/.rpi/web-search.json` + `.rpi/web-search.json`

```jsonc
{
  // 凭据:明文 / "$ENV_VAR" / "!shell命令"(1Password 风格,5s 超时 + env 白名单)
  "braveApiKey": "$BRAVE_API_KEY",
  "tavilyApiKey": "!op read 'op://Private/tavily/credential'",
  "exaApiKey": "exa-...",
  "searxngBaseUrl": "https://search.example.com",
  "searxngHeaders": { "X-API-KEY": "..." },
  // 端点覆盖(可选):braveBaseUrl / tavilyBaseUrl / exaBaseUrl,env 同名大写
  // 路由
  "searchProvider": "auto",            // 或 "all" / "brave" / ["brave","tavily"]
  "searchRouting": {                   // searchProvider 未配置时生效
    "providers": ["searxng", "brave"],
    "fallbackOn": ["network", "transient"]
  },
  // 输出/行为
  "maxInlineContentChars": 30000,      // 上限 200000
  "summaryModel": "provider/model-id", // 摘要 / answer 模式小模型
  "proxy": "socks5h://127.0.0.1:1080", // 每次调用可被工具参数覆盖;"" 强制直连
  "cache": { "maxEntries": 128, "maxBytes": 134217728 },
  "fetchContent": { "domainPolicy": { "allow": [], "deny": [] } },
  "ssrf": { "allowRanges": ["198.18.0.0/15"] }
}
```

凭据语义:env 名命中但值为空 = 未配置(不回退字面值,同 models.json);`$$secret` / `$!secret` 转义;`!命令` 输出 16KB 上限、控制字符拒绝、错误消息 redact key。

## 5. 与上游的偏离清单(取舍确认记录)

1. provider 32 家 → 5 家精选(trait 收敛,后续加家 = 一个文件 + 注册表一行)。
2. 零配置兜底:上游"Exa MCP 免费通道"不做(exa 走 key 通道);auto 兜底为 duckduckgo。
3. `workflow: summary-review`(浏览器 curator,约 5.5k 行 TS)不做;枚举只留 `none | auto-summary`。
4. fetch 的 PDF/GitHub/YouTube/本地视频/图片缩放/浏览器 cookie 通道不做;工具参数相应省略(`forceClone/timestamp/frames/model/auth/answerModel`)。
5. 摘要模型:硬编码候选链 → 配置 `summaryModel` + 回退主模型。
6. session journal 持久化/重放不做;存储 = 内存 + 磁盘缓存。
7. `web_access` 固定加载器名保留;无 `toolNames` 改名面。

## 6. 实现索引

| 内容 | 位置 |
|---|---|
| 工具出口 / WebContext / 注入 trait | `crates/rpi-web/src/tools/mod.rs` |
| 五个工具 | `tools/{web_search,get_search_content,fetch_content,source_check,web_access}.rs` |
| provider trait / auto 链 / all 集合 | `crates/rpi-web/src/providers/mod.rs` |
| 路由(扇出/all/单家/auto/配置路由) | `crates/rpi-web/src/router.rs` |
| 错误分类 | `crates/rpi-web/src/error.rs` |
| 凭据解析 | `crates/rpi-web/src/credential.rs` |
| 配置 | `crates/rpi-web/src/config.rs` |
| 存储(内存 + 磁盘缓存) | `crates/rpi-web/src/storage.rs` |
| 有界输出 | `crates/rpi-web/src/bounded.rs` |
| 逐字契约(prompts/格式化文案) | `crates/rpi-web/src/prompts.rs` |
| SSRF / 提取 / findText / 页面问答 | `crates/rpi-web/src/content/{ssrf,extract,find,page_query}.rs` |
| 摘要工作流 | `crates/rpi-web/src/summary.rs` |
| artifact 构建 | `crates/rpi-web/src/source_check.rs` |
| LLM 一次性调用 | `crates/rpi-web/src/llm.rs` |
| 装配(注入桥 + 懒激活默认) | `crates/rpi-cli/src/assembly.rs`(web 段) |
| 手工端到端验证(需网络,直接驱动工具不经模型) | `crates/rpi-web/examples/e2e_manual.rs` |
| 权限分类 | `crates/rpi-core/src/permission/types.rs`(classify_tool) |

契约测试:`prompts.rs` 内逐字节断言(工具 description / promptSnippet / 页面问答 system prompt / FETCH_MODE_DESCRIPTIONS);模块单测覆盖错误分类、数量 clamp、截断、findText、存储 TTL/LRU、SSRF CIDR、凭据解析与 redact。
