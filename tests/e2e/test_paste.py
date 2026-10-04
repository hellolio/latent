"""T25:bracketed paste——粘贴是整块事件,多行粘贴不逐行触发提交;
大段粘贴折叠为占位摘要,提交时完整内容发给模型。"""

import time

from harness import LatentApp


def _paste(app, text: str) -> None:
    """模拟终端 bracketed paste:内容包裹在 200~/201~ 标记里整块送达
    (行间分隔与真实终端一致,为 \r)。"""
    app.child.send(f"\x1b[200~{text}\x1b[201~")


def test_multiline_paste_does_not_submit():
    app = LatentApp(turns=[{"text": "回复"}])
    try:
        app.wait_ready()
        _paste(app, "first line\rsecond line")
        time.sleep(0.6)
        # 编辑器里是两行草稿;粘贴的换行不触发提交(mock 未收到请求)
        assert len(app.request_bodies()) == 0, "粘贴不应触发提交"
        vt = app.visible_text()
        assert "first line" in vt, "{vt:?}"
        assert "second line" in vt, "{vt:?}"
        # Enter 一次性提交
        app.send_key("enter")
        app.expect_text("回复")
        assert len(app.request_bodies()) == 1
    finally:
        app.close()


def test_large_paste_collapses_to_placeholder():
    app = LatentApp(turns=[{"text": "回复"}])
    try:
        app.wait_ready()
        content = "\r".join(f"line{i}" for i in range(1, 6))  # 5 行
        _paste(app, content)
        time.sleep(0.6)
        # 编辑器只显示占位摘要
        vt = app.visible_text()
        assert "[Pasted text #1 +5 lines]" in vt, "{vt:?}"
        assert "line5" not in vt, "完整内容不应全量显示: {vt:?}"
        # 提交后完整内容发给模型
        app.send_key("enter")
        app.expect_text("回复")
        body = str(app.request_bodies()[-1])
        assert "line1" in body and "line5" in body, "提交应展开完整粘贴内容"
    finally:
        app.close()
