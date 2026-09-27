"""T20:会话 JSONL 半行崩溃恢复——半行被隔离,--continue 后新消息正常追加。

直接构造损坏的 session 文件(模拟写入中途崩溃),钉住 load 修复逻辑(P1-1)。
"""

import glob
import json
import os
import shutil
import tempfile

from harness import RpiApp, load_scenario


def test_half_line_in_session_file_is_isolated():
    home = tempfile.mkdtemp(prefix="rpi_e2e_half_home_")
    workdir = tempfile.mkdtemp(prefix="rpi_e2e_half_cwd_")
    try:
        # 第一段正常会话
        first = RpiApp(turns=load_scenario("continue_first"), home=home, workdir=workdir)
        try:
            first.wait_ready()
            first.sendline("你好")
            first.expect_text("第一次的固定回复")
            first.quit()
        finally:
            first.close()

        # 模拟崩溃:留下末尾无换行的半行
        sessions = glob.glob(os.path.join(home, ".rpi", "sessions", "*.jsonl"))
        assert sessions, "会话文件应已落盘"
        session_file = sessions[0]
        with open(session_file, "ab") as f:
            f.write(b'{"type":"mess')

        # --continue 恢复:半行按损坏行跳过,历史可见,可继续对话
        second = RpiApp(
            turns=load_scenario("continue_second"),
            home=home,
            workdir=workdir,
            extra_args=["--continue"],
        )
        try:
            second.wait_ready()
            second.expect_text("第一次的固定回复")
            second.sendline("恢复后还在吗")
            second.expect_text("续聊后的回复")
        finally:
            second.close()

        # 文件里不存在"半行 + 新 entry 拼接"的行:每行要么完整,要么就是被隔离的半行
        with open(session_file, encoding="utf-8", errors="replace") as f:
            lines = f.read().splitlines()
        concatenated = [l for l in lines if l.startswith('{"type":"mess{"')]
        assert not concatenated, f"半行应被补换行隔离,不允许拼接: {concatenated[:1]}"
        # 恢复会话写入的新消息可被完整解析
        parsed = []
        for line in lines:
            try:
                parsed.append(json.loads(line))
            except json.JSONDecodeError:
                pass
        assert any('"恢复后还在吗"' in json.dumps(e, ensure_ascii=False) for e in parsed), \
            "恢复后追加的新消息应可完整解析"
    finally:
        shutil.rmtree(home, ignore_errors=True)
        shutil.rmtree(workdir, ignore_errors=True)
