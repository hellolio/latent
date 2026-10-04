"""计划模式与审批流(13 文档 §13 L3):新会话默认 Plan(只读 + 请求级模式指令消息),
write 被拒绝且不弹审批;模型输出 <proposed_plan> 渲染为计划卡片;
/mode confirm 后再次 write 触发审批 overlay,数字键批准后工具真实执行。"""

import os
import shutil
import tempfile
import time

from harness import LatentApp, load_scenario


def _wait_screen(app, pattern: str, timeout: float = 10.0) -> None:
    """轮询当前屏幕(不含历史输出)直到出现 pattern。"""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if pattern in app.visible_text():
            return
        time.sleep(0.05)
    raise AssertionError(f"屏幕上未出现 {pattern!r}:\n{app.visible_text()}")


def test_plan_mode_blocks_writes_then_approval_flow():
    home = tempfile.mkdtemp(prefix="latent_e2e_plan_home_")
    workdir = tempfile.mkdtemp(prefix="latent_e2e_plan_cwd_")
    try:
        app = LatentApp(
            turns=load_scenario("plan_mode"),
            home=home,
            workdir=workdir,
            session_mode=None,  # 新会话默认 Plan(本用例的被测行为)
        )
        try:
            app.wait_ready()

            # 新会话默认 Plan:模式指令以持久 ModeSection 消息 append 在转录
            # (位置固定,append-only 保 KV 缓存前缀;文本为 Plan 进入句)
            app.sendline("帮我写一个文件")
            bodies = app.wait_for_requests(1)
            assert (
                "You are entering Plan mode" in str(bodies[0])
            ), "Plan 模式请求应携带模式指令消息"
            # system 提示词与模式解耦:不再包含 <mode> 节
            assert "<mode>" not in str(bodies[0]), "system 提示词不应再含 <mode> 节"

            # 变更类 bash 命令被权限引擎直接拒绝(Deny,不是审批;
            # rm 命中明确写前缀表,即使有沙箱也不放行),
            # 模型收到拒绝原因(write/edit 则在工具集层被收掉,不会进请求)
            app.expect_text("Plan mode blocks commands")
            target = os.path.join(workdir, "e2e-plan-mode.txt")
            assert not os.path.exists(target), "Plan 模式不应产生文件写入"

            # 模型输出 <proposed_plan>:渲染为计划卡片(标题 + 引导行)
            app.expect_text("实施计划")
            app.expect_text("mode confirm")

            # /mode confirm:切换会话模式(不打转录提示,状态栏标记变化)
            app.sendline("/mode confirm")
            _wait_screen(app, "confirm")

            # 重发写入请求:变更类 bash 触发审批 overlay,数字键 1 批准一次
            # (批准的命令在 WorkspaceWrite 沙箱内执行,可写根 = cwd)
            app.sendline("继续,按计划执行")
            # 第三次请求(新回合):旧进入句仍经历史携带(位置固定不动),
            # /mode confirm 时追加的退出句也在请求中 —— append-only 验收点
            bodies = app.wait_for_requests(3)
            tail = str(bodies[2])
            assert (
                "You are entering Plan mode" in tail
            ), "旧模式节应持久留在历史中随请求携带"
            assert "You are exiting Plan mode" in tail, "切换模式后应追加新节点"
            app.expect_text("审批 bash")
            app.sendline("1")

            # 批准后工具真实执行,文件落盘
            app.expect_text("文件已写入")
            assert os.path.exists(target), "批准后 write 应真实执行"
            with open(target, encoding="utf-8") as f:
                assert f.read().strip() == "written after approval"
        finally:
            app.close()
    finally:
        shutil.rmtree(home, ignore_errors=True)
        shutil.rmtree(workdir, ignore_errors=True)


def test_plan_mode_status_bar_marker():
    """状态栏显示当前模式标记;Plan 用黄色 plan 标记。"""
    app = LatentApp(turns=load_scenario("plan_mode"), session_mode=None)
    try:
        app.wait_ready()
        text = app.visible_text()
        assert "plan" in text, f"状态栏应显示 plan 标记: {text}"
    finally:
        app.close()


def test_shift_tab_cycles_session_mode():
    """Shift+Tab(终端 CSI Z)循环切换会话模式:plan → confirm → full-access。"""
    app = LatentApp(turns=load_scenario("plan_mode"), session_mode=None)
    try:
        app.wait_ready()
        assert "plan" in app.visible_text()
        # Shift+Tab 一档:plan → confirm(无转录提示,状态栏标记变化)
        # 断言轮询当前屏幕:历史输出里含启动时的旧标记,不能用全量匹配
        app.child.send("\x1b[Z")
        _wait_screen(app, "confirm")
        # 再一档:confirm → full-access
        app.child.send("\x1b[Z")
        _wait_screen(app, "full-access")
        # 再一档回到 plan
        app.child.send("\x1b[Z")
        _wait_screen(app, "plan")
    finally:
        app.close()
