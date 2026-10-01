//! 斜杠指令与按钮回调处理。

use std::fs;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use serde_json::json;

use crate::channel::*;
use crate::core::session::current_id;

const SESSION_FILE: &str = "messages/session.json";
const MESSAGE_PATH: &str = "messages/";

fn date_now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间早于 UNIX_EPOCH")
        .as_secs()
        .to_string()
}

/// 发送一条文本消息。
async fn reply(channel: &Arc<dyn Channel>, chat_id: &str, text: impl Into<Content>) -> Result<MsgId> {
    channel
        .send(OutboundMessage::text(chat_id.to_string(), text))
        .await
}

/// 延迟删除若干消息。
fn delete_after(channel: Arc<dyn Channel>, chat_id: String, ids: Vec<MsgId>, delay: u64) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(delay)).await;
        for id in ids {
            let _ = channel.delete(&chat_id, &id).await;
        }
    });
}

/// 消息 ID 往前偏移（Telegram 消息 ID 递增，用于删除上一条）。
fn prev_id(id: &MsgId, n: u64) -> Option<MsgId> {
    id.parse::<u64>()
        .ok()
        .map(|v| v.saturating_sub(n).to_string())
}

pub async fn exec_cmd(channel: Arc<dyn Channel>, chat_id: &str, cmd_text: &str) -> Result<bool> {
    let args: Vec<&str> = cmd_text.split_whitespace().collect();

    if cmd_text == "/new" {
        fs::create_dir_all("messages/archive")?;
        let mut session_info = qffunc::file_to_json(SESSION_FILE).unwrap_or(json!({}));
        let session_title = args.get(1).copied().unwrap_or("无标题");
        let date = date_now();
        session_info["session_now"] = json!(date);
        session_info[&date] = json!({ "title": session_title });
        qffunc::json_to_file(SESSION_FILE, &session_info)?;
        reply(&channel, chat_id, format!("✅ New session {date} started")).await?;
        return Ok(true);
    }

    if cmd_text == "/restart" {
        let _ = channel
            .send(OutboundMessage::text(chat_id.to_string(), "🔄重启中").ttl(3))
            .await;
        tokio::time::sleep(Duration::from_millis(2000)).await;
        match Command::new("bash")
            .arg("-c")
            .arg("systemctl --user restart qfclaw")
            .output()
        {
            Ok(_) => return Ok(true),
            Err(_) => return Ok(false),
        };
    }

    if cmd_text == "/status" {
        let session_info = qffunc::file_to_json(SESSION_FILE)?;
        let session_id = current_id()?;
        let n = session_info[&session_id]["tokens"].as_u64().unwrap_or(0);
        let _ = channel
            .send(
                OutboundMessage::text(
                    chat_id.to_string(),
                    format!(
                        "📚 Context: {:.1}K/1M ({:.1}%)",
                        n as f64 / 1000.0,
                        n as f64 / 10000.
                    ),
                )
                .ttl(3),
            )
            .await;
    }

    if cmd_text == "/reasoning" {
        let keyboard = vec![
            vec![
                Button::new("隐藏", "reasoning_set_draft"),
                Button::new("关闭", "reasoning_disabled"),
            ],
            vec![Button::new("折叠", "reasoning_set_fold")],
        ];
        let msg_id = channel
            .send(OutboundMessage::text(chat_id.to_string(), "是否显示推理过程").buttons(keyboard))
            .await?;
        if let Some(prev) = prev_id(&msg_id, 1) {
            delete_after(channel.clone(), chat_id.to_string(), vec![prev], 0);
        }
    }

    if cmd_text.contains("/session") {
        match args.len() {
            1 => {
                reply(&channel, chat_id, fs::read_to_string(SESSION_FILE)?).await?;
            }
            _ => {
                let session_id = match args.get(2) {
                    Some(id) => *id,
                    None => bail!("错误的参数"),
                };
                let session_op = args.get(1).copied().unwrap_or("switch");
                let mut session_info = qffunc::file_to_json(SESSION_FILE)?;
                let session_now = match session_info["session_now"].as_str() {
                    Some(resp) => resp.to_string(),
                    None => date_now(),
                };
                if session_info.get(session_id).is_none() && session_now != session_id {
                    bail!(format!("未找到会话：{}", session_id))
                }

                match session_op {
                    "switch" => {
                        if session_now != session_id {
                            fs::rename(
                                format!("{}{}.json", MESSAGE_PATH, session_id),
                                format!("{}archive/{}.json", MESSAGE_PATH, session_now),
                            )?;
                            fs::rename(
                                format!("{}archive/{}.json", MESSAGE_PATH, session_id),
                                format!("{}{}.json", MESSAGE_PATH, session_id),
                            )?;
                            session_info["session_now"] = json!(session_id);
                            reply(&channel, chat_id, format!("成功切换至{}", session_id)).await?;
                        } else {
                            bail!(format!("{}已是当前会话", session_id))
                        }
                    }
                    "clear" | "archive" => {
                        if session_now != session_id {
                            _ = fs::rename(
                                format!("{}archive/{}.json", MESSAGE_PATH, session_id),
                                format!("{}{}/{}.json", MESSAGE_PATH, session_op, session_id),
                            );
                        } else {
                            _ = fs::rename(
                                format!("{}{}.json", MESSAGE_PATH, session_id),
                                format!("{}{}/{}.json", MESSAGE_PATH, session_op, session_id),
                            );
                        }
                        if let Some(obj) = session_info.as_object_mut() {
                            obj.remove(session_id);
                        }
                        let session_now = date_now();
                        session_info["session_now"] = json!(session_now);
                        reply(&channel, chat_id, format!("{}: {}成功", session_id, session_op))
                            .await?;
                    }
                    "name" => {
                        let session_title = args.get(3).copied().unwrap_or("");
                        session_info[session_id]["title"] = json!(session_title);
                        reply(
                            &channel,
                            chat_id,
                            format!("{}标题成功修改为:{}", session_id, session_title),
                        )
                        .await?;
                    }
                    _ => bail!("错误的参数"),
                }
                qffunc::json_to_file(SESSION_FILE, &session_info)?;
            }
        }
        return Ok(true);
    }

    if cmd_text.contains("/name") {
        if args.len() == 3 {
            Box::pin(exec_cmd(
                channel.clone(),
                chat_id,
                &format!("/session name {} {}", args[1], args[2]),
            ))
            .await?;
        } else {
            bail!("错误的参数")
        }
    }

    if cmd_text.contains("/clear") {
        if args.len() == 2 {
            Box::pin(exec_cmd(
                channel.clone(),
                chat_id,
                &format!("/session clear {}", args[1]),
            ))
            .await?;
        } else {
            bail!("错误的参数")
        }
    }

    Ok(true)
}

/// 处理按钮回调。
pub async fn deal_callback(channel: Arc<dyn Channel>, chat_id: &str, cb: &Callback) -> Result<bool> {
    let data = cb.data.as_str();

    if !data.contains("reasoning") {
        return Ok(true);
    }

    // 一级按钮：进入二级选择
    if data.contains("set") {
        let (draft, adaptive) = if data.contains("draft") {
            ("reasoning_draft_enabled", "reasoning_draft_adaptive")
        } else {
            ("reasoning_fold_enabled", "reasoning_fold_adaptive")
        };
        let _ = channel.delete(chat_id, &cb.msg_id).await;
        let keyboard = vec![vec![
            Button::new("开启", draft),
            Button::new("适应", adaptive),
        ]];
        let _ = channel
            .send(OutboundMessage::text(chat_id.to_string(), "请选择推理模式").buttons(keyboard))
            .await;
        return Ok(true);
    }

    // 二级按钮：写入模型配置
    let params: Vec<&str> = data.split('_').collect();
    if params.len() < 3 {
        bail!("无效的回调数据: {}", data);
    }
    let models_file = "config/models.json";
    let mut models = fs::File::open(models_file)
        .ok()
        .and_then(|file| serde_json::from_reader(file).ok())
        .unwrap_or_else(|| json!({}));
    models["config"]["reasoning"] = json!(params[2]);
    models["config"]["show_reasoning"] = json!(params[1]);
    serde_json::to_writer_pretty(fs::File::create(models_file)?, &models)?;

    let _ = channel.delete(chat_id, &cb.msg_id).await;
    let _ = channel
        .send(OutboundMessage::text(chat_id.to_string(), "✅操作成功").ttl(3))
        .await;
    Ok(true)
}
