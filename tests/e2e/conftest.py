"""pytest 配置:把本目录加入 import 路径,使测试能 import harness/mock_llm。"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
