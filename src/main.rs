mod channel;
mod config;
mod core;
mod tool;

use std::sync::Arc;

use channel::*;
use channel::{Channel, InboundMessage, MsgKind, OutboundMessage};
use core::{agent, command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// 正在运行的对话任务：句柄、消息注入通道、取消令牌。
type RunningTask = (JoinHandle<()>, mpsc::Sender<String>, CancellationToken);

/// 分发收到的消息：白名单校验 + 斜杠指令 + 单任务调度。
///
/// 同一时刻只运行一个对话任务；任务运行期间再收到消息，则通过通道注入，
/// 由对话循环在下一轮请求前合并处理（实现「打断」效果）。
async fn handle_msg(
    channel: Arc<dyn Channel>,
    mut rx: mpsc::Receiver<InboundMessage>,
) {
    let mut current_task: Option<RunningTask> = None;

    while let Some(msg) = rx.recv().await {
        let chat_id = msg.chat_id.clone();

        // 白名单校验
        if msg.user_id != channel.get_allow_id() {
            let _ = channel
                .send(
                    OutboundMessage::text(
                        chat_id,
                        format!("不在白名单，您的id：\n      {}", msg.user_id),
                    )
                    .kind(MsgKind::Error),
                )
                .await;
            continue;
        }

        // 按钮回调交给指令模块
        if let Some(callback) = &msg.callback {
            if let Err(e) = command::deal_callback(channel.clone(), &chat_id, callback).await {
                eprintln!("处理回调失败: {e}");
            }
            continue;
        }

        let input = msg.content.as_text().to_string();
        if input.is_empty() {
            continue;
        }

        // 停止当前任务
        if input == "/stop" {
            if let Some((handle, _, token)) = current_task.take() {
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

        // 斜杠指令
        if ["/session", "/new", "/clear", "/name", "/restart", "/status", "/reasoning"]
            .iter()
            .any(|cmd| input.starts_with(cmd))
        {
            let channel = channel.clone();
            let chat_id = chat_id.clone();
            let cmd = input.clone();
            tokio::spawn(async move {
                if let Err(e) = command::exec_cmd(channel.clone(), &chat_id, &cmd).await {
                    let _ = channel
                        .send(
                            OutboundMessage::text(chat_id, format!("指令执行失败: {e}"))
                                .kind(MsgKind::Error),
                        )
                        .await;
                }
            });
            continue;
        }

        // 单任务调度
        if let Some((handle, _, _)) = &current_task
            && handle.is_finished()
        {
            current_task = None;
        }

        if current_task.is_none() {
            let (tx, rx_chat) = mpsc::channel::<String>(32);
            let channel = channel.clone();
            let chat_id = chat_id.clone();
            let first_input = input.clone();
            let token = CancellationToken::new();
            let child_token = token.clone();
            let handle = tokio::spawn(async move {
                if let Err(e) =
                    agent::main(channel, chat_id, &first_input, rx_chat, child_token).await
                {
                    eprintln!("对话失败: {e}");
                }
            });
            current_task = Some((handle, tx, token));
        } else if let Some((_, tx, _)) = &current_task {
            let _ = tx.send(input).await;
        }
    }
}

#[tokio::main]
async fn main() {
    let _ = std::fs::remove_dir_all("qfclawtask");

    for cfg_init in [telegram::TelegramChannel::new()] {
        let channel: Arc<dyn Channel>;
        if let Ok(cfg) = cfg_init {
            channel = Arc::new(cfg);
        } else {
            continue;
        }
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
    }
}
