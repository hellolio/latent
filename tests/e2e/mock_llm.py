"""mock_llm.py —— 本地 mock LLM 服务(E2E 测试用)。

伪装成 anthropic-messages 端点(POST /v1/messages + SSE),按场景脚本逐 turn
返回预设响应,让真实 latent 二进制的 E2E 测试完全确定、不依赖网络与 API key。
响应格式对齐 crates/latent-ai/tests/adapters.rs 里的本地 SSE 服务器。

场景脚本是一个 turn 列表(每次收到 POST 消费一个,按顺序):

    [
        {"text": "你好,这是固定回复", "delay_ms": 0},
        {"text": "长回复", "chunk_delay_ms": 120},  # delta 间逐块延迟,模拟慢速流式
        {"tool_calls": [{"name": "read", "arguments": {"path": "a.txt"}}]},
        {"error": "模拟的 provider 错误", "status": 500},
    ]

服务同时把收到的每个请求体(JSON)记录在 `.requests` 里,测试可以反向断言
latent 发出的内容(如第二轮是否带上了工具结果)。

只依赖标准库;`threading` 实现并发,`delay_ms` 直接 sleep 即可。
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODEL_ID = "e2e-model"


def text_turn_events(message_id: int, text: str, chunk_size: int = 8) -> list[dict]:
    """一个纯文本回复的 SSE 事件序列(按 chunk 拆分模拟流式)。"""
    events: list[dict] = [
        {
            "type": "message_start",
            "message": {
                "id": f"msg_{message_id}",
                "model": MODEL_ID,
                "usage": {"input_tokens": 10, "output_tokens": 1},
            },
        },
        {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
    ]
    chunks = [text[i : i + chunk_size] for i in range(0, len(text), chunk_size)] or [""]
    for chunk in chunks:
        events.append(
            {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": chunk}}
        )
    events += [
        {"type": "content_block_stop", "index": 0},
        {"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 7}},
        {"type": "message_stop"},
    ]
    return events


def tool_turn_events(message_id: int, calls: list[dict]) -> list[dict]:
    """一个 tool_use 回复的 SSE 事件序列(arguments 整体作为一条 json delta)。"""
    events: list[dict] = [
        {
            "type": "message_start",
            "message": {
                "id": f"msg_{message_id}",
                "model": MODEL_ID,
                "usage": {"input_tokens": 10, "output_tokens": 1},
            },
        }
    ]
    for index, call in enumerate(calls):
        arguments = json.dumps(call.get("arguments", {}), ensure_ascii=False)
        events += [
            {
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "tool_use", "id": f"toolu_{index}", "name": call["name"], "input": {}},
            },
            {
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "input_json_delta", "partial_json": arguments},
            },
            {"type": "content_block_stop", "index": index},
        ]
    events += [
        {"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 5}},
        {"type": "message_stop"},
    ]
    return events


def sse_body(events: list[dict]) -> bytes:
    return "".join(f"data: {json.dumps(event, ensure_ascii=False)}\n\n" for event in events).encode("utf-8")


class _Handler(BaseHTTPRequestHandler):
    """处理 POST /v1/messages:记录请求 → 取下一个 turn → 回 SSE。"""

    protocol_version = "HTTP/1.1"

    def log_message(self, format, *args):  # noqa: A002 - 静默默认访问日志
        pass

    def do_POST(self):  # noqa: N802 - BaseHTTPRequestHandler 命名约定
        server = self.server.mock_llm  # type: MockLLM
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            body = {"_raw": raw.decode("utf-8", errors="replace")}
        server._record({"path": self.path, "headers": dict(self.headers), "body": body})

        turn = server._next_turn()
        if turn is None:
            self._respond(500, json.dumps({"error": {"type": "server_error", "message": "mock script exhausted"}}))
            return

        delay_ms = turn.get("delay_ms", 0)
        if delay_ms:
            # 分块写出期间的延迟由测试在事件里自定;这里统一在响应前延迟
            import time

            time.sleep(delay_ms / 1000.0)

        if "error" in turn:
            payload = json.dumps({"error": {"type": "api_error", "message": turn["error"]}})
            self._respond(turn.get("status", 500), payload)
            return

        server._message_id += 1
        if "text" in turn:
            events = text_turn_events(server._message_id, turn["text"])
        elif "tool_calls" in turn:
            events = tool_turn_events(server._message_id, turn["tool_calls"])
        else:
            self._respond(500, json.dumps({"error": {"type": "invalid_request", "message": f"未知 turn: {turn}"}}))
            return

        chunk_delay_ms = turn.get("chunk_delay_ms", 0)
        if chunk_delay_ms:
            self._respond_sse_paced(events, chunk_delay_ms)
        else:
            self._respond_sse(sse_body(events))

    def _respond(self, status: int, payload: str) -> None:
        data = payload.encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _respond_sse(self, body: bytes) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _respond_sse_paced(self, events: list[dict], delay_ms: int) -> None:
        """逐事件写出 SSE,delta 事件之间 sleep `delay_ms`(模拟慢速流式)。

        Content-Length 仍是完整 body 长度,客户端按流读取;wfile 无缓冲,
        每个 write 即时到达,latent 的流式 UI 全程保持活跃。
        """
        import time

        body = sse_body(events)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        for event in events:
            self.wfile.write(sse_body([event]))
            if event["type"] == "content_block_delta":
                time.sleep(delay_ms / 1000.0)


class MockLLM:
    """本地 mock LLM 服务:ThreadingHTTPServer + 场景脚本 + 请求记录。"""

    def __init__(self, turns: list[dict]):
        self.turns = list(turns)
        self.requests: list[dict] = []
        self._lock = threading.Lock()
        self._message_id = 0
        self._turn_index = 0
        self._httpd = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
        self._httpd.mock_llm = self  # handler 通过 server 取状态
        self._thread = threading.Thread(target=self._httpd.serve_forever, daemon=True)

    @property
    def port(self) -> int:
        return self._httpd.server_address[1]

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def start(self) -> "MockLLM":
        self._thread.start()
        return self

    def stop(self) -> None:
        self._httpd.shutdown()
        self._httpd.server_close()

    # -- 供 handler / 测试使用 -------------------------------------------------

    def _record(self, request: dict) -> None:
        with self._lock:
            self.requests.append(request)

    def _next_turn(self) -> dict | None:
        with self._lock:
            if self._turn_index >= len(self.turns):
                return None
            turn = self.turns[self._turn_index]
            self._turn_index += 1
            return turn

    def request_bodies(self) -> list[dict]:
        """latent 发来的全部请求体(按时间序)。"""
        with self._lock:
            return [r["body"] for r in self.requests]
