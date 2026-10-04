"""T11:--continue 恢复——退出后重启,历史回放上屏且可续聊(共享 HOME/workdir)。"""

import shutil
import tempfile

from harness import LatentApp, load_scenario


def test_continue_replays_history_and_extends_session():
    home = tempfile.mkdtemp(prefix="latent_e2e_cont_home_")
    workdir = tempfile.mkdtemp(prefix="latent_e2e_cont_cwd_")
    try:
        first = LatentApp(
            turns=load_scenario("continue_first"), home=home, workdir=workdir
        )
        try:
            first.wait_ready()
            first.sendline("你好")
            first.expect_text("第一次的固定回复")
            first.quit()
            assert first.exit_status == 0
        finally:
            first.close()

        second = LatentApp(
            turns=load_scenario("continue_second"),
            home=home,
            workdir=workdir,
            extra_args=["--continue"],
        )
        try:
            second.wait_ready()
            # 历史回放:上一轮的问答在上屏可见
            second.expect_text("你好")
            second.expect_text("第一次的固定回复")
            # 续聊:新一轮请求携带完整历史 + 新消息
            second.sendline("还在吗")
            second.expect_text("续聊后的回复")
            bodies = second.wait_for_requests(1)
            payload = str(bodies[0])
            assert "你好" in payload, "续聊请求应携带历史消息"
            assert "还在吗" in payload
        finally:
            second.close()
    finally:
        shutil.rmtree(home, ignore_errors=True)
        shutil.rmtree(workdir, ignore_errors=True)
