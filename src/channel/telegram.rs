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
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use super::{MsgKind, Button, Callback, Channel, InboundMessage, MsgId, OutboundMessage};


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

    /// 上传本地文件并发送，返回消息 ID。
    ///
    /// 按扩展名自动选择接口：图片走 `sendPhoto`、动图走 `sendAnimation`、
    /// 视频走 `sendVideo`、音频走 `sendAudio`，其余走 `sendDocument`。
    /// 发送前校验体积：图片上限 10MB，其余 50MB。
    ///
    /// 会缓存文件的 `file_id`（见 [`FILE_ID_PATH`]）：内容哈希未变时直接复用
    /// `file_id` 发送，免去重复上传；新上传的 `file_id` 在后台异步落盘。
    pub async fn send_file(&self, chat_id: &str, path: &str) -> Result<MsgId> {
        let (method, field) = media_method(path);
        let size = tokio::fs::metadata(path).await?.len();
        if size == 0 {
            bail!("文件为空: {}", path);
        }
        let limit = if method == "sendPhoto" {
            PHOTO_MAX_BYTES
        } else {
            FILE_MAX_BYTES
        };
        if size > limit {
            bail!(
                "文件过大: {} 字节（{} 上限 {} 字节）",
                size,
                method,
                limit
            );
        }
        let data = tokio::fs::read(path).await?;
        let hash = hash_bytes(&data);

        // 命中缓存：直接拿 file_id 发送，省掉一次上传。
        if let Some(file_id) = cached_file_id(path, &hash).await {
            let mut body = serde_json::Map::new();
            body.insert("chat_id".into(), json!(chat_id));
            body.insert(field.into(), json!(file_id));
            let value = self.api(method, &Value::Object(body)).await?;
            return Ok(msg_id(&value));
        }

        let file_name = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        let form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(
                field,
                reqwest::multipart::Part::bytes(data).file_name(file_name),
            );
        let url = format!("{}{}/{}", self.base_url, self.token, method);
        let value: Value = self
            .client
            .post(url)
            .multipart(form)
            .send()
            .await?
            .json()
            .await?;
        if value["ok"].as_bool() != Some(true) {
            bail!("Telegram {} 失败: {}", method, value)
        }
        // 新 file_id 后台落盘，不阻塞发送。
        if let Some(file_id) = extract_file_id(&value, field) {
            let (path, hash) = (path.to_string(), hash);
            tokio::spawn(async move { save_file_id(path, hash, file_id).await });
        }
        Ok(msg_id(&value))
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
        let mut body = json!({
            "chat_id": msg.chat_id,
            "text": msg.content,
        });
        if msg.content.is_empty() {
            bail!("不能发送空消息");
        }
        if msg.fold {
            body["text"] = json!(fold_markdown(&msg.content));
            body["parse_mode"] = json!("MarkdownV2");
        } else if msg.kind != MsgKind::Error {
            body["parse_mode"] = json!("Markdown");
        }
        if !msg.buttons.is_empty() {
            body["reply_markup"] = json!({ "inline_keyboard": buttons_json(&msg.buttons) });
        }
        if msg.kind == MsgKind::File {
            return Ok(self.send_file(&msg.chat_id, &msg.content).await?)
        }
        let resp = match self.api("SendMessage", &body).await {
            Ok(r) => r,
            Err(e) => {
                if e.to_string().contains("parse") {
                    body["parse_mode"] = json!("");
                    self.api("SendMessage", &body).await?
                } else {
                    bail!(e)
                }
            }
        };
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

/// 图片可上传体积上限（字节）。
const PHOTO_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// 其他文件可上传体积上限（字节）。
const FILE_MAX_BYTES: u64 = 50 * 1024 * 1024;

/// file_id 缓存文件路径。
const FILE_ID_PATH: &str = "config/telegram/file_id.json";

/// 串行化 file_id 缓存的读改写，避免并发发送时互相覆盖。
static FILE_ID_LOCK: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));

/// 取响应里的 message_id。
fn msg_id(value: &Value) -> MsgId {
    value["result"]["message_id"]
        .as_u64()
        .unwrap_or(0)
        .to_string()
}

/// 从发送结果里取 file_id（`photo` 是尺寸数组，取最大的一张）。
fn extract_file_id(value: &Value, field: &str) -> Option<String> {
    let node = value["result"].get(field)?;
    let target = node.as_array().and_then(|a| a.last()).unwrap_or(node);
    target["file_id"].as_str().map(str::to_string)
}

/// SHA-256 哈希，转成十六进制字符串。
fn hash_bytes(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 读取 file_id 缓存；文件缺失或内容损坏时返回空对象。
async fn read_file_id_cache() -> Value {
    tokio::fs::read_to_string(FILE_ID_PATH)
        .await
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

/// 缓存里存在同一路径且哈希一致时，返回其 file_id。
async fn cached_file_id(path: &str, hash: &str) -> Option<String> {
    let cache = read_file_id_cache().await;
    let entry = cache.get(path)?;
    if entry["sha256"].as_str() == Some(hash) {
        entry["file_id"].as_str().map(str::to_string)
    } else {
        None
    }
}

/// 把 file_id 写入缓存（加锁读改写）。
async fn save_file_id(path: String, hash: String, file_id: String) {
    let _guard = FILE_ID_LOCK.lock().await;
    let mut cache = read_file_id_cache().await;
    if let Some(map) = cache.as_object_mut() {
        map.insert(path, json!({ "sha256": hash, "file_id": file_id }));
    }
    if let Some(dir) = std::path::Path::new(FILE_ID_PATH).parent() {
        let _ = tokio::fs::create_dir_all(dir).await;
    }
    if let Err(e) = tokio::fs::write(FILE_ID_PATH, cache.to_string()).await {
        eprintln!("写入 file_id 缓存失败: {e}");
    }
}

/// 按扩展名判断媒体类型，返回（API 方法名，表单字段名）。
fn media_method(path: &str) -> (&'static str, &'static str) {
    let ext = std::path::Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "bmp" | "webp" => ("sendPhoto", "photo"),
        "gif" => ("sendAnimation", "animation"),
        "mp4" | "mov" | "mkv" | "webm" | "avi" => ("sendVideo", "video"),
        "mp3" | "m4a" | "ogg" | "opus" | "wav" | "flac" | "aac" => ("sendAudio", "audio"),
        _ => ("sendDocument", "document"),
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
            content: String::new(),
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
        content: text.to_string(),
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
