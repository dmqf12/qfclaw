//! Telegram 平台实现。
//!
//! 负责把 Telegram update 转换成 [`InboundMessage`]，
//! 以及把 [`OutboundMessage`] 渲染成 Telegram 消息（含 MarkdownV2 折叠）。

use std::time::Duration;

use anyhow::{bail, Result};
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Client;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::{Button, Callback, Channel, Content, InboundMessage, MsgId, OutboundMessage};


/// Telegram 平台。
pub struct TelegramChannel {
    token: String,
    base_url: String,
    commands: Vec<Value>,
    allow_user_id: String,
    client: Client,
}

impl TelegramChannel {
    pub fn new() -> Result<Self> {
        let json: Value = qffunc::read_json("config/bot.json", "telegram")?;
        let token = json["token"].as_str().unwrap_or_default();
        if token.is_empty() {
            bail!("未找到token")
        }
        Ok(Self {
            token: token.to_string(),
            base_url: json["base_url"]
                .as_str()
                .unwrap_or("https://api.telegram.org/bot")
                .to_string(),
            commands: json["commands"].as_array().cloned().unwrap_or_default(),
            allow_user_id: json["allow_user_id"].as_i64().unwrap_or(0).to_string(),
            client: Client::new(),
        })
    }

    /// 调用 Bot API，成功时返回完整响应。
    async fn api(&self, method: &str, body: &Value) -> Result<Value> {
        let url = format!("{}{}/{}", self.base_url, self.token, method);
        let resp = self.client.post(url).json(body).send().await?;
        let value: Value = resp.json().await?;
        if value["ok"].as_bool() == Some(true) {
            Ok(value)
        } else {
            bail!("Telegram API {} 失败: {}", method, value)
        }
    }

    /// 答复按钮回调，消除客户端 loading 状态。
    async fn answer_callback(&self, callback_id: &str) {
        let _ = self
            .api(
                "answerCallbackQuery",
                &json!({ "callback_query_id": callback_id }),
            )
            .await;
    }

    /// 延迟删除消息。
    fn delete_after(&self, chat_id: &str, msg_id: &str, secs: u64) {
        let client = self.client.clone();
        let url = format!("{}{}/deleteMessage", self.base_url, self.token);
        let (chat_id, msg_id) = (chat_id.to_string(), msg_id.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            let _ = client
                .post(url)
                .json(&json!({ "chat_id": chat_id, "message_id": msg_id }))
                .send()
                .await;
        });
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    async fn init(&self) -> Result<()> {
        if !self.commands.is_empty() {
            self.api("setMyCommands", &json!({ "commands": self.commands }))
                .await?;
        }
        Ok(())
    }

    async fn run(&self, tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        let mut offset: i64 = 1;
        loop {
            let url = format!(
                "{}{}/getUpdates?limit=10&offset={}&timeout=60",
                self.base_url, self.token, offset
            );
            match self
                .client
                .get(&url)
                .timeout(Duration::from_secs(65))
                .send()
                .await
            {
                Ok(resp) => match resp.json::<Value>().await {
                    Ok(json) => {
                        if let Some(results) = json["result"].as_array() {
                            for update in results {
                                if let Some(id) = update["update_id"].as_i64() {
                                    offset = id + 1;
                                }
                                // 回调需要立即回执，否则客户端一直转圈
                                if let Some(cq) = update.get("callback_query") && let Some(cid) = cq["id"].as_str() {
                                    self.answer_callback(cid).await
                                }
                                if let Some(msg) = parse_update(update) {
                                    let _ = tx.send(msg).await;
                                }
                            }
                        }
                    }
                    Err(e) => eprintln!("解析新消息失败: {e}"),
                },
                Err(e) => eprintln!("拉取新消息失败: {e}"),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn send(&self, msg: OutboundMessage) -> Result<MsgId> {
        let raw = msg.content.as_text();
        if raw.is_empty() {
            bail!("不能发送空消息");
        }
        let mut body = json!({
            "chat_id": msg.chat_id,
            "text": raw,
        });
        if msg.fold {
            body["text"] = json!(fold_markdown(raw));
            body["parse_mode"] = json!("MarkdownV2");
        }
        if !msg.buttons.is_empty() {
            body["reply_markup"] = json!({ "inline_keyboard": buttons_json(&msg.buttons) });
        }
        let resp = self.api("SendMessage", &body).await?;
        let id = resp["result"]["message_id"].as_u64().unwrap_or(0).to_string();
        if let Some(ttl) = msg.ttl {
            self.delete_after(&msg.chat_id, &id, ttl);
        }
        Ok(id)
    }

    async fn delete(&self, chat_id: &str, id: &str) -> Result<()> {
        self.delete_after(chat_id, id, 0);
        Ok(())
    }
    fn get_allow_id(&self) -> String {
        self.allow_user_id.clone()
    }
}

/// 把 Telegram update 转换成统一消息；无法识别时返回 `None`。
fn parse_update(update: &Value) -> Option<InboundMessage> {
    // 按钮回调
    if let Some(cq) = update.get("callback_query") {
        let user_id = cq["from"]["id"].as_i64()?;
        let chat_id = cq["message"]["chat"]["id"].as_i64()?;
        return Some(InboundMessage {
            user_id: user_id.to_string(),
            chat_id: chat_id.to_string(),
            content: Content::Text(String::new()),
            callback: Some(Callback {
                data: cq["data"].as_str().unwrap_or_default().to_string(),
                msg_id: cq["message"]["message_id"].as_u64().unwrap_or(0).to_string(),
            }),
        });
    }
    // 普通消息
    let message = update.get("message")?;
    let text = message["text"].as_str().unwrap_or_default();
    if text.is_empty() {
        return None;
    }
    let user_id = message["from"]["id"].as_i64()?;
    let chat_id = message["chat"]["id"].as_i64()?;
    Some(InboundMessage {
        user_id: user_id.to_string(),
        chat_id: chat_id.to_string(),
        content: Content::Text(text.to_string()),
        callback: None,
    })
}

/// 转义 MarkdownV2 特殊字符。
fn escape_markdown_v2(text: &str) -> String {
    const SPECIALS: &[char] = &[
        '_', '*', '[', ']', '(', ')', '~', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!', '\\',
        '$',
    ];
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if SPECIALS.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

static BLANK_LINES: Lazy<Regex> = Lazy::new(|| Regex::new(r"\n{2,}").unwrap());

/// 折叠渲染：压缩空行 → 转义 → 可展开引用 + 剧透。
fn fold_markdown(text: &str) -> String {
    let collapsed = BLANK_LINES.replace_all(text, "\n");
    let escaped = escape_markdown_v2(&collapsed).replace('\n', "\n>");
    format!("**>{}||", escaped)
}

/// 按钮转 Telegram inline_keyboard 结构。
fn buttons_json(buttons: &[Vec<Button>]) -> Value {
    Value::Array(
        buttons
            .iter()
            .map(|row| {
                Value::Array(
                    row.iter()
                        .map(|b| json!({ "text": b.text, "callback_data": b.data }))
                        .collect(),
                )
            })
            .collect(),
    )
}
