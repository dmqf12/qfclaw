use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{ bail, Result };
use serde_json::{ json };

use crate::telegram::*;
use crate::aichat::get_session_id;

const MESSAGE_PATH: &str = "messages/";


fn date_now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间早于 UNIX_EPOCH")
        .as_secs()
        .to_string()
}

pub async fn exec_cmd(chat_id: i64, cmd_text: &str) -> Result<bool> {
    let session_file = "messages/session.json";
    let args: Vec<&str> = cmd_text.split_whitespace().collect();
    if cmd_text == "/new" {
        fs::create_dir_all("messages/archive")?;
        let args: Vec<&str> = cmd_text.split_whitespace().collect();
        let mut session_info = qffunc::file_to_json(session_file).unwrap_or(json!({}));
        let session_title: &str;
        if args.len() >= 2 {
            session_title = args[1];
        } else {
            session_title = "无标题";
        }
        let date = date_now();
        session_info["session_now"] = json!(date);
        session_info[&date] = json!({ "title": session_title});
        qffunc::json_to_file(session_file, &session_info)?;
        MsgBuilder::new(&format!("✅ New session {date} started")).send().await?;
        return Ok(true);
    }
    if cmd_text == "/restart" {
        if let Some(msg_id) = MsgBuilder::new("🔄重启中").send().await?.msg_id {
            delete_msg(chat_id, vec![msg_id - 1, msg_id], 3);
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
        match Command::new("bash").arg("-c").arg("systemctl --user restart qfclaw").output() {
            Ok(_) => return Ok(true),
            Err(_) => return Ok(false),
        };
    }
    if cmd_text == "/status" {
        let session_info = qffunc::file_to_json(session_file)?;
        let session_id = get_session_id()?;
        let n = session_info[session_id]["tokens"].as_u64().unwrap_or(0);
        if let Some(msg_id) = MsgBuilder::new(&format!(
            "📚 Context: {:.1}K/1M ({:.1}%)",
            n as f64 / 1000.0,
            n as f64 / 10000.
        )).send().await?.msg_id {
            delete_msg(chat_id, vec![msg_id - 1, msg_id], 3);
        }
    }
    if cmd_text == "/reasoning" {
         let msg_id = send_inline(chat_id, "是否显示推理过程", json!([
            [{"text": "隐藏", "callback_data": "reasoning_set_draft"}, {"text": "关闭", "callback_data": "reasoning_disabled"}],
            [{"text": "折叠", "callback_data": "reasoning_set_fold"}]])).await?;
         delete_msg(chat_id, vec![ msg_id - 1 ], 0);
    }
    if cmd_text.contains("/session") {
        match args.len() {
            1 => {_ = MsgBuilder::new(&fs::read_to_string(session_file)?).send().await?;println!("1234")}
            _ => {
                let session_id = args[2];
                let session_op = args.get(1).copied().unwrap_or("switch");
                let mut session_info = qffunc::file_to_json(session_file)?;
                let session_now = match session_info["session_now"].as_str() {
                    Some(resp) => resp,
                    None => &date_now()
                };
                if session_info.get(session_id).is_none() && session_now != session_id {
                    bail!(format!("未找到会话：{}", session_id))
                }

                match session_op {
                    "switch" => {
                        if session_now != session_id {
                            fs::rename(format!("{}{}.json", MESSAGE_PATH, session_id), format!("{}archive/{}.json", MESSAGE_PATH, session_now))?;
                            fs::rename(format!("{}archive/{}.json", MESSAGE_PATH, session_id), format!("{}{}.json", MESSAGE_PATH, session_id))?;
                            session_info["session_now"] = json!(session_id);
                            MsgBuilder::new(&format!("成功切换至{}", session_id)).send().await?;
                        } else {
                            bail!(format!("{}已是当前会话", session_id))
                        }
                    }
                    "clear" | "archive" => {
                        if session_now != session_id {
                            _ = fs::rename(format!("{}archive/{}.json", MESSAGE_PATH, session_id), format!("{}{}/{}.json", MESSAGE_PATH, session_op, session_id));
                        } else {
                            _ = fs::rename(format!("{}{}.json", MESSAGE_PATH, session_id), format!("{}{}/{}.json", MESSAGE_PATH, session_op, session_id));
                        }
                        if let Some(obj) = session_info.as_object_mut() {
                                obj.remove(session_id);
                        }
                        let session_now = date_now();
                        session_info["session_now"] = json!(session_now);
                        MsgBuilder::new(&format!("{}: {}成功", session_id, session_op)).send().await?;
                    }
                    "name" => {
                        let session_title = args.get(3).copied().unwrap_or("");
                        session_info[session_id]["title"] = json!(session_title);
                        MsgBuilder::new(&format!("{}标题成功修改为:{}", session_id, session_title)).send().await?;
                    }
                    _ => bail!("错误的参数"),
                }
                qffunc::json_to_file(session_file, &session_info)?;
            }
        }
        return Ok(true);
    }
    if cmd_text.contains("/name") {
        if args.len() == 3 {
            Box::pin(exec_cmd(chat_id, &format!("/session name {} {}", args[1], args[2]))).await?;
        } else {
            bail!("错误的参数")
        }
    }
    if cmd_text.contains("/clear") {
        if args.len() == 2 {
            Box::pin(exec_cmd(chat_id, &format!("/session clear {}", args[1]))).await?;
        } else {
            bail!("错误的参数")
        }
    }
    Ok(true)
}
