# 给 rpi 补齐 pi 的工具输出净化能力(修订版)

三个改动:①ANSI 剥离+控制字符过滤覆盖所有产出外部内容的工具,默认开启,模型可用 `sanitize` 参数关闭;②图片占位符替换由用户配置 `blockImages` 控制;③JSON 修复路径处理孤立代理对转义。

## 改动 1:rpi-tools 新增 sanitize 模块 + 全工具 `sanitize` 参数

**新文件 `crates/rpi-tools/src/sanitize.rs`**,lib.rs 加私有 `mod sanitize;`(docs/10-implementation-policy.md §2,不进 `pub use`):

- `strip_ansi(text: &str) -> String`:剥离 CSI(`\x1b[...` 至 `@-~` 最终字节)与 OSC(`\x1b]...` 至 BEL/ST)序列;快路径:不含 `\x1b` 直接返回原文(照 pi `utils/ansi.ts`)。用已有 `regex` 依赖。
- `sanitize_control_chars(text: &str) -> String`:白名单保留 `\t` `\n` `\r` 与可打印字符,删除其余 C0/C1 控制字符及 `U+FFF9-FFFB`(照 pi `shell.ts` 的 `INVALID_SHELL_OUTPUT`)。Rust `String` 恒为合法 UTF-8,无孤立代理对,注释说明。
- `sanitize_output(text: &str) -> String`:组合入口,`pub(crate)` 供各工具复用。

**工具参数(默认开启,模型可关)**:bash/powershell、read、grep、find、ls 五个工具的 `schema()` 各加可选属性:

```json
"sanitize": {"type": "boolean", "description": "Set false to keep raw output including ANSI color codes and control characters. Defaults to true."}
```

各工具 `parse_args` 解析 `Option<bool>`,缺省 `true`。edit/write 不加(输出是 rpi 自己生成的固定确认文本,净化为空操作)。

**应用位置**(参数为 true 时调用 `sanitize_output`):
- bash/powershell:`bash.rs:354` `settle_output` 改为接收 sanitize 标志,对 `snapshot.content` 过滤;超时路径(bash.rs:314-327)同样传入。流式 `tail()`(TUI 实时显示)与落盘临时文件保持原文——TUI 保留彩色、临时文件可调试(与 pi 的 raw 落盘一致)。
- read:`read.rs:123` `from_utf8_lossy` 之后过滤;NUL 二进制拦截(read.rs:116-122)保持在前。
- grep:`grep.rs:229` 解码后对匹配行内容过滤。
- find/ls:文件名列表(`to_string_lossy` 后)过滤。

**测试**:sanitize.rs 内联 `#[test]`(颜色码/OSC 剥离、无 ESC 快路径、控制字符白名单、`U+FFF9-FFFB`、多字节保留);bash/read/grep 各补参数开关用例(schema 含新属性、`sanitize:false` 时输出保留 ANSI 码、缺省为 true)。

## 改动 2:图片占位符替换(用户配置 `blockImages`)

**配置**(rpi-core/src/config.rs):
- `SettingsDefaults`(:311)加 `block_images: Option<bool>`(serde camelCase `blockImages`,默认 `false` = 允许图片输入)。
- 按 `load_theme`(:355-374)的既有模式新增加载函数:项目 `.rpi/settings.json` 优先于全局 `~/.rpi/settings.json`。

**替换逻辑**(rpi-ai/src/transcript.rs 新增纯函数) `replace_images_with_placeholders(messages: Vec<Message>, block: bool, model: &Model) -> Vec<Message>`:
- `block == true` **或** `model.input`(types.rs:430)不含 `"image"` 时,把 `ToolResult.content` / `User` blocks 里的 `ContentBlock::Image` 替换为文本 `"[Image omitted: image input is disabled or unsupported]"`;连续占位符合并为一条(pi `sdk.ts:268` 的去重逻辑)。
- `block == false` 且模型支持图片时原样保留(anthropic `content_blocks_to_api:136` 的 has_images 分支不受影响);同时统一了 openai 适配器静默丢图的行为(`openai_completions.rs:431`)。

**接线**:装配层从 settings 读出 `block_images`,经 agent loop 入口以参数传入 `stream_assistant_response`(loop_.rs:919,已有 `#[allow(clippy::too_many_arguments)]`);在 `hooks.convert_to_llm` 之后、`normalize_context` 之前(loop_.rs:933-939)调用替换函数。接线沿途(rpi-core 装配 / rpi-cli / rpi-tui 构造点)把标志带过去,不改接缝 trait 签名。

**测试**:config.rs 加载用例(照 :522-558 风格);transcript.rs 内联用例(开启替换+去重、关闭且模型支持时保留、模型不支持时替换)。

## 改动 3:rpi-ai json_parse.rs 代理对转义清理(Rust 版"代理对清理")

**背景**:Rust `String` 恒为合法 UTF-8,无孤立代理对;真实风险点是 LLM 流式 JSON 里的孤立 `\uD800` 转义——`serde_json` 报 "lone leading surrogate in hex escape",而 `repair_json`(json_parse.rs:36-49)原样保留合法 hex 转义,导致整个工具参数解析失败退化为 `{}`。

**修复**:`repair_json` 中,`\uD800-DBFF` 未跟 `\uDC00-DFFF`、或孤立 `\uDC00-DFFF`,替换为 `\uFFFD`;成对代理对(如 `\ud842\udfb7`)保留为真实字符。

**测试**:孤立高/低代理修复后可解析出 U+FFFD;成对代理对正确合并;`parse_streaming_json` 不再因此退化为 `{}`。

## 验证

- `cargo test -p rpi-tools -p rpi-ai -p rpi-core -p rpi-agent`
- `cargo clippy --workspace`
- 手动冒烟:`cargo run -p rpi-cli`,①执行 `ls --color=always` 确认工具结果无 ESC 码,模型要求 `sanitize:false` 时保留;②settings.json 设 `"blockImages": true` 确认含图消息被替换为占位符。