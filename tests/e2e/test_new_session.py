"""T20:/new——进程内新建会话:转录清空、上下文清空、旧会话文件原样保留;
退出后 --continue 续到新会话。"""

import glob
import os
import shutil
import tempfile
import time

from harness import RpiApp, load_scenario


def _session_files(home: str) -> list[str]:
    return glob.glob(os.path.join(home, ".rpi", "sessions", "*.jsonl"))


def test_new_session_clears_context_and_keeps_old_file():
    """/new 后新请求不携带旧上下文;旧 session 文件不被修改。"""
    home = tempfile.mkdtemp(prefix="rpi_e2e_new_home_")
    workdir = tempfile.mkdtemp(prefix="rpi_e2e_new_cwd_")
    try:
        app = RpiApp(turns=load_scenario("new_session"), home=home, workdir=workdir)
        try:
            app.wait_ready()
            app.sendline("旧会话问题")
            app.expect_text("旧会话的回复")

            old_files = _session_files(home)
            assert len(old_files) == 1, f"应有 1 个会话文件,实际 {old_files}"
            old_file = old_files[0]
            old_size = os.path.getsize(old_file)
            time.sleep(0.2)  # mtime 分辨率兜底

            # /new:清屏 + 提示新会话
            app.sendline("/new")
            app.expect_text("new session started")
            new_files = _session_files(home)
            assert len(new_files) == 2, f"/new 后应有 2 个会话文件,实际 {new_files}"

            # 旧文件原样保留(append-only:切换不触发任何写入)
            assert os.path.getsize(old_file) == old_size, "旧会话文件不应被修改"

            # 新会话对话:请求只含系统提示词 + 新消息,不含旧上下文
            app.sendline("新会话问题")
            app.expect_text("新会话的回复")
            bodies = app.wait_for_requests(2)
            second = str(bodies[1])
            assert "新会话问题" in second, "新请求应携带新消息"
            assert "旧会话问题" not in second, "新请求不应携带旧会话上下文"
        finally:
            app.close()
    finally:
        shutil.rmtree(home, ignore_errors=True)
        shutil.rmtree(workdir, ignore_errors=True)


def test_continue_after_new_resumes_latest_session():
    """/new 后退出,--continue 按 mtime 续到新会话(上下文不含旧会话)。"""
    home = tempfile.mkdtemp(prefix="rpi_e2e_new_cont_home_")
    workdir = tempfile.mkdtemp(prefix="rpi_e2e_new_cont_cwd_")
    try:
        first = RpiApp(
            turns=load_scenario("new_session"), home=home, workdir=workdir
        )
        try:
            first.wait_ready()
            first.sendline("旧会话问题")
            first.expect_text("旧会话的回复")
            first.sendline("/new")
            first.expect_text("new session started")
            first.sendline("新会话问题")
            first.expect_text("新会话的回复")
            first.quit()
            assert first.exit_status == 0
        finally:
            first.close()

        second = RpiApp(
            turns=load_scenario("new_session_second"),
            home=home,
            workdir=workdir,
            extra_args=["--continue"],
        )
        try:
            second.wait_ready()
            # 回放的是新会话:只见新问答,不见旧问答
            second.expect_text("新会话问题")
            second.expect_text("新会话的回复")
            assert "旧会话的回复" not in second.transcript(), "不应回放旧会话"
            second.sendline("续聊问题")
            second.expect_text("续到新会话的回复")
            bodies = second.wait_for_requests(1)
            payload = str(bodies[0])
            assert "新会话问题" in payload, "续聊应携带新会话历史"
            assert "旧会话问题" not in payload, "续聊不应携带旧会话上下文"
        finally:
            second.close()
    finally:
        shutil.rmtree(home, ignore_errors=True)
        shutil.rmtree(workdir, ignore_errors=True)
