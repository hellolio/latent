"""T15:并行多工具——一轮两个 read 调用,结果齐全且按源序进入第二轮请求。"""

import json
import os

from harness import RpiApp, load_scenario


def test_parallel_tool_calls_roundtrip_in_order():
    workdir = os.path.join(os.environ.get("TMPDIR", "/tmp"), f"rpi_e2e_par_{os.getpid()}")
    os.makedirs(workdir, exist_ok=True)
    with open(os.path.join(workdir, "a.txt"), "w") as f:
        f.write("content-of-a")
    with open(os.path.join(workdir, "b.txt"), "w") as f:
        f.write("content-of-b")
    try:
        app = RpiApp(turns=load_scenario("parallel_tools"), workdir=workdir)
        try:
            app.wait_ready()
            app.sendline("同时读两个文件")
            app.expect_text("两个文件都读完了")

            bodies = app.wait_for_requests(2)
            second = json.dumps(bodies[1], ensure_ascii=False)
            assert "content-of-a" in second and "content-of-b" in second, "两个工具结果都应回传"
            # 源序不变量:toolu_0(a.txt) 的结果在 toolu_1 之前
            assert second.index("content-of-a") < second.index("content-of-b")
        finally:
            app.close()
    finally:
        import shutil
        shutil.rmtree(workdir, ignore_errors=True)
