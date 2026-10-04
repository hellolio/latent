"""T10:/model 配置入口——选择器候选与配置项(添加模型 / 编辑 models.json)。

覆盖 models.json 顶层 `showBuiltinModels: false` 隐藏内置 provider 默认表、
裸 /model 打开选择器的候选内容与末尾配置条目、Esc 关闭、$EDITOR 编辑后
热重载(经 LATENT_EDITOR 注入脚本,避免依赖真实编辑器)。
"""

import json
import os
import time

from harness import LatentApp

# 内置 provider 默认表里的一个代表(anthropic 的默认模型),用于断言
# showBuiltinModels 开关是否生效
BUILTIN_REPRESENTATIVE = "anthropic/claude-sonnet-4-5"


def test_model_selector_shows_builtin_defaults_by_default():
    app = LatentApp(turns=[])
    try:
        app.wait_ready()
        app.sendline("/model")
        app.expect_text("选择模型")
        # 默认(未配置 showBuiltinModels):内置 provider 默认表在列
        app.expect_text(BUILTIN_REPRESENTATIVE)
        # 末尾两个配置入口
        app.expect_text("添加模型")
        app.expect_text("models.json")
        # Esc 关闭选择器
        app.send_key("esc")
        time.sleep(0.3)
        assert "选择模型" not in app.visible_text(), "Esc 应关闭选择器"
    finally:
        app.close()


def test_model_selector_hides_builtin_models_when_configured():
    app = LatentApp(turns=[], show_builtin_models=False)
    try:
        app.wait_ready()
        app.sendline("/model")
        app.expect_text("选择模型")
        # 只显示配置的模型 + 两个配置入口
        app.expect_text("e2e/e2e-model")
        app.expect_text("添加模型")
        app.expect_text("models.json")
        # 内置默认表被隐藏
        app.expect_absent(BUILTIN_REPRESENTATIVE)
        app.send_key("esc")
        time.sleep(0.3)
        assert "选择模型" not in app.visible_text(), "Esc 应关闭选择器"
        # 带参数切换仍然可用
        app.sendline("/model e2e/e2e-model")
        time.sleep(0.5)
        assert "切换失败" not in app.transcript()
    finally:
        app.close()


def test_model_editor_entry_reloads_config(tmp_path):
    """选中「编辑 models.json」→ LATENT_EDITOR 脚本注入新 provider → 热重载
    → 选择器重开并显示新候选(挂起/恢复 + 热重载全链路)。"""
    script = tmp_path / "fake_editor.sh"
    script.write_text(
        "#!/bin/sh\n"
        'python3 - "$1" <<\'PYEOF\'\n'
        "import json, sys\n"
        "path = sys.argv[1]\n"
        "with open(path, encoding='utf-8') as f:\n"
        "    data = json.load(f)\n"
        'data.setdefault("providers", {})["extra"] = {\n'
        '    "baseUrl": "http://127.0.0.1:9/v1",\n'
        '    "api": "openai-completions",\n'
        '    "apiKey": "EXTRA_KEY",\n'
        '    "models": [{"id": "extra-model", "name": "extra-model"}],\n'
        "}\n"
        "with open(path, 'w', encoding='utf-8') as f:\n"
        "    json.dump(data, f)\n"
        "PYEOF\n",
        encoding="utf-8",
    )
    script.chmod(0o755)
    old_editor = os.environ.get("LATENT_EDITOR")
    os.environ["LATENT_EDITOR"] = str(script)
    app = LatentApp(turns=[])
    try:
        app.wait_ready()
        app.sendline("/model")
        app.expect_text("选择模型")
        # 一路 Down 到末位「编辑 models.json」条目(选择列表移动饱和)
        for _ in range(20):
            app.send_key("down")
        time.sleep(0.2)
        app.send_key("enter")
        # 编辑器退出后:重载提示 + 选择器重开,新候选在列
        app.expect_text("已重载")
        app.expect_text("extra/extra-model")
        app.send_key("esc")
        time.sleep(0.3)
    finally:
        if old_editor is None:
            os.environ.pop("LATENT_EDITOR", None)
        else:
            os.environ["LATENT_EDITOR"] = old_editor
        app.close()
