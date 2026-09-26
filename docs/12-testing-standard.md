# 12 · 测试标准(Test Pyramid & E2E)

> 本文档是 rpi 的测试规范:分哪些层、每层测什么、什么改动必须补哪层测试、
> 以及 L3"像人一样"的端到端测试怎么写。基建对应 `tests/e2e/`(L3)与
> `crates/rpi-tui/src/app.rs` 的 `TestBackend` 接入(L2)。

## 分层模型

```
        ┌─────────────┐
        │ L3 真终端 E2E │  真实二进制 + PTY 键盘输入 + 本地 mock LLM
        ├─────────────┤  tests/e2e/ (pytest) — 少而精,只测核心用户流程
        │ L2 TUI 屏幕  │  TuiApp 组件 + TestBackend 屏幕断言
        ├─────────────┤  crates/rpi-tui/src/app.rs #[cfg(test)]
        │ L1 集成      │  crate 间协作、trait 接缝、agent 循环
        ├─────────────┤  crates/*/tests/*.rs
        │ L0 单元      │  纯函数/单一类型
        └─────────────┘  各 crate #[cfg(test)] — 大量、快速、随处
```

原则:**能往低层推的测试尽量往低层推**(快、定位准);高层只保留低层
覆盖不了的"组合行为"——尤其是只有真终端上才能暴露的问题(光标、
raw mode、PTTY、视口重建、输入竞态)。

## 各层定义

### L0 单元测试

- **范围**:纯函数与单一类型的行为(markdown 渲染、key 归一、编辑器操作、
  JSONL 条目、工具参数解析……)。
- **位置**:被测代码同文件的 `#[cfg(test)] mod tests`。
- **约定**:
  - 不做 I/O(文件/网络/终端),需要文件时用临时目录并在测试内清理;
  - 断言用标准 `assert_eq!/assert!`,失败信息自带上下文(打印实际值);
  - AI 边界(L0 也好 L1 也好)一律不联网:用 `Provider` trait 接缝
    (`MockProvider` / `ScriptedProvider`,见 `rpi-ai/src/mock.rs`)或
    本地 `TcpListener` 模拟 SSE。

### L1 集成测试

- **范围**:跨 crate 协作——agent 循环多轮工具调用、session 树投影/压缩、
  core 装配、json/rpc/print 模式、MCP 扩展注册。
- **位置**:`crates/<crate>/tests/*.rs`。
- **约定**:
  - provider 用 `ScriptedProvider` 按脚本逐 turn 出流(多轮循环必用);
  - 文件类测试用进程内临时目录,不依赖 `/tmp` 以外的环境;
  - CLI 模式测试用内存 Writer / 通道,不起真进程(那属于 L3)。

### L2 TUI 屏幕测试(TestBackend)

- **范围**:`TuiApp` 的 `commit_lines` / `draw_viewport` 组合出的**最终屏幕
  效果**——折行、截断、光标位置、视口占行。组件级渲染(header/footer/
  markdown/select_list)仍是 L0 纯函数,不走这层。
- **位置**:`crates/rpi-tui/src/app.rs` 的 `#[cfg(test)] mod tests`。
- **怎么写**:`TuiApp` 的 backend 已泛型化(`TuiApp<B: Backend = CrosstermBackend<Stdout>>`),
  测试用 `test_app(cols, rows, viewport_height)` 构造绑定
  `TestBackend` 的实例,然后:
  - 断言**相对结构**(commit 行在视口上方、按序),不要断言绝对行号——
    TestBackend 没有真实终端的光标锚定,视口位置与真实终端不同;
  - 屏幕文本提取用现成的 `screen_lines()` helper;
  - 视口重建/resize 路径(set_viewport_height/redraw_full)与真实终端
    耦合过深,由 L3 覆盖,不在 L2 测。

### L3 真终端 E2E(pexpect + 本地 mock LLM)

- **范围**:按**用户使用流程**测完整二进制——启动、提问、流式回复、工具
  往返、Esc 中断、退出。这是唯一能抓住"人一用就发现"那类问题
  (输入竞态、光标查询超时、渲染错位、退出码)的层。
- **位置**:`tests/e2e/`(pytest;运行方式与编写规范见 `tests/e2e/README.md`)。
- **架构**:
  - `mock_llm.py`:本地 mock LLM 服务,伪装 anthropic-messages SSE 端点,
    按场景 JSON 逐 turn 返回,并**记录 rpi 发来的请求体**(可反向断言);
  - `harness.py`:`RpiApp` 封装——隔离 HOME、pexpect 启动真实二进制、
    pyte 把 ANSI 输出解析回屏幕文本、后台线程应答光标位置查询
    (crossterm Inline 视口依赖,~2s 内必须应答否则应用退出);
  - 断言面向"人读到的文本",**对空白不敏感**(markdown 渲染会在 CJK
    间插空格、TUI 会重绘插入换行)。
- **确定性**:LLM 响应由 `scenarios/*.json` 场景脚本决定,不依赖网络与
  API key;二进制经 `models.json` 的 provider override 把 baseUrl 指向
  mock 服务(配置机制为产品功能,测试只是使用它)。
- **数量纪律**:L3 慢(每个用例 ~1-2s),只保留核心用户流程;新流程
  (新斜杠命令、新交互模式、abort/恢复语义变化)**必须**补 L3 场景。

## 改动类型 → 必补测试层

| 改动 | L0 | L1 | L2 | L3 |
|---|:-:|:-:|:-:|:-:|
| 纯函数(渲染/解析/归一) | ● | | | |
| 工具实现(rpi-tools) | ● | ●(循环集成) | | |
| agent 循环/流式协议 | | ●(ScriptedProvider) | | |
| session 树/压缩 | ● | ● | | |
| provider 装配/重试/models.json 解析 | | ● | | |
| TUI 组件(header/footer/markdown…) | ● | | | |
| TuiApp 视口/折行/光标 | | | ● | |
| 键盘事件处理(handlers.rs) | ● | | | ●(关键路径) |
| 交互流程/斜杠命令/abort/退出语义 | | | | ● |
| 模式壳(print/json/rpc) | | ● | | ●(冒烟) |

## 命令速查

```bash
cargo test --workspace          # L0 + L1 + L2
cargo build --bin rpi && \
  cd tests/e2e && pytest -v     # L3(先编译二进制;依赖装在全局/conda 环境)
```

## 已知边界(后续可补)

- **无 CI**:测试全部本地跑;若上 GitHub Actions,L3 在 ubuntu runner
  直接可跑(macOS 需注意 PTY 权限,Windows 走 WSL)。
- **无 snapshot 测试**:markdown/组件渲染是逐行手写断言;若断言负担变重,
  引入 `insta` 做 golden 文件, golden 更新需人工 review。
- **print/json/rpc 模式的 L3 冒烟**:目前 L3 只覆盖 interactive;其余三模式
  已有 L1(内存 Writer),如需"真进程"验证可用 `assert_cmd` 补管道级用例。
