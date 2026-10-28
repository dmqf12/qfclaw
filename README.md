# qfclaw

基于 Rust 的 AI Agent，通过 Telegram Bot 接入。支持工具调用、话题（Topic）多会话隔离、后台任务。

## 特性

- **Telegram Bot 接入**：长轮询（getUpdates），无需公网 IP / 域名 / 证书
- **话题即会话**：每个 Telegram Topic（含私聊话题）对应一份独立上下文，互不干扰
- **工具调用**：执行命令、读写/编辑文件、管理后台任务、发送文件
- **消息处理**：超长回复按 `<cut/>` 自动分段发送、长内容折叠、临时消息自动删除、Markdown 失败自动降级

## 环境要求

| 依赖 | 说明 |
| --- | --- |
| Rust 1.85+ | 项目使用 edition 2024 |

## 目录结构

运行目录（示例 `~/qfclaw`）：

```
~/qfclaw/
├── qfclaw                 # 编译产物
├── config/
│   ├── bot.json           # Telegram 配置
│   ├── models.json        # 模型配置
│   ├── Soul.md            # 人设：核心设定
│   ├── Agent.md           # 人设：行为准则
│   ├── User.md            # 人设：用户信息
│   └── telegram/
│       └── file_id.json   # 发送文件的 file_id 缓存（自动生成）
├── messages/              # 会话文件，一个对话一个文件
├── workspace/             # 进程工作目录，工具的根目录
│   └── SKILL.md           # 技能索引
└── qfclawtask/            # exec 的临时执行目录（启动时清空）
```

## 配置

### config/bot.json

```json
{
  "telegram": {
    "token": "1234567890:ABCdefGHIJKlmNop...",
    "allow_user_id": 123456789,
    "base_url": "https://api.telegram.org/bot",
    "bot_id": 0,
    "commands": [
      { "command": "new", "description": "✅开启新对话" }
    ]
  }
}
```

| 字段 | 说明 |
| --- | --- |
| `token` | **必填**，@BotFather 获取 |
| `allow_user_id` | **必填**，允许使用机器人的 Telegram 用户 ID（白名单，其他人会被拒绝并回显其 ID） |
| `base_url` | Bot API 地址，默认官方地址；自建网关可改 |
| `commands` | 启动时通过 `setMyCommands` 注册的指令列表 |
| `bot_id` | 预留，当前未使用 |

### config/models.json

```json
{
  "config": {
    "reasoning": "enabled",
    "stream": false
  },
  "deepseek": {
    "base_url": "https://api.deepseek.com/v1",
    "model_name": "deepseek-flash",
    "token": ""
  },
  "use_model": "deepseek"
}
```

| 字段 | 说明 |
| --- | --- |
| `use_model` | 当前使用的模型块名 |
| `<模型名>.base_url` | OpenAI 兼容接口地址 |
| `<模型名>.model_name` | 模型名 |
| `<模型名>.token` | API Key |
| `config.reasoning` | 推理模式：`enabled` / `adaptive` / `disabled`，由 `/reasoning` 按钮修改 |
| `config.show_reasoning` | 由 `/reasoning` 按钮写入，当前版本未使用 |
| `config.stream` | 预留，当前未使用 |

### 人设文件

`config/Soul.md`、`Agent.md`、`User.md` 会按顺序拼接成 system prompt，修改后下一次请求生效（无需重启）。

## 构建与部署

```bash
cargo build --release
```

产物 `target/release/qfclaw` 放到运行目录（见上方目录结构）后启动即可。

进程的工作目录就是 `workspace/`，配置与会话都按相对路径查找，因此**必须在运行目录下启动**，或用常驻方式（systemd / tmux 等）把工作目录固定下来。`/restart` 指令依赖 `systemctl --user restart qfclaw`，如果不用 systemd 托管，需要自行调整。

## 指令

| 指令 | 说明 |
| --- | --- |
| `/new [标题]` | 新建对话。话题模式下创建新 Topic；不支持话题时清空当前对话 |
| `/clear` | 删除当前对话：消息文件移到 `messages/clear/`，话题模式下一并删除该话题 |
| `/status` | 显示话题标题与上下文用量 |
| `/reasoning` | 选择推理模式（隐藏 / 折叠 / 关闭 / 适应） |
| `/stop` | 停止当前对话正在运行的任务 |
| `/restart` | 重启服务（`systemctl --user restart qfclaw`） |

## 会话机制

会话键有两种形式：

- 普通聊天：`<chat_id>`
- 话题（Forum Topic / 私聊话题）：`<chat_id>:<thread_id>`

每个会话键对应一个文件 `messages/<会话键>.json`，结构：

```json
{
  "create_time": 1730000000,
  "title": "话题标题",
  "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 },
  "messages": []
}
```

- `messages` 不保存 system prompt，每次请求时重新拼接
- `/clear` 会把当前会话文件移到 `messages/clear/`（同名时追加时间戳），话题模式下还会删除该话题；之后的对话从空白开始
- 写入为「临时文件 + rename」原子操作
- 每个会话独立运行一个对话任务：A 话题生成中收到 B 话题的消息互不阻塞
- 同一会话运行中收到新消息，会打断当前回复并把新消息合并进去重新请求

话题标题来自 Telegram 的 `forum_topic_created` / `forum_topic_edited` 服务消息；机器人不在线时创建的旧话题拿不到标题。

## 工具

模型可调用以下工具：

| 工具 | 说明 |
| --- | --- |
| `exec` | 执行 shell 命令。超时（默认 10s）后转入后台，返回任务 ID |
| `operate_task` | 查看后台任务输出（`check`）或终止任务（`kill`） |
| `operate_file` | 读取（支持 offset/limit）、写入、删除文本文件 |
| `edit_file` | 精确替换文件中的一段文本，`old_string` 必须唯一 |
| `send_file` | 发送图片（≤10MB）/ 其他文件（≤50MB） |

工具的输出会被截断到最新 8000 字符，避免占用过多上下文。

## 消息长度

Telegram 单条文本上限 4096 字符（UTF-16 码元）。模型被要求在超长回复中用 `<cut/>` 标记分段，发送时按标签拆成多条消息依次发出（按钮只挂在最后一条）。

如果模型没打标签但内容仍然超长，会兜底按行切分、单行过长再按字符切分；折叠模式（MarkdownV2 转义会膨胀）使用更保守的阈值 3500。

## 技能

`workspace/SKILL.md` 是技能索引，具体脚本放在 `workspace/skill/<名称>/` 下，由模型按需通过 `exec` 调用。

## 注意事项

1. **白名单**：仅 `allow_user_id` 指定的用户可使用；其他人发消息会收到自己的 ID 方便配置。
2. **群聊 / 话题**：需要在 @BotFather 关闭 privacy mode，否则机器人收不到普通消息；群里的论坛话题还需授予「Manage Topics」权限。
3. **工作目录共享**：所有会话的 `exec` 和文件工具都在同一个 `workspace/` 下，会话之间不隔离文件与进程。

## 许可证

LGPL-2.1，详见 [LICENSE](LICENSE)。
