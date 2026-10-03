"""harness.py —— E2E 测试驱动:像人一样操作真实 rpi 二进制。

`RpiApp` 把一次 E2E 测试的全部环境拼起来:

1. 起本地 mock LLM 服务(`mock_llm.MockLLM`,场景脚本决定响应);
2. 造隔离的临时 `HOME`(写入指向 mock 服务的 `.rpi/models.json` provider
   override),不污染真实 `~/.rpi`;
3. 用 pexpect 在真实 PTY 里启动 `rpi --mode interactive`,程序看到的就是
   一个真终端;
4. 提供 `send`(打字)、`expect_text`(等屏幕上出现内容)等人类视角操作,
   输出经 pyte 解析回"用户看到的屏幕文本"。

只测黑盒行为:键盘进、屏幕出、(可选)mock 服务记录的请求内容。
"""

from __future__ import annotations

import json
import os
import re
import shutil
import tempfile
import threading
import time

import pexpect
import pyte

from mock_llm import MockLLM

# CSI 序列 / OSC 序列 / 其他 ESC 对 / 控制字符(保留换行)
_ANSI_RE = re.compile(
    r"\x1b\[[0-9;:?]*[ -/]*[@-~]"  # CSI ... final byte
    r"|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)?"  # OSC ... BEL/ST
    r"|\x1b[@-_]"  # 其余单字符 ESC 序列(\x1b7、\x1b= 等)
)
_CONTROL_RE = re.compile(r"[\x00-\x09\x0b-\x1f\x7f]")


def strip_ansi(text: str) -> str:
    """把终端输出粗略还原成可见文本(用于内容出现断言)。"""
    text = _ANSI_RE.sub("", text)
    text = _CONTROL_RE.sub("", text)
    return text


def _squeeze(text: str) -> str:
    """去掉全部空白:断言面向"人读到的内容",对排版空白(markdown 渲染会在
    CJK 字符间插空格、重绘会插入换行)不敏感。"""
    return re.sub(r"\s+", "", text)


PROVIDER_ID = "e2e"
MODEL_ID = "e2e-model"


def load_scenario(name: str) -> list:
    """读取 scenarios/<name>.json 场景脚本(mock LLM 的响应序列)。"""
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "scenarios", f"{name}.json")
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def _default_rpi_bin() -> str:
    env = os.environ.get("RPI_BIN")
    if env:
        return env
    repo = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    return os.path.join(repo, "target", "debug", "rpi")


class RpiApp:
    """一次 E2E 会话:mock LLM + 隔离 HOME + PTY 里的真实 rpi。

    用法(测试内):

        app = RpiApp(turns=[{"text": "回复内容"}])
        try:
            app.expect_text("rpi v")
            app.sendline("你好")
            app.expect_text("回复内容")
        finally:
            app.close()
    """

    def __init__(
        self,
        turns: list,
        workdir: str | None = None,
        rows: int = 30,
        cols: int = 100,
        timeout: float = 15.0,
        rpi_bin: str | None = None,
        extra_args: list | None = None,
        home: str | None = None,
        session_mode: str | None = "full-access",
    ):
        self.mock = MockLLM(turns).start()
        self.timeout = timeout
        self._closed = False

        # 隔离 HOME:models.json 指向 mock 服务;session 也写进临时目录。
        # 显式传 home 时复用(--continue 跨进程测试);models.json 必须每次
        # 重写:mock 端口随机,旧文件指向的是上一个(已停止)的 mock
        self._owns_home = home is None
        self.home = home or tempfile.mkdtemp(prefix="rpi_e2e_home_")
        os.makedirs(os.path.join(self.home, ".rpi"), exist_ok=True)
        models = {
            "providers": {
                PROVIDER_ID: {
                    "baseUrl": self.mock.base_url,
                    "api": "anthropic-messages",
                    "apiKey": "e2e-dummy-key",
                    "models": [
                        {"id": MODEL_ID, "contextWindow": 32000, "maxTokens": 4096}
                    ],
                }
            }
        }
        with open(os.path.join(self.home, ".rpi", "models.json"), "w", encoding="utf-8") as f:
            json.dump(models, f, ensure_ascii=False)

        self._owns_workdir = workdir is None
        self.workdir = workdir or tempfile.mkdtemp(prefix="rpi_e2e_cwd_")

        env = os.environ.copy()
        env["HOME"] = self.home
        env["TERM"] = "xterm-256color"
        bin_path = rpi_bin or _default_rpi_bin()
        args = ["--mode", "interactive", "--provider", PROVIDER_ID]
        # 既有场景按旧全自动语义编写:默认 --session-mode full-access;
        # 计划模式/审批流场景传 session_mode=None(走默认 Plan)或显式指定
        if session_mode:
            args += ["--session-mode", session_mode]
        args += extra_args or []
        self.child = pexpect.spawn(
            bin_path,
            args,
            cwd=self.workdir,
            env=env,
            dimensions=(rows, cols),
            encoding="utf-8",
            codec_errors="replace",
            timeout=timeout,
        )

        # pyte 把 ANSI 输出解析成屏幕;另存一份去 ANSI 的全量输出做内容断言。
        # 读取在后台线程持续进行:应用在视口变化时会查询光标位置并要求 ~2s
        # 内应答(否则报错退出),不能依赖测试代码轮询式地泵输出。
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.Stream(self.screen)
        self.screen.stream = self.stream
        self._raw: list[str] = []
        self._tail = ""  # 处理跨块的转义序列
        self._lock = threading.Lock()
        self._eof = False
        self._closed = False
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()

    # -- 终端视角 ---------------------------------------------------------------

    def _read_loop(self) -> None:
        """持续读取子进程输出:喂 pyte、累积原文、实时应答 DSR 查询。"""
        while not self._closed:
            try:
                data = self.child.read_nonblocking(size=65536, timeout=0.05)
            except pexpect.TIMEOUT:
                continue
            except Exception:
                break  # EOF / 关闭后的 fd 错误都视为读取结束
            with self._lock:
                self._raw.append(data)
                try:
                    self.stream.feed(data)
                except Exception:
                    pass  # 个别无法解析的序列不影响后续断言
                # 模拟真实终端应答 DSR 光标位置查询(crossterm Inline 视口依赖):
                # 用 pyte 跟踪到的光标位置回 \x1b[<row>;<col>R。
                # 查询序列可能被拆在两个读取块里,所以拼上前一块的尾部再找。
                window = self._tail + data
                self._tail = data[-8:]
                if "\x1b[6n" in window:
                    cursor = self.screen.cursor
                    try:
                        self.child.send(f"\x1b[{cursor.y + 1};{cursor.x + 1}R")
                    except OSError:
                        pass  # 子进程已退出,无需应答
        with self._lock:
            self._eof = True

    def visible_text(self) -> str:
        """当前屏幕上用户看到的内容(按行)。"""
        with self._lock:
            try:
                return "\n".join(self.screen.display)
            except Exception:
                return ""  # pyte 偶发的宽字符渲染异常不影响断言主路径

    def transcript(self) -> str:
        """全量输出的可见文本(含已滚出屏幕的历史)。"""
        with self._lock:
            return strip_ansi("".join(self._raw))

    def expect_text(self, pattern: str, timeout: float | None = None) -> str:
        """轮询屏幕/输出,直到 `pattern`(正则)出现;超时抛 AssertionError。

        断言面向"人看到的文本":ANSI 控制序列被剥离,且**忽略全部空白**
        (markdown 渲染与重绘会插入任意空白),pattern 中的空白同样被忽略。
        """
        deadline = time.monotonic() + (timeout or self.timeout)
        compiled = re.compile(_squeeze(pattern))
        last = ""
        while time.monotonic() < deadline:
            last = self.transcript()
            match = compiled.search(_squeeze(last))
            if match:
                return match.group(0)
            time.sleep(0.05)
        raise AssertionError(
            f"等待超时({timeout or self.timeout}s):屏幕上未出现 {pattern!r}\n"
            f"--- 最后的可见输出 ---\n{last[-2000:]}"
        )

    def expect_visible(self, pattern: str, timeout: float | None = None) -> str:
        """轮询**当前屏幕**(visible_text),直到 `pattern`(正则)出现。

        与 expect_text 的区别:expect_text 匹配含已滚出历史的累计字节流,
        模式一旦出现过的内容会立即命中;expect_visible 只认用户此刻看到的
        屏幕,适用于"按键后屏幕应变为某状态"的断言(如 ctrl+o 重绘)。
        """
        deadline = time.monotonic() + (timeout or self.timeout)
        compiled = re.compile(_squeeze(pattern))
        last = ""
        while time.monotonic() < deadline:
            last = self.visible_text()
            match = compiled.search(_squeeze(last))
            if match:
                return match.group(0)
            time.sleep(0.05)
        raise AssertionError(
            f"等待超时({timeout or self.timeout}s):当前屏幕上未出现 {pattern!r}\n"
            f"--- 最后的可见屏幕 ---\n{last[-2000:]}"
        )

    def expect_absent(self, pattern: str, after_idle: float = 1.0) -> None:
        """等输出静默 `after_idle` 秒后,断言 `pattern` 未出现过。"""
        deadline = time.monotonic() + after_idle
        while time.monotonic() < deadline:
            time.sleep(0.05)
        compiled = re.compile(_squeeze(pattern))
        match = compiled.search(_squeeze(self.transcript()))
        assert match is None, f"不应出现 {pattern!r},却在输出中找到: {match.group(0) if match else ''}"

    # -- 键盘视角 ---------------------------------------------------------------

    def send(self, text: str) -> None:
        """像打字一样输入(可含控制字符,如 \\x1b、\\x03、\\x04)。"""
        self.child.send(text)

    def type_text(self, text: str, delay: float = 0.01) -> None:
        """逐字符输入(模拟真人打字节奏,避免一次性写入与启动竞态)。"""
        for ch in text:
            self.child.send(ch)
            time.sleep(delay)

    def sendline(self, text: str) -> None:
        """输入一行并回车提交(编辑器 Enter = 发送)。"""
        self.type_text(text)
        self.child.send("\r")

    def wait_ready(self, timeout: float | None = None) -> None:
        """等应用就绪:footer 显示模型名、编辑器占位文本出现。之后输入才被接受。"""
        self.expect_text(r"rpi v\d+\.\d+", timeout)
        self.expect_text("Ask rpi to do anything", timeout)

    def send_key(self, name: str) -> None:
        mapping = {
            "enter": "\r",
            "esc": "\x1b",
            "ctrl+c": "\x03",
            "ctrl+d": "\x04",
            "ctrl+o": "\x0f",
            "up": "\x1b[A",
            "down": "\x1b[B",
        }
        self.child.send(mapping[name])

    # -- mock 侧断言 ------------------------------------------------------------

    def request_bodies(self) -> list:
        """rpi 发给 LLM 的全部请求体(按时间序,mock 服务记录)。"""
        return self.mock.request_bodies()

    def wait_for_requests(self, count: int, timeout: float | None = None) -> list:
        """等 mock 收到至少 `count` 个请求,返回请求体列表。"""
        deadline = time.monotonic() + (timeout or self.timeout)
        while time.monotonic() < deadline:
            bodies = self.request_bodies()
            if len(bodies) >= count:
                return bodies
            time.sleep(0.05)
        raise AssertionError(
            f"等待超时:mock 服务只收到 {len(self.request_bodies())} 个请求(期望 ≥{count})"
        )

    # -- 生命周期 ----------------------------------------------------------------

    def quit(self, timeout: float = 5.0) -> None:
        """Ctrl+D 退出并等待进程结束(空输入时 ctrl+d = exit)。"""
        self.child.send("\x04")
        try:
            self.child.expect(pexpect.EOF, timeout=timeout)
            self.child.wait()
        except pexpect.TIMEOUT:
            self.child.terminate(force=True)
            raise AssertionError(f"Ctrl+D 后进程未退出(最后一屏:\n{self.visible_text()})")

    @property
    def exit_status(self):
        return self.child.exitstatus

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        if self.child.isalive():
            self.child.terminate(force=True)
        self.child.close()
        self.mock.stop()
        if self._owns_home:
            shutil.rmtree(self.home, ignore_errors=True)
        if self._owns_workdir:
            shutil.rmtree(self.workdir, ignore_errors=True)
        # 复用传入的 home/workdir 由测试自己清理
