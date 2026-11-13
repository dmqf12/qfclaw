//! 会话存储。
//!
//! 对话内容与元信息分开存放，一个对话对应两个文件：
//!
//! - 对话：`messages/<chat>.json`，结构 `{ "memory": "历史记忆", "messages": [...] }`
//! - 元信息：`messages/meta/<chat>.info.json`，结构
//!   `{ "create_time": 1730000000, "title": "话题标题", "usage": {} }`
//!
//! 压缩前的完整上下文归档到 `messages/archive/<chat>/<时间戳>.json`。
//!
//! `<chat>` 为会话键（`chat_id` 或 `chat_id:thread_id`）。
//! 压缩记忆与消息一起存放在对话文件里（两者总是一起写）；
//! 标题 / usage 这类只关心元数据的操作只碰很小的 meta 文件，
//! `/status` 也不会解析整份消息历史。
//!
//! 旧版本把 state 与消息放在同一个文件里、memory 放在 meta 里，
//! 读取时都会自动迁移到当前结构。

use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use once_cell::sync::Lazy;
use serde_json::{Value, json};

use crate::log;

const MSG_DIR: &str = "../messages";
const META_DIR: &str = "../messages/meta";

/// 串行化状态文件的读改写，避免并发写入互相覆盖。
static META_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

/// 文件名安全化（Telegram 的 chat / thread 都是数字，这里只兜底）。
fn file_key(chat: &str) -> String {
    chat.replace(['/', '\\'], "_")
}

fn messages_path(chat: &str) -> String {
    format!("{MSG_DIR}/{}.json", file_key(chat))
}

fn info_path(chat: &str) -> String {
    format!("{META_DIR}/{}.info.json", file_key(chat))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 空状态结构。
fn empty_info() -> Value {
    json!({ "create_time": now(), "title": "", "usage": {} })
}

/// 原子写入 JSON（临时文件 + rename）。
fn write_json(path: &str, doc: &Value) -> Result<()> {
    if let Some(dir) = Path::new(path).parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = format!("{path}.tmp");
    serde_json::to_writer_pretty(fs::File::create(&tmp)?, doc)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// 读取 JSON 对象；缺失时返回 `None`，损坏时先备份再返回 `None`。
fn read_json(path: &str) -> Option<Value> {
    match qffunc::file_to_json(path) {
        Ok(value) if value.is_object() => Some(value),
        _ => {
            if fs::metadata(path).is_ok() {
                let backup = format!("{path}.corrupt");
                if let Err(e) = fs::rename(path, &backup) {
                    log::warn("session", format!("备份损坏的文件失败 {path}：{e}"));
                }
            }
            None
        }
    }
}

/// 补全状态字段的默认值。
fn normalize_info(mut info: Value) -> Value {
    if info["create_time"].as_u64().is_none() {
        info["create_time"] = json!(now());
    }
    if !info["title"].is_string() {
        info["title"] = json!("");
    }
    if !info["usage"].is_object() {
        info["usage"] = json!({});
    }
    info
}

/// 读取状态（不加锁）。
fn read_info(chat: &str) -> Value {
    normalize_info(read_json(&info_path(chat)).unwrap_or_else(empty_info))
}

/// 读取状态，必要时先迁移旧格式（对外入口用）。
fn info(chat: &str) -> Value {
    migrate_if_needed(chat);
    read_info(chat)
}

/// 旧格式迁移：状态字段原本与消息存在同一文件里。
///
/// 只在状态文件缺失时执行一次；调用方需持有 `META_LOCK`。
fn migrate_unlocked(chat: &str) {
    if Path::new(&info_path(chat)).exists() {
        return;
    }
    let Some(doc) = read_json(&messages_path(chat)) else {
        return;
    };
    // memory 属于对话内容，迁移后仍留在对话文件里
    let has_state = ["create_time", "title", "usage"]
        .iter()
        .any(|key| doc.get(key).is_some());
    if !has_state {
        return;
    }
    let info = normalize_info(json!({
        "create_time": doc.get("create_time").cloned().unwrap_or(Value::Null),
        "title": doc.get("title").cloned().unwrap_or(Value::Null),
        "usage": doc.get("usage").cloned().unwrap_or(Value::Null),
    }));
    match write_json(&info_path(chat), &info) {
        Ok(_) => log::info("session", format!("会话状态已迁移到 meta: {chat}")),
        Err(e) => log::error("session", format!("迁移会话状态失败 {chat}：{e}")),
    }
}

fn migrate_if_needed(chat: &str) {
    let _guard = META_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    migrate_unlocked(chat);
}

/// 读取状态 → 修改 → 原子写回（全程持锁）。
fn update_info<F: FnOnce(&mut Value)>(chat: &str, f: F) -> Result<()> {
    let _guard = META_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    migrate_unlocked(chat);
    let mut info = read_info(chat);
    f(&mut info);
    write_json(&info_path(chat), &info)
}

/// 读取对话文件：`{ "memory": "", "messages": [] }`。
fn read_chat_doc(chat: &str) -> Value {
    let mut doc = read_json(&messages_path(chat)).unwrap_or_else(|| json!({}));
    if !doc["messages"].is_array() {
        doc["messages"] = json!([]);
    }
    if !doc["memory"].is_string() {
        doc["memory"] = json!("");
    }
    doc
}

/// 读取对话历史（不含 system）。
pub fn load_history(chat: &str) -> Result<Vec<Value>> {
    Ok(read_chat_doc(chat)["messages"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

/// 保存对话历史（去掉首条 system），保留记忆字段。
pub fn save(chat: &str, messages: &[Value]) -> Result<()> {
    // 先确保旧格式的元信息已迁移，避免随后的写入把标题 / usage 丢掉
    migrate_if_needed(chat);
    let mut doc = read_chat_doc(chat);
    doc["messages"] = json!(messages.get(1..).unwrap_or_default());
    // 元信息已迁到 meta 后，对话文件只保留记忆与消息
    let doc = if Path::new(&info_path(chat)).exists() {
        json!({ "memory": doc["memory"].clone(), "messages": doc["messages"].clone() })
    } else {
        doc
    };
    write_json(&messages_path(chat), &doc)
}

/// 记录最近一次请求的 usage。
pub fn record_usage(chat: &str, usage: &Value) -> Result<()> {
    if !usage.is_object() {
        return Ok(());
    }
    update_info(chat, |info| info["usage"] = usage.clone())
}

/// 读取长期记忆（存在对话文件里）。
///
/// 旧版本的记忆写在 meta 里，读到就顺手迁回对话文件。
pub fn memory(chat: &str) -> String {
    let doc = read_chat_doc(chat);
    let memory = doc["memory"].as_str().unwrap_or_default().to_string();
    if !memory.trim().is_empty() {
        return memory;
    }
    // 兼容旧数据：meta 里的记忆迁回对话文件
    let legacy = read_info(chat)["memory"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if legacy.trim().is_empty() {
        return String::new();
    }
    let mut doc = read_chat_doc(chat);
    doc["memory"] = json!(legacy);
    if let Err(e) = write_json(&messages_path(chat), &doc) {
        log::warn("session", format!("迁移记忆到对话文件失败 {chat}：{e}"));
    }
    legacy
}

/// 压缩结果写回：更新记忆并替换消息列表（`messages` 不含 system）。
///
/// 同时清空 `usage`：压缩后上下文已经重建，旧的 token 数不再代表当前大小，
/// 留着会让下一轮再次判定「需要压缩」，从而反复压缩。
pub fn apply_compact(chat: &str, memory: &str, messages: &[Value]) -> Result<()> {
    let mut doc = read_chat_doc(chat);
    // 压缩会替换消息列表，先把完整上下文归档，方便回溯
    if let Err(e) = archive(chat, &doc, memory) {
        log::warn("session", format!("归档压缩前上下文失败 {chat}：{e}"));
    }
    doc["memory"] = json!(memory);
    doc["messages"] = json!(messages);
    write_json(&messages_path(chat), &doc)?;
    update_info(chat, |info| info["usage"] = Value::Null)
}

/// 压缩前归档完整上下文：`messages/archive/<chat>/<时间戳>.json`。
///
/// 归档内容含压缩前的历史记忆、全部消息、当次摘要与 usage，
/// 压缩本身仍然照旧把消息列表替换成「最后一条用户消息」。
fn archive(chat: &str, doc: &Value, summary: &str) -> Result<String> {
    let dir = format!("{MSG_DIR}/archive/{}", file_key(chat));
    fs::create_dir_all(&dir)?;
    let at = now();
    let path = format!("{dir}/{at}.json");
    let meta = info(chat);
    let snapshot = json!({
        "archived_at": at,
        "title": meta["title"].clone(),
        "usage": meta["usage"].clone(),
        "memory": doc["memory"].clone(),
        "summary": summary,
        "messages": doc["messages"].clone(),
    });
    write_json(&path, &snapshot)?;
    Ok(path)
}

/// 对话概览：`(标题, 最近一次请求的总 token)`。
pub fn status(chat: &str) -> (String, u64) {
    let meta = info(chat);
    (
        meta["title"].as_str().unwrap_or_default().to_string(),
        meta["usage"]["total_tokens"].as_u64().unwrap_or(0),
    )
}

/// 重置对话：清空历史与 usage，重置创建时间；话题标题保留。
pub fn reset(chat: &str) -> Result<()> {
    let title = info(chat)["title"].clone();
    write_json(
        &messages_path(chat),
        &json!({ "memory": "", "messages": [] }),
    )?;
    update_info(chat, move |info| {
        *info = empty_info();
        info["title"] = title;
    })
}

/// 删除当前对话：消息与状态文件移到 `messages/clear/` 下保留。
pub fn clear(chat: &str) -> Result<()> {
    let dir = format!("{MSG_DIR}/clear");
    fs::create_dir_all(&dir)?;
    let key = file_key(chat);
    for file in [messages_path(chat), info_path(chat)] {
        if !Path::new(&file).exists() {
            continue;
        }
        let name = Path::new(&file)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("{key}.json"));
        let mut dest = format!("{dir}/{name}");
        // 已有同名归档时加时间戳，避免覆盖
        if Path::new(&dest).exists() {
            dest = format!("{dir}/{}.{name}", now());
        }
        fs::rename(&file, dest)?;
    }
    Ok(())
}

/// 记录话题标题（来自 Telegram 的服务消息或建话题的返回值）。
pub fn set_title(chat: &str, name: &str) -> Result<()> {
    if info(chat)["title"].as_str() == Some(name) {
        return Ok(());
    }
    update_info(chat, |info| info["title"] = json!(name))
}
