"""@ 文件选择弹窗(pi @ autocomplete 对应):输入 @ 弹出文件/目录候选,
↑/↓/Enter 选中后把路径补全进输入框;提交时 @path 作为普通文本发给模型,
**文件内容不内联**(对齐上游 pi:由模型自行调 read 工具按需读取)。

夹具:测试自建 workdir(README.md / src/main.rs / node_modules/x.js),
显式传给 LatentApp(workdir=...)并自行清理。
"""

import json
import os
import shutil
import tempfile
import time

from harness import LatentApp

REPLY = "好的,我看到了你提到的文件。"


def make_workdir() -> str:
    workdir = tempfile.mkdtemp(prefix="latent_e2e_mention_")
    os.makedirs(f"{workdir}/src")
    os.makedirs(f"{workdir}/node_modules")
    with open(f"{workdir}/README.md", "w", encoding="utf-8") as f:
        f.write("hello from readme")
    with open(f"{workdir}/src/main.rs", "w", encoding="utf-8") as f:
        f.write("fn main() {}")
    with open(f"{workdir}/node_modules/x.js", "w", encoding="utf-8") as f:
        f.write("noise")
    with open(f"{workdir}/.latentignore", "w", encoding="utf-8") as f:
        f.write("node_modules/\n")
    return workdir


def user_message_texts(body: dict) -> list[str]:
    """请求体里全部 user 消息的拼接文本(anthropic content 块兼容)。"""
    texts = []
    for message in body.get("messages", []):
        if message.get("role") != "user":
            continue
        content = message.get("content")
        if isinstance(content, str):
            texts.append(content)
        else:
            texts.append(
                " ".join(
                    block.get("text", "")
                    for block in content
                    if isinstance(block, dict)
                )
            )
    return texts


def wait_screen_absent(app: LatentApp, pattern: str, timeout: float = 3.0) -> None:
    """轮询当前屏幕,直到 pattern 消失(弹窗行被过滤/退场的断言)。"""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if pattern not in app.visible_text():
            return
        time.sleep(0.05)
    raise AssertionError(f"屏幕上仍未消失: {pattern!r}\n{app.visible_text()}")


def test_at_popup_select_and_submit_keeps_mention_as_plain_text():
    workdir = make_workdir()
    # 应用侧 cwd 是 getcwd 的物理路径(macOS /var → /private/var)
    root = os.path.realpath(workdir)
    app = LatentApp(turns=[{"text": REPLY}], workdir=workdir)
    try:
        app.wait_ready()
        # 输入 @:弹窗出现,列出 README.md 与目录 src/(尾缀 /)
        app.type_text("@")
        app.expect_visible("README.md")
        app.expect_visible("src/")
        # node_modules 被 .latentignore 规则剪枝,不出现在候选里
        time.sleep(0.3)
        assert "node_modules" not in app.visible_text()

        # 过滤:输入 rea 后 src/ 从屏幕消失(只剩 README.md 行)
        app.type_text("rea")
        wait_screen_absent(app, "src/")

        # Enter 选中:补全为 @绝对全路径(尾随空格),弹窗退场;
        # 手输的路径才原样发出——选中与手输的区分点
        app.send_key("enter")
        app.expect_visible(f"@{root}/README.md")
        wait_screen_absent(app, "❯")

        # 继续输入提问并提交:回复到达,mock 收到请求
        app.type_text(" 这个文件讲什么?")
        app.send_key("enter")
        app.expect_text(REPLY)
        bodies = app.wait_for_requests(1)

        # 反向断言:提交的 user 消息包含 @全路径 提及文本,但请求体不含
        # 文件内容(不内联;模型需自行 read)——对齐 pi 的 TUI 行为。
        # (模式切换的 mode-section 也是 user 消息,故断言"含提及的消息唯一")
        user_texts = user_message_texts(bodies[0])
        mentions = [t for t in user_texts if f"@{root}/README.md" in t]
        assert len(mentions) == 1, f"应只有一条含提及的 user 消息: {user_texts}"
        assert "这个文件讲什么?" in mentions[0], f"提及与提问同在一条消息: {mentions}"
        assert "hello from readme" not in json.dumps(bodies[0]), "文件内容不应被内联"
    finally:
        app.close()
        shutil.rmtree(workdir, ignore_errors=True)


def test_at_directory_descent_completes_step_by_step():
    workdir = make_workdir()
    root = os.path.realpath(workdir)
    app = LatentApp(turns=[], workdir=workdir)
    try:
        app.wait_ready()
        # @ + sr 过滤到目录 src,Enter 下钻(补全为 @全路径/src/,弹窗保持)
        app.type_text("@sr")
        app.expect_visible("src/")
        app.send_key("enter")
        app.expect_visible(f"@{root}/src/")
        # 弹窗保持:显示 src 的直接子项 main.rs
        app.expect_visible("main.rs")

        # 再次 Enter:补全为 @全路径/src/main.rs(文件,尾随空格)
        app.send_key("enter")
        app.expect_visible(f"@{root}/src/main.rs")
        # 未提交:零 LLM 请求
        time.sleep(0.5)
        assert len(app.request_bodies()) == 0, "未提交不应产生 LLM 请求"
    finally:
        app.close()
        shutil.rmtree(workdir, ignore_errors=True)
