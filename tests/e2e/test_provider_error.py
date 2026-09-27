"""T13:provider 错误上屏——HTTP 500 自动重试后错误可见,会话仍可用。"""

from harness import RpiApp, load_scenario


def test_provider_error_displayed_and_session_usable():
    app = RpiApp(turns=load_scenario("provider_error"), timeout=40.0)
    try:
        app.wait_ready()
        app.sendline("触发一个错误")
        # 错误上屏(重试耗尽后,会话层包装文案)
        app.expect_text("Error: Retry failed", timeout=30.0)
        # 500 可重试:默认 max_retries=3,应共发出 4 次请求(首次 + 3 重试)
        bodies = app.wait_for_requests(4, timeout=30.0)
        assert len(bodies) >= 4, f"应自动重试,实际请求数 {len(bodies)}"
        # 错误后继续对话:走新一轮
        app.sendline("还能继续吗")
        app.expect_text("错误后仍能继续对话")
        all_bodies = app.wait_for_requests(5, timeout=30.0)
        assert "还能继续吗" in str(all_bodies[-1])
    finally:
        app.close()
