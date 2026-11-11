//! 斜杠指令与按钮回调处理。

use std::fs;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::channel::*;
use crate::config::ModelConfig;
use crate::core::compact;
use crate::core::session;
use crate::log;

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
                log::info(
                    "cmd",
                    format!("{} 新建话题 {new_chat}「{title}」", log::chat(chat_id)),
                );
                channel
                    .send(OutboundMessage::text(new_chat, "✅ 新对话已开始"))
                    .await?;
            }
            Err(e) => {
                log::warn(
                    "cmd",
                    format!("{} 建话题失败，改为重置当前对话：{e}", log::chat(chat_id)),
                );
                session::reset(chat_id)?;
                reply(&channel, chat_id, "✅ 新对话已开始").await?;
            }
        }
        return Ok(());
    }

    // 删除当前对话：消息文件移到 messages/clear/，话题一并删除
    if cmd_text == "/clear" {
        session::clear(chat_id)?;
        log::info("cmd", format!("{} 对话已归档", log::chat(chat_id)));
        // 话题删除后无法再发消息，只有删除失败时才回执
        if channel.delete_topic(chat_id).await.is_err() {
            reply(&channel, chat_id, "🧹 当前对话已清除").await?;
        }
        return Ok(());
    }

    // 压缩上下文：把历史总结进 memory，messages 只留最后一条用户消息
    if cmd_text == "/compact" {
        // 压缩要先调用一次模型，耗时可能几十秒，先回执避免用户以为没反应
        let notice = reply(&channel, chat_id, "🗜 正在压缩上下文…").await.ok();
        let result = compact::compact(chat_id).await;
        if let Some(id) = &notice {
            let _ = channel.delete(chat_id, id).await;
        }
        let report = result?;
        let text = format!(
            "🗜 压缩完成\n消息：{} → {} 条\n记忆：{} 字\n压缩前上下文：{:.1}K tokens",
            report.before_messages,
            report.after_messages,
            report.memory_chars,
            report.before_tokens as f64 / 1000.0
        );
        log::info("cmd", format!("{} 手动压缩", log::chat(chat_id)));
        reply(&channel, chat_id, &text).await?;
        return Ok(());
    }

    if cmd_text == "/restart" {
        log::info("cmd", format!("{} 请求重启服务", log::chat(chat_id)));
        let _ = channel
            .send(OutboundMessage::text(chat_id.to_string(), "🔄重启中").ttl(3))
            .await;
        tokio::time::sleep(Duration::from_millis(2000)).await;
        let output = Command::new("bash")
            .arg("-c")
            .arg("systemctl --user restart qfclaw")
            .output()
            .map_err(|e| anyhow!("重启失败: {e}"))?;
        log::info(
            "cmd",
            format!(
                "systemctl restart 退出码 {:?}：{}",
                output.status.code(),
                log::preview(&String::from_utf8_lossy(&output.stderr), 200)
            ),
        );
        return Ok(());
    }

    if cmd_text == "/status" {
        let (title, used) = session::status(chat_id);
        let title = if title.is_empty() {
            "（无标题）"
        } else {
            &title
        };
        // 分母取配置里的压缩阈值；读不到配置时只显示已用量，不编造上限
        let text = match ModelConfig::load() {
            Ok(cfg) if cfg.context_window > 0 => format!(
                "🆔 {chat_id}\n🏷 {title}\n📚 Context: {}/{} ({:.1}%)",
                human_tokens(used),
                human_tokens(cfg.context_window),
                used as f64 / cfg.context_window as f64 * 100.0
            ),
            _ => format!(
                "🆔 {chat_id}\n🏷 {title}\n📚 Context: {} tokens",
                human_tokens(used)
            ),
        };
        log::info("cmd", format!("{} /status {used}", log::chat(chat_id)));
        reply(&channel, chat_id, &text).await?;
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

    // 上下文窗口阈值：不带参数弹按钮，带参数直接设置
    if let Some(arg) = cmd_text.strip_prefix("/context") {
        let arg = arg.trim();
        if arg.is_empty() {
            let current = ModelConfig::load().map(|c| c.context_window).unwrap_or(0);
            let keyboard = vec![
                vec![
                    Button::new("100K", "context_set_100000"),
                    Button::new("300K", "context_set_300000"),
                    Button::new("500K", "context_set_500000"),
                ],
                vec![
                    Button::new("700K", "context_set_700000"),
                    Button::new("900K", "context_set_900000"),
                    Button::new("1M", "context_set_1000000"),
                ],
            ];
            let text = format!(
                "🧠 上下文窗口阈值：{}\n超过该值后自动压缩历史",
                human_tokens(current)
            );
            channel
                .send(OutboundMessage::text(chat_id.to_string(), &text).buttons(keyboard))
                .await?;
            return Ok(());
        }
        match arg.parse::<u64>() {
            Ok(value) if value > 0 => {
                save_context_window(value)?;
                reply(
                    &channel,
                    chat_id,
                    &format!("✅ 上下文窗口已设为 {}", human_tokens(value)),
                )
                .await?;
            }
            _ => {
                reply(
                    &channel,
                    chat_id,
                    "❌ 请输入正整数 token 数，例如 /context 500000",
                )
                .await?;
            }
        }
        return Ok(());
    }

    Ok(())
}

/// 处理按钮回调。
pub async fn deal_callback(channel: Arc<dyn Channel>, chat_id: &str, cb: &Callback) -> Result<()> {
    let data = cb.data.as_str();

    // 一级按钮：展开二级选择（隐藏 / 折叠）
    if let Some(show) = data.strip_prefix("reasoning_set_") {
        let keyboard = vec![vec![
            Button::new("开启", format!("reasoning_{show}_enabled")),
            Button::new("适应", format!("reasoning_{show}_adaptive")),
        ]];
        let _ = channel.delete(chat_id, &cb.msg_id).await;
        let _ = channel
            .send(OutboundMessage::text(chat_id.to_string(), "请选择推理模式").buttons(keyboard))
            .await;
        return Ok(());
    }

    // 上下文窗口按钮
    if let Some(value) = data.strip_prefix("context_set_") {
        let value = value.parse::<u64>().unwrap_or(0);
        if value > 0 {
            save_context_window(value)?;
            let _ = channel.delete(chat_id, &cb.msg_id).await;
            let _ = channel
                .send(
                    OutboundMessage::text(
                        chat_id.to_string(),
                        &format!("✅ 上下文窗口已设为 {}", human_tokens(value)),
                    )
                    .ttl(3),
                )
                .await;
        }
        return Ok(());
    }

    // 一级按钮：关闭推理
    if data == "reasoning_disabled" {
        save_reasoning("disabled", None)?;
        done(&channel, chat_id, &cb.msg_id).await;
        return Ok(());
    }

    // 二级按钮：reasoning_<显示方式>_<模式>
    if let Some((show, mode)) = parse_mode_button(data) {
        save_reasoning(mode, Some(show))?;
        done(&channel, chat_id, &cb.msg_id).await;
    }
    Ok(())
}

/// token 数的易读写法：1500 → `1.5K`，300000 → `300K`，1000000 → `1M`。
fn human_tokens(n: u64) -> String {
    let (value, unit) = if n >= 1_000_000 {
        (n as f64 / 1_000_000.0, "M")
    } else if n >= 1_000 {
        (n as f64 / 1_000.0, "K")
    } else {
        return n.to_string();
    };
    let text = format!("{value:.1}");
    format!("{}{unit}", text.trim_end_matches(".0"))
}

/// 解析二级按钮数据，返回 `(显示方式, 推理模式)`。
///
/// 数据形如 `reasoning_draft_enabled`、`reasoning_fold_adaptive`。
fn parse_mode_button(data: &str) -> Option<(&str, &str)> {
    let (show, mode) = data.strip_prefix("reasoning_")?.split_once('_')?;
    let valid_show = matches!(show, "draft" | "fold");
    let valid_mode = matches!(mode, "enabled" | "adaptive");
    (valid_show && valid_mode).then_some((show, mode))
}

/// 回调处理完成：删除原消息并回执。
async fn done(channel: &Arc<dyn Channel>, chat_id: &str, msg_id: &MsgId) {
    let _ = channel.delete(chat_id, msg_id).await;
    let _ = channel
        .send(OutboundMessage::text(chat_id.to_string(), "✅操作成功").ttl(3))
        .await;
}

/// 原子写入 models.json 的 `config` 字段。
///
/// 配置文件读不出来时直接报错，绝不用空对象覆盖（否则会丢掉全部模型配置）；
/// 写入走临时文件 + rename，避免中途崩溃留下半截配置。
fn update_models_config(key: &str, value: Value) -> Result<()> {
    let models_file = "../config/models.json";
    let mut models: Value = serde_json::from_reader(fs::File::open(models_file)?)?;
    models["config"][key] = value;
    let tmp = format!("{models_file}.tmp");
    serde_json::to_writer_pretty(fs::File::create(&tmp)?, &models)?;
    fs::rename(&tmp, models_file)?;
    Ok(())
}

/// 写入推理配置。
fn save_reasoning(reasoning: &str, show: Option<&str>) -> Result<()> {
    update_models_config("reasoning", json!(reasoning))?;
    if let Some(show) = show {
        update_models_config("show_reasoning", json!(show))?;
    }
    log::info(
        "cmd",
        format!(
            "推理设置已保存：reasoning={reasoning} show={}",
            show.unwrap_or("不变")
        ),
    );
    Ok(())
}

/// 写入上下文压缩阈值。
fn save_context_window(tokens: u64) -> Result<()> {
    update_models_config("context_window", json!(tokens))?;
    log::info("cmd", format!("上下文窗口阈值已设为 {tokens} tokens"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_tokens_formats() {
        assert_eq!(human_tokens(0), "0");
        assert_eq!(human_tokens(999), "999");
        assert_eq!(human_tokens(1000), "1K");
        assert_eq!(human_tokens(1500), "1.5K");
        assert_eq!(human_tokens(300_000), "300K");
        assert_eq!(human_tokens(1_000_000), "1M");
    }

    #[test]
    fn context_window_write_is_atomic() {
        // 用例会切换工作目录，和会话测试互斥
        let _guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::path::Path::new("/tmp/qfclaw-command-test/workspace");
        let _ = fs::remove_dir_all("/tmp/qfclaw-command-test");
        fs::create_dir_all(dir).unwrap();
        std::env::set_current_dir(dir).unwrap();
        fs::create_dir_all("../config").unwrap();
        fs::write(
            "../config/models.json",
            r#"{"use_model":"x","x":{"token":"t"},"config":{"context_window":300000}}"#,
        )
        .unwrap();

        save_context_window(500_000).unwrap();

        let text = fs::read_to_string("../config/models.json").unwrap();
        let doc: Value = serde_json::from_str(&text).unwrap();
        // 只改了目标字段，其他配置保留
        assert_eq!(doc["config"]["context_window"], 500_000);
        assert_eq!(doc["x"]["token"], "t");
        // 临时文件不残留
        assert!(!std::path::Path::new("../config/models.json.tmp").exists());

        std::env::set_current_dir("/").ok();
        let _ = fs::remove_dir_all("/tmp/qfclaw-command-test");
    }
}
