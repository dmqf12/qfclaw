mod channel;
mod config;
mod core;
mod tool;

use std::collections::HashMap;
use std::sync::Arc;

use channel::{Channel, InboundMessage, MsgKind, OutboundMessage, telegram};
use core::{agent, command, session};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// 正在运行的对话任务：句柄、消息注入通道、取消令牌。
type RunningTask = (JoinHandle<()>, mpsc::Sender<String>, CancellationToken);

/// 分发收到的消息：白名单校验 + 斜杠指令 + 按会话调度。
///
/// 会话键为 `chat_id` 或 `chat_id:thread_id`（Telegram 话题），
/// 每个会话各自运行一个对话任务，话题之间互不干扰。
/// 同一会话任务运行期间再收到消息，则通过通道注入，
/// 由对话循环在下一轮请求前合并处理（实现「打断」效果）。
async fn handle_msg(
    channel: Arc<dyn Channel>,
    mut rx: mpsc::Receiver<InboundMessage>,
) {
    let mut tasks: HashMap<String, RunningTask> = HashMap::new();

    while let Some(msg) = rx.recv().await {
        let chat_id = msg.chat_id.clone();

        // 白名单校验（服务消息、回调不回复，避免刷屏）
        if msg.user_id != channel.get_allow_id() {
            if msg.content.is_empty() {
                continue;
            }
            let _ = channel
                .send(
                    OutboundMessage::text(
                        chat_id,
                        &format!("不在白名单，您的id：\n      {}", msg.user_id),
                    )
                    .kind(MsgKind::Error),
                )
                .await;
            continue;
        }

        // 话题标题（用户新建/改名话题时由服务消息下发）
        if let Some(name) = &msg.topic_name
            && let Err(e) = session::set_title(&chat_id, name)
        {
            eprintln!("记录话题标题失败: {e}");
        }

        // 按钮回调交给指令模块
        if let Some(callback) = &msg.callback {
            if let Err(e) = command::deal_callback(channel.clone(), &chat_id, callback).await {
                eprintln!("处理回调失败: {e}");
            }
            continue;
        }

        if msg.content.is_empty() {
            continue;
        }

        // 停止当前会话的任务
        if msg.content == "/stop" {
            if let Some((handle, _, token)) = tasks.remove(&chat_id) {
                token.cancel();
                handle.abort();
                let _ = channel
                    .send(OutboundMessage::text(chat_id, "🛑 任务已停止"))
                    .await;
            } else {
                let _ = channel
                    .send(OutboundMessage::text(chat_id, "⚠️ 当前没有正在运行的任务"))
                    .await;
            }
            continue;
        }

        // 会重置/删除当前对话的指令，先停掉本会话任务，避免旧历史被写回覆盖。
        // 话题模式下的 /new 是新建 Topic，不影响任何任务。
        let reset_current = msg.content == "/clear"
            || ((msg.content == "/new" || msg.content.starts_with("/new "))
                && !chat_id.contains(':'));
        if reset_current
            && let Some((handle, _, token)) = tasks.remove(&chat_id)
        {
            token.cancel();
            handle.abort();
        }

        // 斜杠指令
        if ["/new", "/clear", "/restart", "/status", "/reasoning"]
            .iter()
            .any(|cmd| msg.content.starts_with(cmd))
        {
            let channel = channel.clone();
            let chat_id = chat_id.clone();
            let cmd = msg.content.clone();
            tokio::spawn(async move {
                if let Err(e) = command::exec_cmd(channel.clone(), &chat_id, &cmd).await {
                    let _ = channel
                        .send(
                            OutboundMessage::text(chat_id, &format!("指令执行失败: {e}"))
                                .kind(MsgKind::Error),
                        )
                        .await;
                }
            });
            continue;
        }

        // 清理该会话已结束的任务
        if tasks
            .get(&chat_id)
            .is_some_and(|(handle, _, _)| handle.is_finished())
        {
            tasks.remove(&chat_id);
        }

        let content = msg.content;
        if let Some((_, tx, _)) = tasks.get(&chat_id) {
            let _ = tx.send(content).await;
            continue;
        }

        let (tx, rx_chat) = mpsc::channel::<String>(32);
        let task_channel = channel.clone();
        let task_chat_id = chat_id.clone();
        let token = CancellationToken::new();
        let child_token = token.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = agent::main(
                task_channel.clone(),
                task_chat_id.clone(),
                &content,
                rx_chat,
                child_token,
            )
            .await
            {
                eprintln!("对话失败: {e}");
                let _ = task_channel
                    .send(
                        OutboundMessage::text(task_chat_id, &format!("对话失败: {e}"))
                            .kind(MsgKind::Error),
                    )
                    .await;
            }
        });
        tasks.insert(chat_id, (handle, tx, token));
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 清理上次运行遗留的临时任务目录
    let _ = std::fs::remove_dir_all("qfclawtask");
    std::fs::create_dir_all("workspace/")?;
    // 后续的相对路径（config / messages / workspace）都基于 workspace 目录
    std::env::set_current_dir("workspace/")?;

    let channel: Arc<dyn Channel> = match telegram::TelegramChannel::new() {
        Ok(cfg) => Arc::new(cfg),
        Err(e) => {
            eprintln!("初始化 Telegram 失败: {e}");
            return Ok(());
        }
    };

    match channel.init().await {
        Ok(_) => println!("指令注册成功"),
        Err(e) => println!("指令注册失败: {e}"),
    }

    let (tx, rx) = mpsc::channel(32);

    let run_channel = channel.clone();
    let handle_updates = tokio::spawn(async move {
        if let Err(e) = run_channel.run(tx).await {
            eprintln!("接收循环退出: {e}");
        }
    });
    let handle_messages = tokio::spawn(handle_msg(channel.clone(), rx));

    let _ = tokio::join!(handle_updates, handle_messages);
    Ok(())
}
