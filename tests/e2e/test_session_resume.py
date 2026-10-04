"""会话恢复与切换:latent -r 续聊最近会话、latent -l 列出历史会话、
TUI 内 /session 无参数弹出历史会话列表并切换。

断言面向"人读到的文本"+ mock 侧请求体(上下文真值)。
"""

import os
import shutil
import subprocess
import tempfile

from harness import LatentApp, _default_latent_bin, load_scenario


def test_r_flag_reopens_last_session():
    """退出后 latent -r 重启:回放上次问答,续聊携带上次上下文。"""
    home = tempfile.mkdtemp(prefix="latent_e2e_resume_home_")
    workdir = tempfile.mkdtemp(prefix="latent_e2e_resume_cwd_")
    try:
        first = LatentApp(turns=load_scenario("new_session"), home=home, workdir=workdir)
        try:
            first.wait_ready()
            first.sendline("旧会话问题")
            first.expect_text("旧会话的回复")
            first.quit()
            assert first.exit_status == 0
        finally:
            first.close()

        second = LatentApp(
            turns=load_scenario("new_session_second"),
            home=home,
            workdir=workdir,
            extra_args=["-r"],
        )
        try:
            second.wait_ready()
            # 启动回放:上次问答上屏
            second.expect_text("旧会话问题")
            second.expect_text("旧会话的回复")
            # 续聊:请求体携带上次上下文
            second.sendline("续聊问题")
            second.expect_text("续到新会话的回复")
            bodies = second.wait_for_requests(1)
            payload = str(bodies[0])
            assert "旧会话问题" in payload, "续聊应携带上次会话历史"
        finally:
            second.close()
    finally:
        shutil.rmtree(home, ignore_errors=True)
        shutil.rmtree(workdir, ignore_errors=True)


def test_list_flag_prints_sessions_with_preview():
    """latent -l:stdout 打印当前项目的历史会话(序号/时间/预览/路径)。"""
    home = tempfile.mkdtemp(prefix="latent_e2e_list_home_")
    workdir = tempfile.mkdtemp(prefix="latent_e2e_list_cwd_")
    try:
        app = LatentApp(turns=load_scenario("new_session"), home=home, workdir=workdir)
        try:
            app.wait_ready()
            app.sendline("旧会话问题")
            app.expect_text("旧会话的回复")
            app.quit()
            assert app.exit_status == 0
        finally:
            app.close()

        env = os.environ.copy()
        env["HOME"] = home
        proc = subprocess.run(
            [_default_latent_bin(), "-l"],
            cwd=workdir,
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert proc.returncode == 0, f"-l 应成功退出: {proc.stderr}"
        assert "历史会话" in proc.stdout, proc.stdout
        assert "1." in proc.stdout, "应有列表序号"
        assert "旧会话问题" in proc.stdout, "应含首条 user 消息预览"
    finally:
        shutil.rmtree(home, ignore_errors=True)
        shutil.rmtree(workdir, ignore_errors=True)


def test_session_slash_switches_to_previous_session():
    """/new 后 /session 弹出列表,选中旧会话:回放旧转录,上下文切回旧会话。"""
    home = tempfile.mkdtemp(prefix="latent_e2e_switch_home_")
    workdir = tempfile.mkdtemp(prefix="latent_e2e_switch_cwd_")
    try:
        app = LatentApp(turns=load_scenario("new_session"), home=home, workdir=workdir)
        try:
            app.wait_ready()
            app.sendline("旧会话问题")
            app.expect_text("旧会话的回复")
            app.sendline("/new")
            app.expect_text("new session started")
            app.sendline("新会话问题")
            app.expect_text("新会话的回复")

            # /session list 弹列表:/new 的新会话是最新且预选中,↓ 移到旧会话,Enter 切换
            app.sendline("/session list")
            app.expect_text("切换历史会话")
            app.send_key("down")
            app.send_key("enter")
            app.expect_text("resumed session")

            # 旧转录回放上屏;续聊上下文 = 旧会话(不含 /new 后的新会话消息)
            app.expect_text("旧会话的回复")
            app.sendline("回到旧会话继续")
            app.expect_text("续到新会话的回复")
            bodies = app.wait_for_requests(3)
            payload = str(bodies[2])
            assert "旧会话问题" in payload, "切换后应携带旧会话上下文"
            assert "新会话问题" not in payload, "不应携带已切走会话的上下文"
        finally:
            app.close()
    finally:
        shutil.rmtree(home, ignore_errors=True)
        shutil.rmtree(workdir, ignore_errors=True)


def test_partial_slash_input_opens_variant_page():
    """/sess 直接回车:补全命令名并展开变体选择页;再次回车执行选中变体。"""
    home = tempfile.mkdtemp(prefix="latent_e2e_variant_home_")
    workdir = tempfile.mkdtemp(prefix="latent_e2e_variant_cwd_")
    try:
        app = LatentApp(turns=[], home=home, workdir=workdir)
        try:
            app.wait_ready()
            app.sendline("/sess")
            # 第一次回车只展开变体页,不执行(不会出现用法提示)
            app.expect_absent(r"用法: /session")
            # 第二次回车执行高亮的 list 变体 → 打开历史会话选择器
            app.send_key("enter")
            app.expect_text("Enter 续聊")
            app.send_key("esc")
        finally:
            app.close()
    finally:
        shutil.rmtree(home, ignore_errors=True)
        shutil.rmtree(workdir, ignore_errors=True)
