"""平行子 agent 会话的谱系独立:/session list 在子会话内列出**该 agent 自己**
的历史会话并恢复,与主会话谱系互不可见;恢复后续聊上下文只含子会话历史。
"""

import shutil
import tempfile

from harness import LatentApp, load_scenario

SCOUT_DEF = """---
name: scout
description: 侦查用平行 agent
---

You are scout.
"""


def test_subagent_session_list_scopes_to_child_lineage():
    """第一次运行:主会话问答 → /subagent → 子会话问答。
    第二次运行:/subagent → /session list 列出 scout 自己的上次会话 → 恢复:
    历史回放上屏,续聊上下文含子会话历史、不含主会话历史。"""
    home = tempfile.mkdtemp(prefix="latent_e2e_sub_sess_home_")
    workdir = tempfile.mkdtemp(prefix="latent_e2e_sub_sess_cwd_")
    # agent 定义(项目级)
    agents_dir = workdir + "/.latent/agents"
    import os

    os.makedirs(agents_dir, exist_ok=True)
    with open(agents_dir + "/scout.md", "w") as f:
        f.write(SCOUT_DEF)
    try:
        first = LatentApp(
            turns=load_scenario("subagent_session"), home=home, workdir=workdir
        )
        try:
            first.wait_ready()
            first.sendline("主会话问题")
            first.expect_text("主会话的回复")
            # 切到 scout:选择器首项即 scout
            first.sendline("/subagent")
            first.expect_text("选择 subagent")
            first.send_key("enter")
            first.expect_text("已切换到 subagent: scout")
            # 子会话内问答(落 scout 谱系文件)
            first.sendline("侦查问题")
            first.expect_text("侦查的回复")
            first.quit()
            assert first.exit_status == 0
        finally:
            first.close()

        second = LatentApp(
            turns=load_scenario("subagent_session_second"), home=home, workdir=workdir
        )
        try:
            second.wait_ready()
            second.sendline("/subagent")
            second.expect_text("选择 subagent")
            second.send_key("enter")
            second.expect_text("已切换到 subagent: scout")
            # /session list:列出 scout 自己的谱系(新会话预选中,↓ 选上次会话)
            second.sendline("/session list")
            second.expect_text("切换历史会话")
            second.send_key("down")
            second.send_key("enter")
            second.expect_text("resumed session")
            # 历史回放上屏(子会话谱系)
            second.expect_text("侦查的回复")
            # 恢复后续聊:上下文 = scout 历史,不含主会话历史(谱系独立)
            second.sendline("恢复后问题")
            second.expect_text("恢复后的回复")
            bodies = second.wait_for_requests(1)
            payload = str(bodies[0])
            assert "侦查问题" in payload, "续聊应携带 scout 谱系历史"
            assert "主会话问题" not in payload, "不应看到主会话谱系上下文"
        finally:
            second.close()
    finally:
        shutil.rmtree(home, ignore_errors=True)
        shutil.rmtree(workdir, ignore_errors=True)
