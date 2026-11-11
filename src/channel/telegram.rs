//! Telegram 平台实现。
//!
//! 负责把 Telegram update 转换成 [`InboundMessage`]，
//! 以及把 [`OutboundMessage`] 渲染成 Telegram 消息（含 MarkdownV2 折叠）。

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use super::{Button, Callback, Channel, InboundMessage, MsgId, MsgKind, OutboundMessage};
use crate::log;

/// Telegram 单条消息文本上限（UTF-16 码元）。
const MAX_TEXT_UNITS: usize = 4000;
/// 折叠模式会转义 MarkdownV2 特殊字符、行首加 `>`，内容膨胀，留更多余量。
const MAX_FOLD_UNITS: usize = 3500;
/// 模型用于显式切分超长回复的标记。
const CUT_TAG: &str = "<cut/>";

/// Telegram 按 UTF-16 码元计数（中文算 1，emoji 算 2）。
fn text_units(text: &str) -> usize {
    text.encode_utf16().count()
}

/// 按 `<cut/>` 与单条上限切分消息。
///
/// 优先使用模型给出的分段点；单段仍超限时按行、再按字符硬切。
fn split_message(text: &str, max: usize) -> Vec<String> {
    let mut parts = Vec::new();
    for part in text.split(CUT_TAG) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if text_units(part) <= max {
            parts.push(part.to_string());
            continue;
        }
        let mut buf = String::new();
        for line in part.split_inclusive('\n') {
            if !buf.is_empty() && text_units(&buf) + text_units(line) > max {
                parts.push(std::mem::take(&mut buf));
            }
            if text_units(line) > max {
                for ch in line.chars() {
                    if text_units(&buf) + ch.len_utf16() > max {
                        parts.push(std::mem::take(&mut buf));
                    }
                    buf.push(ch);
                }
            } else {
                buf.push_str(line);
            }
        }
        if !buf.is_empty() {
            parts.push(buf);
        }
    }
    parts
}

/// Telegram API 请求超时（上传大文件需要时间）。
const API_TIMEOUT_SECS: u64 = 180;

/// Bot 菜单里的指令（硬编码，避免与配置文件不同步）。
///
/// 顺序即 Telegram 客户端里展示的顺序；新增指令时这里和
/// `main::handle_msg` 的分发列表要一起改。
const COMMANDS: &[(&str, &str)] = &[
    ("new", "✅开启新对话"),
    ("clear", "🧹删除当前对话"),
    ("status", "📊查看当前状态"),
    ("compact", "🗜压缩上下文"),
    ("reasoning", "🧠选择推理模式"),
    ("context", "📏设置上下文窗口"),
    ("stop", "⏹️停止当前任务"),
    ("restart", "🔄重新启动网关"),
];

/// 把会话键拆成 `(chat_id, 话题 ID)`。
///
/// 普通聊天直接用 `chat_id`；论坛话题（Threaded Mode）用 `chat_id:thread_id`。
fn split_chat(key: &str) -> (&str, Option<i64>) {
    match key.rsplit_once(':') {
        Some((chat, thread)) => match thread.parse::<i64>() {
            Ok(t) => (chat, Some(t)),
            Err(_) => (key, None),
        },
        None => (key, None),
    }
}

/// 由 `chat_id` 与话题 ID 组成会话键。
fn join_chat(chat_id: i64, thread: Option<i64>) -> String {
    match thread {
        Some(t) => format!("{chat_id}:{t}"),
        None => chat_id.to_string(),
    }
}

/// Telegram 平台。
pub struct TelegramChannel {
    token: String,
    base_url: String,
    /// 白名单用户 ID（可配置多个）。
    allow_user_ids: Vec<String>,
    client: Client,
}

impl TelegramChannel {
    pub fn new() -> Result<Self> {
        let json: Value = qffunc::read_json("../config/bot.json", "telegram")?;
        let token = json["token"].as_str().unwrap_or_default();
        if token.is_empty() {
            bail!("未找到token")
        }
        let allow_user_ids = parse_allow_ids(&json["allow_user_id"]);
        log::info(
            "tg",
            format!(
                "Telegram 初始化：bot_id={}，白名单 {} 个用户（{}）",
                json["bot_id"].as_i64().unwrap_or(0),
                allow_user_ids.len(),
                allow_user_ids.join(",")
            ),
        );
        Ok(Self {
            token: token.to_string(),
            base_url: json["base_url"]
                .as_str()
                .unwrap_or("https://api.telegram.org/bot")
                .to_string(),
            allow_user_ids,
            // 统一超时：避免网络异常时请求永久挂起（上传大文件也需要时间）
            client: Client::builder()
                .timeout(Duration::from_secs(API_TIMEOUT_SECS))
                .build()
                .unwrap_or_else(|_| Client::new()),
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
    /// `key` 为会话键；话题模式下会把文件发到对应话题。
    ///
    /// 按扩展名自动选择接口：图片走 `sendPhoto`、动图走 `sendAnimation`、
    /// 视频走 `sendVideo`、音频走 `sendAudio`，其余走 `sendDocument`。
    /// 发送前校验体积：图片上限 10MB，其余 50MB。
    ///
    /// 会缓存文件的 `file_id`（见 [`FILE_ID_PATH`]）：内容哈希未变时直接复用
    /// `file_id` 发送，免去重复上传；新上传的 `file_id` 在后台异步落盘。
    pub async fn send_file(&self, key: &str, path: &str) -> Result<MsgId> {
        let (chat_id, thread) = split_chat(key);
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
            bail!("文件过大: {} 字节（{} 上限 {} 字节）", size, method, limit);
        }
        let data = tokio::fs::read(path).await?;
        let hash = hash_bytes(&data);

        // 命中缓存：直接拿 file_id 发送，省掉一次上传。
        if let Some(file_id) = cached_file_id(path, &hash).await {
            log::info(
                "tg",
                format!("{} ↑ 文件 {path} {size}B {method} 缓存", log::chat(key)),
            );
            let mut body = serde_json::Map::new();
            body.insert("chat_id".into(), json!(chat_id));
            body.insert(field.into(), json!(file_id));
            if let Some(t) = thread {
                body.insert("message_thread_id".into(), json!(t));
            }
            let value = self.api(method, &Value::Object(body)).await?;
            return Ok(msg_id(&value));
        }

        let file_name = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        log::info(
            "tg",
            format!("{} ↑ 文件 {path} {size}B {method} 上传", log::chat(key)),
        );
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(
                field,
                reqwest::multipart::Part::bytes(data).file_name(file_name),
            );
        if let Some(t) = thread {
            form = form.text("message_thread_id", t.to_string());
        }
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

    /// 发送单条文本（调用方保证长度不超限）。
    ///
    /// Markdown 解析失败时自动降级为纯文本重发。
    async fn send_text(
        &self,
        msg: &OutboundMessage,
        text: &str,
        with_buttons: bool,
    ) -> Result<MsgId> {
        let (chat_id, thread) = split_chat(&msg.chat_id);
        let mut body = json!({ "chat_id": chat_id, "text": text });
        if let Some(t) = thread {
            body["message_thread_id"] = json!(t);
        }
        if msg.fold {
            body["text"] = json!(fold_markdown(text));
            body["parse_mode"] = json!("MarkdownV2");
        } else if msg.kind == MsgKind::Text {
            body["parse_mode"] = json!("Markdown");
        }
        // 回复挂载：原消息可能已被删除（临时提示会过期），允许照常发送；
        // ID 解析失败时退化成普通消息，不因一条提示影响结果回传
        if let Some(reply_to) = &msg.reply_to
            && let Ok(message_id) = reply_to.parse::<i64>()
        {
            body["reply_parameters"] = json!({
                "message_id": message_id,
                "allow_sending_without_reply": true,
            });
        }
        if with_buttons && !msg.buttons.is_empty() {
            body["reply_markup"] = json!({ "inline_keyboard": buttons_json(&msg.buttons) });
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
        let id = resp["result"]["message_id"]
            .as_u64()
            .unwrap_or(0)
            .to_string();
        if let Some(ttl) = msg.ttl {
            self.delete_after(&msg.chat_id, &id, ttl);
        }
        Ok(id)
    }

    /// 延迟删除消息。`key` 为会话键，删除时只取 `chat_id`。
    fn delete_after(&self, key: &str, msg_id: &str, secs: u64) {
        let client = self.client.clone();
        let url = format!("{}{}/deleteMessage", self.base_url, self.token);
        let chat_id = split_chat(key).0.to_string();
        let msg_id = msg_id.to_string();
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
        let commands: Vec<Value> = COMMANDS
            .iter()
            .map(|(command, description)| json!({ "command": command, "description": description }))
            .collect();
        self.api("setMyCommands", &json!({ "commands": commands }))
            .await?;
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
                                if let Some(cq) = update.get("callback_query")
                                    && let Some(cid) = cq["id"].as_str()
                                {
                                    self.answer_callback(cid).await
                                }
                                if let Some(msg) = parse_update(update) {
                                    let _ = tx.send(msg).await;
                                }
                            }
                        }
                    }
                    Err(e) => log::warn("tg", format!("解析新消息失败：{e}")),
                },
                Err(e) => log::warn("tg", format!("拉取新消息失败（稍后重试）：{e}")),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn send(&self, msg: OutboundMessage) -> Result<MsgId> {
        if msg.content.is_empty() {
            bail!("不能发送空消息");
        }
        if msg.kind == MsgKind::File {
            return self.send_file(&msg.chat_id, &msg.content).await;
        }

        // 超长回复按 `<cut/>` 分段后逐条发送，按钮只挂在最后一条
        let max = if msg.fold {
            MAX_FOLD_UNITS
        } else {
            MAX_TEXT_UNITS
        };
        let mut parts = split_message(&msg.content, max);
        let last = parts.pop().ok_or_else(|| anyhow!("不能发送空消息"))?;
        let mut brief = format!(
            "{} ↑ {}字",
            log::chat(&msg.chat_id),
            msg.content.chars().count()
        );
        if !parts.is_empty() {
            brief.push_str(&format!(" {}段", parts.len() + 1));
        }
        if msg.fold {
            brief.push_str(" 折叠");
        }
        log::info("tg", brief);
        for part in &parts {
            self.send_text(&msg, part, false).await?;
        }
        self.send_text(&msg, &last, true).await
    }

    async fn delete(&self, chat_id: &str, id: &str) -> Result<()> {
        self.delete_after(chat_id, id, 0);
        Ok(())
    }

    fn is_allowed(&self, user_id: &str) -> bool {
        self.allow_user_ids.iter().any(|id| id == user_id)
    }

    /// 删除当前话题（连带话题内全部消息）。
    async fn delete_topic(&self, chat_id: &str) -> Result<()> {
        let (chat, thread) = split_chat(chat_id);
        let thread = thread.ok_or_else(|| anyhow!("当前对话不是话题"))?;
        self.api(
            "deleteForumTopic",
            &json!({ "chat_id": chat, "message_thread_id": thread }),
        )
        .await?;
        Ok(())
    }

    /// 创建论坛话题，返回 `chat_id:thread_id` 形式的新会话键。
    async fn create_topic(&self, chat_id: &str, name: &str) -> Result<super::ChatId> {
        let (chat, _) = split_chat(chat_id);
        let resp = self
            .api(
                "createForumTopic",
                &json!({ "chat_id": chat, "name": name }),
            )
            .await?;
        let thread_id = resp["result"]["message_thread_id"]
            .as_i64()
            .ok_or_else(|| anyhow!("createForumTopic 未返回 message_thread_id"))?;
        Ok(format!("{chat}:{thread_id}"))
    }
}

/// 图片可上传体积上限（字节）。
const PHOTO_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// 其他文件可上传体积上限（字节）。
const FILE_MAX_BYTES: u64 = 50 * 1024 * 1024;

/// file_id 缓存文件路径。
const FILE_ID_PATH: &str = "../config/telegram/file_id.json";

/// 串行化 file_id 缓存的读改写，避免并发发送时互相覆盖。
static FILE_ID_LOCK: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));

/// 解析白名单配置：支持 `[123, 456]` 数组，也兼容旧的单个数字/字符串写法。
///
/// 空值、`0` 与非法项会被忽略；解析结果为空时任何人都无法使用。
fn parse_allow_ids(value: &Value) -> Vec<String> {
    let items: Vec<&Value> = match value {
        Value::Array(list) => list.iter().collect(),
        Value::Null => Vec::new(),
        single => vec![single],
    };
    items
        .into_iter()
        .filter_map(|item| match item {
            Value::Number(n) => n.as_i64().map(|n| n.to_string()),
            Value::String(s) => Some(s.trim().to_string()),
            _ => None,
        })
        .filter(|id| !id.is_empty() && id != "0")
        .collect()
}

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
        log::warn("tg", format!("写入 file_id 缓存失败：{e}"));
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
        let thread = cq["message"]["message_thread_id"].as_i64();
        return Some(InboundMessage {
            user_id: user_id.to_string(),
            chat_id: join_chat(chat_id, thread),
            content: String::new(),
            callback: Some(Callback {
                data: cq["data"].as_str().unwrap_or_default().to_string(),
                msg_id: cq["message"]["message_id"]
                    .as_u64()
                    .unwrap_or(0)
                    .to_string(),
            }),
            topic_name: None,
        });
    }
    // 普通消息
    let message = update.get("message")?;
    // 忽略机器人发出的消息：自己创建话题等操作产生的服务消息会以 bot 身份下发，
    // 当成用户消息处理会触发白名单校验。
    if message["from"]["is_bot"].as_bool() == Some(true) {
        return None;
    }
    let user_id = message["from"]["id"].as_i64()?;
    let chat_id = message["chat"]["id"].as_i64()?;
    let thread = message["message_thread_id"].as_i64();
    let key = join_chat(chat_id, thread);

    // 话题创建 / 改名服务消息：只有标题，没有正文
    let created = message["forum_topic_created"]["name"]
        .as_str()
        .or_else(|| message["forum_topic_edited"]["name"].as_str());
    if let Some(name) = created {
        return Some(InboundMessage {
            user_id: user_id.to_string(),
            chat_id: key,
            content: String::new(),
            callback: None,
            topic_name: Some(name.to_string()),
        });
    }

    let text = message["text"].as_str().unwrap_or_default();
    if text.is_empty() {
        return None;
    }
    // 兜底：话题内消息有时会在 reply_to_message 里带上话题标题
    let topic_name = message["reply_to_message"]["forum_topic_created"]["name"]
        .as_str()
        .map(str::to_string);
    Some(InboundMessage {
        user_id: user_id.to_string(),
        chat_id: key,
        content: text.to_string(),
        callback: None,
        topic_name,
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
