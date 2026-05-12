# qfclaw

目前仅支持通过 Telegram Bot 接入使用。

## 创建 Telegram Bot

1. 在 Telegram 中联系 @BotFather
2. 发送 /newbot 并按提示设置机器人的名称和用户名
3. BotFather 会给你一个 API Token，格式类似：
      1234567890:ABCdefGHIJKLMNopqrsTUVWXYZ-abc123

配置文件

编辑 config/bot.json：

```json
{
  "token": "你的:BotToken",
  "allow_id": 123456789
}
```
token：必填，从 BotFather 获得

allow_id：允许使用该 Bot 的 Telegram 用户 ID，给机器人发送任意消息获取