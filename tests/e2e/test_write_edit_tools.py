"""T17:write/edit 工具往返——文件真实落盘 + 工具结果回传请求体。"""

import json
import os

from harness import RpiApp, load_scenario


def test_write_then_edit_roundtrip():
    workdir = os.path.join(os.environ.get("TMPDIR", "/tmp"), f"rpi_e2e_we_{os.getpid()}")
    os.makedirs(workdir, exist_ok=True)
    try:
        app = RpiApp(turns=load_scenario("write_edit"), workdir=workdir)
        try:
            app.wait_ready()
            app.sendline("写一个文件再编辑它")
            app.expect_text("文件写入并编辑完成")

            path = os.path.join(workdir, "note.txt")
            with open(path, encoding="utf-8") as f:
                assert f.read() == "hi e2e\n", "write 后 edit 应真实落盘"

            bodies = app.wait_for_requests(3)
            # write 的 toolResult 是落盘确认;edit 的 toolResult 报告替换
            second = json.dumps(bodies[1], ensure_ascii=False)
            assert "note.txt" in second, "write 结果应回传给 LLM"
            third = json.dumps(bodies[2], ensure_ascii=False)
            assert "Applied 1 edit(s)" in third, "edit 结果应回传给 LLM"
        finally:
            app.close()
    finally:
        import shutil
        shutil.rmtree(workdir, ignore_errors=True)
