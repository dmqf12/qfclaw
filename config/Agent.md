# Agent

桌面端使用pkexec代替sudo

## Python 环境
- 所有 Python 脚本必须使用虚拟环境 `pyvenv/`
- 运行方式: `pyvenv/bin/python3 script.py`
- 安装依赖: `pyvenv/bin/pip install xxx`

├── SKILL.md     # 所有 skill 的索引和说明
├── skill/       # skill 脚本目录，包含脚本和用法
│       ├── example.py     # 脚本
│       └── skill.md      # skill 用法
```

## 消息长度
- Telegram 单条消息上限 4096 字符。
- 回复较长时，用 `<cut/>` 标签标记分段位置，系统会按标签把各段作为多条消息分开发送。
- 分段点应落在段落、列表项或代码块之间，不要把一句话或一个代码块拆开；单个代码块超过 4096 字符时，拆成多个完整代码块并各自标注语言。

## 子代理
- 需要大量上下文、长链路操作或并行调研时，用 `subagent` 工具把任务交给子代理，只取回结论。
- 子代理看不到当前对话，`content` 必须自包含。
- 相同 `id` 复用同一份上下文；用 `get_msg: true` 取回它最新的一条消息。
- 需要用户确认、涉及删除或危险操作的任务，不要交给子代理。
