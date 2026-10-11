"""T28:/context —— 查看/切换模型上下文窗口(写 models.json,当前会话即时生效)。

harness 的 models.json 给 e2e/e2e-model 显式写着 `contextWindow: 32000`;
本测试覆盖:裸 /context 开四档选择器并标出当前档、带档位直接写入并刷新
footer ctx 段分母、「默认」档删键回落 models.json 记载值。

注:启动时 harness 只传 `--provider e2e`(裸 provider),这种解析走
`Model::minimal` 缺省窗口 = 128k,读不到模型条目的 32000;而 /context 一律
按 `provider/id` 完整 spec 重解析,所以「默认」档确实会落到 32000。
"""

import json
import os
import time

from harness import LatentApp


def test_context_command_switches_window_and_persists():
    # 宽 160:footer 右侧用量段(含 ctx 分母)不被左侧 cwd/模式段挤掉
    app = LatentApp(turns=[{"text": "第一轮回复"}, {"text": "第二轮回复"}], cols=160)
    try:
        app.wait_ready()

        # 裸 /context:四档选择器,标出当前档(当前窗口 = 缺省 128k → ❯ 在
        # 「128k」行;提示行还写出当前值)
        app.sendline("/context")
        app.expect_visible(r"选择上下文窗口\(当前\s*128000\s*tokens")
        for preset in ("默认", "128k", "256k", "1m"):
            app.expect_visible(preset)
        app.expect_visible(r"❯\s*128k")
        app.send_key("esc")
        time.sleep(0.3)
        assert "选择上下文窗口" not in app.visible_text(), "Esc 应关闭选择器"

        # 带档位:写 models.json 并即时生效
        app.sendline("/context 256k")
        app.expect_text(r"context\s*→\s*256000\s*tokens")

        # 生效:发一轮消息后 footer ctx 段的分母变 256k;请求仍能到达 mock
        # (项目 models.json 只补 contextWindow,provider 端点经全局配置合并
        # 保留 —— 若被项目文件遮蔽,这里的请求会失败)
        app.sendline("你好")
        app.expect_text("第一轮回复")
        app.expect_text(r"/\s*256\s*k\s*\)")
        assert len(app.wait_for_requests(1)) == 1, "切换窗口后请求应正常发出"

        # 写盘落在项目 .latent/models.json
        project_models = os.path.join(app.workdir, ".latent", "models.json")
        with open(project_models, encoding="utf-8") as f:
            saved = json.load(f)
        assert saved["providers"]["e2e"]["models"][0]["contextWindow"] == 256000

        # 「默认」档:删键,回落全局 models.json 的 32000
        app.sendline("/context default")
        app.expect_text(r"context\s*→\s*32000\s*tokens")
        with open(project_models, encoding="utf-8") as f:
            saved = json.load(f)
        assert "contextWindow" not in saved["providers"]["e2e"]["models"][0]

        # 未知档位:本地报错,不写盘
        app.sendline("/context 300k")
        app.expect_text("未知上下文档位")
        assert len(app.request_bodies()) == 1, "本地斜杠命令不产生 LLM 请求"
    finally:
        app.close()
