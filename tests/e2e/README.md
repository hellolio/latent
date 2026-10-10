# latent E2E 测试（L3：真终端 + 本地 mock LLM）

像人一样测试：在真实 PTY 里启动 `latent` 二进制、模拟键盘输入、把 ANSI 输出解析
回"用户看到的屏幕"做断言；LLM 由本地 mock 服务替代，响应完全确定。

## 运行

```bash
# 1. 编译被测二进制(默认路径 target/debug/latent,可用 LATENT_BIN 覆盖)
cargo build --bin latent

# 2. 装 Python 依赖(首次;装进当前全局/conda 环境)
pip install -r requirements.txt

# 3. 跑测试
pytest -v
```

依赖只有 `pexpect` / `pyte` / `pytest` 三个纯 Python 包,统一装在全局
Python(本机为 conda base)环境,不建虚拟环境。

要求:macOS 或 Linux(pexpect 不支持 Windows 原生 PTY,Windows 走 WSL)。

## 架成三件套

| 文件 | 职责 |
|---|---|
| `mock_llm.py` | 本地 mock LLM 服务:伪装 anthropic-messages 端点，按场景脚本逐 turn 返回 SSE 流（`delay_ms` 响应前延迟、`chunk_delay_ms` delta 间逐块延迟模拟慢速流式）;记录 latent 发来的每个请求供反向断言 |
| `harness.py` | `LatentApp`:临时隔离 HOME(写入指向 mock 的 models.json,可经 `settings=` 注入 settings.json)+ pexpect 启动真实二进制 + pyte 解析屏幕;提供打字/断言/退出 API |
| `test_*.py` | 场景测试,每个对应一条用户使用流程 |

## 写一个新场景

1. 在 `scenarios/` 加一个 JSON(mock LLM 的响应序列,格式见该目录 README);
2. 新建 `test_xxx.py`:

```python
from harness import LatentApp, load_scenario

def test_my_flow():
    app = LatentApp(turns=load_scenario("my_flow"))
    try:
        app.wait_ready()                      # 等横幅/编辑器就绪
        app.sendline("用户输入")               # 像打字一样输入并回车
        app.expect_text(r"期望出现在屏幕上的内容")
        bodies = app.wait_for_requests(2)     # (可选)反向断言发出的 LLM 请求
        assert "..." in str(bodies[1])
    finally:
        app.close()                           # 必须清理(杀进程/关 mock/删临时目录)
```

### 断言 API 速查

| 方法 | 用途 |
|---|---|
| `wait_ready()` | 等启动完成(之后输入才被应用接受) |
| `type_text(s)` / `sendline(s)` | 逐字符输入 / 输入并回车 |
| `send_key(name)` | 特殊键:`esc` `ctrl+c` `ctrl+d` `ctrl+o` `enter` |
| `expect_text(pattern, timeout)` | 等正则出现在可见输出中(**忽略空白**与 ANSI) |
| `expect_absent(pattern)` | 输出静默后断言未出现 |
| `visible_text()` / `transcript()` | 当前屏幕 / 全量可见输出 |
| `wait_for_requests(n)` / `request_bodies()` | mock 侧:等 / 读 latent 发出的请求体 |
| `quit()` + `exit_status` | Ctrl+D 退出并校验退出码 |

### 注意事项

- 断言面向"人读到的文本":markdown 渲染会在 CJK 间插空格、TUI 会重绘,
  `expect_text` 对空白不敏感,写 pattern 时不要依赖排版。
- 应用在 Inline 视口变化时会查询光标位置,harness 的后台读线程会自动应答
  (模拟真终端);不要在测试里绕过 harness 直接读 child 输出。
- `LatentApp` 用临时目录做 `HOME` 并注入 `LATENT_HOME` 指到其 `.latent` 子目录,session 写进隔离环境,不会污染真实数据目录。
