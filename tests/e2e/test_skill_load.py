"""skill 的 developer 角色注入(skills::expand 两条路径):

1. 模型调用 load_skill 工具:第二轮请求里工具结果换成短确认,SKILL.md 全文
   以 developer 角色(Anthropic wire 上并入相邻 user 消息)紧随其后;
2. `/skill <名称>` 伪命令(与工具调用无关):输入 `/sk` 补全 → 技能变体
   过滤 → 选定后继续输入正文 → 提交;首轮请求里 developer 注入在剥前缀的
   用户消息之前。

夹具:测试自建 workdir 与 `.latent/skills/dev-loop/SKILL.md`,自行清理。
"""

import os
import shutil
import tempfile

from harness import LatentApp, load_scenario

SKILL_BODY = "Follow the loaded instructions."


def make_workdir() -> str:
    workdir = tempfile.mkdtemp(prefix="latent_e2e_skill_")
    skill_dir = os.path.join(workdir, ".latent", "skills", "dev-loop")
    os.makedirs(skill_dir)
    with open(os.path.join(skill_dir, "SKILL.md"), "w", encoding="utf-8") as f:
        f.write("---\ndescription: 开发循环指引\n---\n" + SKILL_BODY)
    return workdir


def test_load_skill_tool_result_becomes_developer_message():
    workdir = make_workdir()
    try:
        app = LatentApp(turns=load_scenario("skill_load"), workdir=workdir)
        try:
            app.wait_ready()
            app.sendline("按技能流程来")
            app.expect_text(r"已按 dev-loop 技能指引处理")

            bodies = app.wait_for_requests(2)
            second = str(bodies[1])
            # 工具结果换短确认,正文以 developer 角色紧随(不再双份)
            assert "Skill loaded; its full instructions follow" in second, second
            assert "<skill name=\"dev-loop\">" in second, second
            assert SKILL_BODY in second, second
        finally:
            app.close()
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def test_skill_prefix_popup_completes_and_loads_without_tool_call():
    workdir = make_workdir()
    try:
        app = LatentApp(turns=[{"text": "好的,已按指引处理。"}], workdir=workdir)
        try:
            app.wait_ready()

            # /sk → 命令条目 → Enter 展开技能变体列表(req:前缀过滤)
            app.type_text("/sk")
            app.expect_visible(r"/skill")
            app.send_key("enter")
            app.expect_visible("dev-loop")
            app.expect_visible("开发循环指引")

            # Enter 补全首个变体,停留编辑(不发送)
            app.send_key("enter")
            app.expect_visible(r"/skill dev-loop")

            # 继续输入正文后提交;弹窗随正文隐藏
            app.type_text(" hello")
            app.send_key("enter")
            app.expect_text(r"已按指引处理")

            bodies = app.wait_for_requests(1)
            body = str(bodies[0])
            # developer 注入在用户消息之前,前缀被剥掉,全程无工具调用
            assert "<skill name=\"dev-loop\">" in body, body
            assert SKILL_BODY in body, body
            assert "hello" in body, body
            assert "/skill dev-loop" not in body, "前缀不应进入模型上下文"
        finally:
            app.close()
    finally:
        shutil.rmtree(workdir, ignore_errors=True)
