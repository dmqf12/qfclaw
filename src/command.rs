use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chrono::Local;
use serde_json::{Value, json};
//use reqwest::Client;
//use aichat;
use crate::sendmsg::*;

fn date_now() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{}-{}", Local::now().date_naive(), ts)
}

pub async fn exec_cmd(cmd_text: &str, msg: &Value) -> Result<bool> {
    let session_file = "messages/session.json";

    if cmd_text.contains("/mv_session") {
        let target_path = cmd_text
            .splitn(2, ' ')
            .last()
            .unwrap_or(cmd_text)
            .to_string();
        fs::create_dir_all(format!("messages/{target_path}"))?;
        let target_file = format!("messages/{}/{}.json", target_path, date_now());
        let _ = fs::rename("messages/messages.json", target_file);
        let new_msg_id = SendMessage::new("✅ New session started").send().await;
        fs::create_dir_all("messages")?;
        let session = json!({"last_session_start": new_msg_id, "total_tokens": 0});
        serde_json::to_writer_pretty(fs::File::create(session_file)?, &session)?;
    }
    if cmd_text == "/new" {
        Box::pin(exec_cmd("/mv_session archive", msg)).await?;
    }
    if cmd_text == "/restart" {
        let msg_id = SendMessage::new("🔄重启中").send().await;
        clear_up(((msg_id[0] - 1)..=msg_id[0]).collect(), 0);
        tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
        match Command::new("bash").arg("-c").arg("systemctl --user restart qfclaw").output() {
            Ok(_) => return Ok(true),
            Err(_) => return Ok(false),
        };
    }
    if cmd_text == "/status" {
        let session: Value = fs::File::open(session_file)
            .ok()
            .and_then(|file| serde_json::from_reader(file).ok())
            .unwrap_or_default();
        let n = session["total_tokens"].as_u64().unwrap_or(0);
        let msg_id = SendMessage::new(&format!(
            "📚 Context: {:.1}K/1M ({:.1}%)",
            n as f64 / 1000.0,
            n as f64 / 10000.
        ))
        .send()
        .await;
        clear_up(((msg_id[0] - 1)..=msg_id[0]).collect(), 5);
    }
    if cmd_text == "/reasoning" {
         let msg_id = send_inline("是否显示推理过程", json!([
            [{"text": "隐藏", "callback_data": "reasoning_set_draft"}, {"text": "关闭", "callback_data": "reasoning_disabled"}],
            [{"text": "折叠", "callback_data": "reasoning_set_fold"}]])).await?;
         clear_up(vec![ msg_id - 1 ], 0);
    }

    if cmd_text == "/clear" {
        let message_id = SendMessage::new("🧹Clear up immediately").send().await;
        let session: Value = fs::File::open(session_file)
            .ok()
            .and_then(|file| serde_json::from_reader(file).ok())
            .unwrap_or_default();
        if let Some(last_session_id) = session["last_session_start"].as_u64() {
            _ = Box::pin(exec_cmd("/mv_session clear", msg)).await;
            clear_up((last_session_id..=message_id[0]).collect(), 0);
        } else {
            SendMessage::new("🧹当前无session可清理").send().await;
        }
    }

    Ok(true)
}
