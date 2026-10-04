"""场景脚本:E2E 测试用的 mock LLM 响应序列(格式见 mock_llm.py 模块注释)。

每个文件是一个 JSON 数组,按 latent 发请求的顺序逐 turn 消费:
  {"text": "...", "delay_ms": 0}                                纯文本回复
  {"tool_calls": [{"name": "...", "arguments": {...}}]}          工具调用回复
  {"error": "...", "status": 500}                                provider 错误
"""
