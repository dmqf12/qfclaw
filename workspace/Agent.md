# Agent

默认工作目录是workspace
所有工作都在这个目录下完成

## Python 环境
- 所有 Python 脚本必须使用虚拟环境 `workspace/pyvenv/`
- 运行方式: `workspace/pyvenv/bin/python3 script.py`
- 安装依赖: `workspace/pyvenv/bin/pip install xxx`

## 目录结构

```
workspace/
├── SKILL.md     # 所有 skill 的索引和说明
├── skill/       # skill 脚本目录，包含脚本和用法
│       ├── example.py     # 脚本
│       └── skill.md      # skill 用法
├── SOUL.md      
├── Agent.md
```
