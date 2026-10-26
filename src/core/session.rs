//! 会话存储。
//!
//! 一个对话对应一个文件：私聊/群聊是 `messages/<chat_id>.json`，
//! Telegram 论坛话题（Threaded Mode）是 `messages/<chat_id>:<thread_id>.json`。
//! 对话之间互不影响，文件结构：
//!
//! ```json
//! {
//!   "create_time": 1730000000,
//!   "title": "话题标题",
//!   "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 },
//!   "messages": []
//! }
//! ```

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{json, Value};

const MSG_DIR: &str = "../messages";

/// 文件名安全化（Telegram 的 chat / thread 都是数字，这里只兜底）。
fn file_key(chat: &str) -> String {
    chat.replace(['/', '\\'], "_")
}

fn path(chat: &str) -> String {
    format!("{MSG_DIR}/{}.json", file_key(chat))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 空对话结构。
fn empty() -> Value {
    json!({ "create_time": now(), "title": "", "usage": {}, "messages": [] })
}

/// 读取对话文件；缺失或损坏时返回空结构（不落盘）。
fn load(chat: &str) -> Value {
    let mut doc = match qffunc::file_to_json(&path(chat)) {
        Ok(value) if value.is_object() => value,
        _ => empty(),
    };
    if !doc["messages"].is_array() {
        doc["messages"] = json!([]);
    }
    if !doc["usage"].is_object() {
        doc["usage"] = json!({});
    }
    if doc["create_time"].as_u64().is_none() {
        doc["create_time"] = json!(now());
    }
    if !doc["title"].is_string() {
        doc["title"] = json!("");
    }
    doc
}

/// 原子写入对话文件。
fn store(chat: &str, doc: &Value) -> Result<()> {
    fs::create_dir_all(MSG_DIR)?;
    let tmp = format!("{MSG_DIR}/.{}.tmp", file_key(chat));
    serde_json::to_writer_pretty(fs::File::create(&tmp)?, doc)?;
    fs::rename(&tmp, path(chat))?;
    Ok(())
}

/// 读取对话历史（不含 system）。
pub fn load_history(chat: &str) -> Result<Vec<Value>> {
    Ok(load(chat)["messages"].as_array().cloned().unwrap_or_default())
}

/// 保存对话历史（去掉首条 system）。
pub fn save(chat: &str, messages: &[Value]) -> Result<()> {
    let mut doc = load(chat);
    doc["messages"] = json!(messages.get(1..).unwrap_or_default());
    store(chat, &doc)
}

/// 记录最近一次请求的 usage。
pub fn record_usage(chat: &str, usage: &Value) -> Result<()> {
    if !usage.is_object() {
        return Ok(());
    }
    let mut doc = load(chat);
    doc["usage"] = usage.clone();
    store(chat, &doc)
}

/// 当前对话的 usage。
pub fn usage(chat: &str) -> Value {
    load(chat)["usage"].clone()
}

/// 重置对话：清空历史与 usage，重置创建时间；话题标题保留。
pub fn reset(chat: &str) -> Result<()> {
    let mut doc = empty();
    doc["title"] = json!(title(chat));
    store(chat, &doc)
}

/// 记录话题标题（来自 Telegram 的服务消息或建话题的返回值）。
pub fn set_title(chat: &str, name: &str) -> Result<()> {
    let mut doc = load(chat);
    if doc["title"].as_str() == Some(name) {
        return Ok(());
    }
    doc["title"] = json!(name);
    store(chat, &doc)
}

/// 话题标题；没有时返回空串。
pub fn title(chat: &str) -> String {
    load(chat)["title"].as_str().unwrap_or_default().to_string()
}
