//! 会话存储。
//!
//! 会话列表在 `messages/session.json`，每个会话的消息在 `messages/<id>.json`。

use std::fs;

use anyhow::{bail, Result};
use serde_json::{json, Value};

const SESSION_FILE: &str = "messages/session.json";

/// 当前会话 ID。
pub fn current_id() -> Result<String> {
    let session_id = qffunc::read_json(SESSION_FILE, "session_now")?;
    match session_id.as_str() {
        Some(resp) => Ok(resp.to_string()),
        None => bail!("未找到 session_now"),
    }
}

/// 读取当前会话的历史消息（不含 system）。
pub fn load_history() -> Result<Vec<Value>> {
    fs::create_dir_all("messages")?;
    let msg_file = format!("messages/{}.json", current_id()?);
    Ok(match qffunc::file_to_json(&msg_file) {
        Ok(resp) => resp.as_array().cloned().unwrap_or_default(),
        Err(_) => Vec::new(),
    })
}

/// 保存当前会话的消息（去掉首条 system）。
pub fn save(messages: &[Value]) -> Result<()> {
    let msg_file = format!("messages/{}.json", current_id()?);
    serde_json::to_writer_pretty(
        fs::File::create(format!("{}.tmp", msg_file))?,
        &messages[1..],
    )?;
    fs::rename(format!("{}.tmp", msg_file), &msg_file)?;
    Ok(())
}

/// 记录当前会话最近一次请求的 token 数。
pub fn record_tokens(total_tokens: u64) -> Result<()> {
    let session_id = current_id()?;
    let mut session_info = qffunc::file_to_json(SESSION_FILE)?;
    session_info[&session_id]["tokens"] = json!(total_tokens);
    serde_json::to_writer_pretty(fs::File::create(SESSION_FILE)?, &session_info)?;
    Ok(())
}
