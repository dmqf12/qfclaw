//! 斜杠指令与按钮回调处理。

use std::fs;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::json;

use crate::channel::*;
use crate::core::session;

/// 发送一条纯文本消息（不解析 Markdown）。
async fn reply(channel: &Arc<dyn Channel>, chat_id: &str, text: &str) -> Result<MsgId> {
    channel
        .send(OutboundMessage::text(chat_id.to_string(), text).kind(MsgKind::Plain))
        .await
}

pub async fn exec_cmd(channel: Arc<dyn Channel>, chat_id: &str, cmd_text: &str) -> Result<()> {
    // 新对话：话题模式下新建 Topic，否则清空当前对话的历史与 usage
    if cmd_text == "/new" || cmd_text.starts_with("/new ") {
        let title = cmd_text
            .strip_prefix("/new")
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or("新对话");
        match channel.create_topic(chat_id, title).await {
            Ok(new_chat) => {
                session::set_title(&new_chat, title)?;
                channel
                    .send(OutboundMessage::text(new_chat, "✅ 新对话已开始"))
                    .await?;
            }
            Err(_) => {
                session::reset(chat_id)?;
                reply(&channel, chat_id, "✅ 新对话已开始").await?;
            }
        }
        return Ok(());
    }

    // 删除当前对话：消息文件移到 messages/clear/，话题一并删除
    if cmd_text == "/clear" {
        session::clear(chat_id)?;
        // 话题删除后无法再发消息，只有删除失败时才回执
        if channel.delete_topic(chat_id).await.is_err() {
            reply(&channel, chat_id, "🧹 当前对话已清除").await?;
        }
        return Ok(());
    }

    if cmd_text == "/restart" {
        let _ = channel
            .send(OutboundMessage::text(chat_id.to_string(), "🔄重启中").ttl(3))
            .await;
        tokio::time::sleep(Duration::from_millis(2000)).await;
        Command::new("bash")
            .arg("-c")
            .arg("systemctl --user restart qfclaw")
            .output()
            .map_err(|e| anyhow!("重启失败: {e}"))?;
        return Ok(());
    }

    if cmd_text == "/status" {
        let n = session::usage(chat_id)["total_tokens"].as_u64().unwrap_or(0);
        let title = session::title(chat_id);
        let title = if title.is_empty() { "（无标题）" } else { &title };
        let _ = channel
            .send(
                OutboundMessage::text(
                    chat_id.to_string(),
                    &format!(
                        "🏷 {title}\n📚 Context: {:.1}K/1M ({:.1}%)",
                        n as f64 / 1000.0,
                        n as f64 / 10000.
                    ),
                )
                .ttl(5),
            )
            .await;
        return Ok(());
    }

    if cmd_text == "/reasoning" {
        let keyboard = vec![
            vec![
                Button::new("隐藏", "reasoning_set_draft"),
                Button::new("关闭", "reasoning_disabled"),
            ],
            vec![Button::new("折叠", "reasoning_set_fold")],
        ];
        channel
            .send(OutboundMessage::text(chat_id.to_string(), "是否显示推理过程").buttons(keyboard))
            .await?;
        return Ok(());
    }

    Ok(())
}

/// 处理按钮回调。
pub async fn deal_callback(channel: Arc<dyn Channel>, chat_id: &str, cb: &Callback) -> Result<()> {
    let data = cb.data.as_str();

    if !data.contains("reasoning") {
        return Ok(());
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
        return Ok(());
    }

    // 二级按钮：写入模型配置
    let params: Vec<&str> = data.split('_').collect();
    if params.len() < 3 {
        bail!("无效的回调数据: {}", data);
    }
    let models_file = "../config/models.json";
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
    Ok(())
}
