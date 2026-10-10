"""E2E:bash 长命令自动转后台 + task_status 三动作(status/result/kill)。

场景(backgroundAfterSecs=1 加速转后台):
- test_background_result_flow:转后台立即结算(Task started / Task ID);
  任务完成时通知(纯指引,不含输出)在运行中的下一轮边界注入,模型据此
  result 取到结果并消费(输出只经 fetch 获得,通知里没有 late-marker);
- test_background_kill:kill 终止运行中的任务并消费,不产生完成通知;
- test_completion_notification:模型未主动查询 → 完成通知作为 user 消息
  注入并触发新一轮回复。
"""

from harness import LatentApp, load_scenario

SETTINGS_FAST_BG = {"backgroundAfterSecs": 1}


def test_background_result_flow():
    app = LatentApp(turns=load_scenario("background_task"), settings=SETTINGS_FAST_BG, timeout=30)
    try:
        app.wait_ready()
        app.sendline("后台跑一个任务")
        # 转后台:立即结算,消息只含任务开始 + task id(不带输出)
        app.expect_text("Task started in background", timeout=20)
        app.expect_text("Task ID: bash-1")
        # status 查询(任务未结束)
        app.expect_text("running")
        # 任务完成:通知(纯指引)在下一轮边界注入,不含任务输出
        app.expect_text(r"background task bash-1 finished successfully", timeout=20)
        # 模型按指引 result 取结果 → 此时才见到任务输出
        app.expect_text("late-marker", timeout=20)
        app.expect_text("后台任务已确认完成")
    finally:
        app.close()


def test_background_kill():
    app = LatentApp(turns=load_scenario("background_kill"), settings=SETTINGS_FAST_BG, timeout=30)
    try:
        app.wait_ready()
        app.sendline("后台跑一个长任务然后终止它")
        app.expect_text("Task started in background", timeout=20)
        app.expect_text("Task ID: bash-1")
        app.expect_text("killed", timeout=20)
        app.expect_text("后台任务已终止")
        # kill 即消费 → 不产生完成通知
        app.expect_absent(r"\[latent\] background task", after_idle=1.0)
    finally:
        app.close()


def test_completion_notification():
    app = LatentApp(turns=load_scenario("background_notify"), settings=SETTINGS_FAST_BG, timeout=30)
    try:
        app.wait_ready()
        app.sendline("后台跑一个任务")
        app.expect_text("Task started in background", timeout=20)
        # 模型未查询:任务结束后完成通知作为 user 消息注入并触发新回复
        app.expect_text(r"background task bash-1 finished successfully", timeout=20)
        app.expect_text("收到完成通知")
    finally:
        app.close()
