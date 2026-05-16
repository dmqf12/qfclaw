use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chrono::Local;
use serde_json::{Value, json};
//use reqwest::Client;
//use aichat;
use sendmsg::*;

fn date_now() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{}-{}", Local::now().date_naive(), ts)
}

/*
async fn retry(text: &str, msg: &Value) {
    let messages: Vec<serde_json::Value> = fs::File::open("messages/messages.json")
        .ok()
        .and_then(|file| serde_json::from_reader(file).ok())
        .unwrap_or_else(|| vec![]);
    if messages.is_empty() {
        let _ = SendMessage::new("❓当前会话找不到此消息").send().await;
        return;
    }
    let reply_msg_id = msg["message"]["reply_to_message"]["message_id"]
        .as_u64()
        .unwrap_or_default();
    let reply_msg_text = msg["message"]["reply_to_message"]["text"]
        .as_str()
        .unwrap_or_default()
        .replace("\nMarkdown解析失败", "")
        .replacen("/retry", "", 1)
        .trim_start()
        .to_string();

    let msg_id = msg["message"]["message_id"].as_u64().unwrap_or_default();

    if reply_msg_text.is_empty() {
        clear_up(msg_id - 1, msg_id, 0);
        let _ = fs::create_dir_all("messages");
        for (i, message) in messages.iter().enumerate().rev() {
            if let Some(role) = message["role"].as_str() {
                if role == "user" {
                    if let Ok(file) = fs::File::create("messages/messages.json") {
                        let _ = serde_json::to_writer_pretty(file, &messages[..i + 1]);
                        let user_text = messages[i]["content"].as_str().unwrap_or_default();
                        let _ = aichat::main(&user_text).await;
                    }
                }
            }
        }
        return;
    }
    for (i, message) in messages.iter().enumerate() {
        let msg_text = message["content"].as_str().unwrap_or_default();
        let role = message["role"].as_str().unwrap_or_default();
        if msg_text == reply_msg_text {
            let _ = fs::create_dir_all("messages");
            if let Ok(file) = fs::File::create("messages/messages.json") {
                if role == "user" {
                    let _ = serde_json::to_writer_pretty(file, &messages[..i]);
                    if text.len() > 6 {
                        clear_up(reply_msg_id, msg_id - 1, 0);
                        let _ = aichat::main(&text.replacen("/retry", "", 1).trim_start()).await;
                    } else {
                        clear_up(reply_msg_id + 1, msg_id, 0);
                        let _ = aichat::main(&msg_text).await;
                    }
                    return;
                }
            }
        }
    }
    let _ = SendMessage::new("❓当前会话找不到此消息").send().await;
    return;
}
*/
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
        let (new_msg_id, _) = SendMessage::new("✅ New session started").send().await?;
        fs::create_dir_all("messages")?;
        let session = json!({"last_session_start": new_msg_id, "total_tokens": 0});
        serde_json::to_writer_pretty(fs::File::create(session_file)?, &session)?;
    }
    if cmd_text == "/new" {
        Box::pin(exec_cmd("/mv_session archive", msg)).await?;
    }
    if cmd_text == "/restart" {
        let (msg_id, _) = SendMessage::new("🔄重启中").send().await?;
        clear_up(msg_id - 1, msg_id, 0);
        tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
        match Command::new("bash").arg("-c").arg("systemctl --user restart qfclaw").output() {
            Ok(_) => return Ok(true),
            Err(_) => return Ok(false),
        };
    }
    if cmd_text.contains("/retry") {
        println!("/retry");
        //  retry(cmd_text, msg).await
    }
    if cmd_text == "/status" {
        let session: Value = fs::File::open(session_file)
            .ok()
            .and_then(|file| serde_json::from_reader(file).ok())
            .unwrap_or_default();
        let n = session["total_tokens"].as_u64().unwrap_or(0);
        let (msg_id, _) = SendMessage::new(&format!(
            "📚 Context: {:.1}K/1M ({:.1}%)",
            n as f64 / 1000.0,
            n as f64 / 10000.
        ))
        .send()
        .await?;
        clear_up(msg_id - 1, msg_id, 5);
    }
    if cmd_text == "/reasoning" {
         let msg_id = send_inline("是否显示推理过程", json!([
            [{"text": "隐藏", "callback_data": "reasoning_set_draft"}, {"text": "关闭", "callback_data": "reasoning_disabled"}],
            [{"text": "折叠", "callback_data": "reasoning_set_fold"}]])).await?;
         clear_up(msg_id - 1, msg_id - 1, 0);
    }

    if cmd_text == "/clear" {
        let (message_id, _) = SendMessage::new("🧹Clear up immediately").send().await?;
        let session: Value = fs::File::open(session_file)
            .ok()
            .and_then(|file| serde_json::from_reader(file).ok())
            .unwrap_or_default();
        let last_session_id = session["last_session_start"].as_u64().unwrap_or(9999999);
        if last_session_id != 9999999 {
            Box::pin(exec_cmd("/mv_session clear", msg)).await?;
            clear_up(last_session_id, message_id, 0);
        } else {
            SendMessage::new("🧹当前无session可清理").send().await?;
        }
    }

    Ok(true)
}
