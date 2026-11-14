mod channel;
mod config;
mod core;
mod log;
mod service;
mod tool;
mod util;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use channel::{Channel, InboundMessage, MsgKind, OutboundMessage, telegram};
use core::{agent, command, session};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// 一个会话正在运行的对话任务。
struct RunningTask {
    handle: JoinHandle<()>,
    /// 对话进行中注入新消息的通道。
    input: mpsc::Sender<String>,
    /// 中止当前工具调用用的令牌（可替换）。
    cancel: Arc<Mutex<CancellationToken>>,
    /// 正在执行的工具数量。
    active: Arc<AtomicUsize>,
}

/// 中止会话正在执行的工具调用，返回给用户的提示。
///
/// 只取消「当前这一代」令牌，随后立刻换新令牌，所以后续工具调用不受影响；
/// 已转入后台的任务完全不在这个范围内。
async fn stop_tools(tasks: &HashMap<String, RunningTask>, chat_id: &str) -> &'static str {
    match tasks.get(chat_id) {
        Some(task) if task.active.load(Ordering::SeqCst) > 0 => {
            let mut guard = task.cancel.lock().await;
            guard.cancel();
            *guard = CancellationToken::new();
            "🛑 已中止当前工具调用"
        }
        Some(_) => "⚠️ 当前没有正在执行的工具调用",
        None => "⚠️ 当前没有正在运行的任务",
    }
}

/// 为一个会话启动对话任务。
fn spawn_task(channel: Arc<dyn Channel>, chat_id: String, content: String) -> RunningTask {
    let (tx, rx_chat) = mpsc::channel::<String>(32);
    let cancel = Arc::new(Mutex::new(CancellationToken::new()));
    let active = Arc::new(AtomicUsize::new(0));

    let task_channel = channel.clone();
    let task_chat_id = chat_id.clone();
    let task_cancel = cancel.clone();
    let task_active = active.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = agent::main(
            task_channel.clone(),
            task_chat_id.clone(),
            &content,
            rx_chat,
            task_cancel,
            task_active,
        )
        .await
        {
            log::error(
                "agent",
                format!("{} 任务失败：{e}", log::chat(&task_chat_id)),
            );
            let _ = task_channel
                .send(
                    OutboundMessage::text(task_chat_id, &format!("对话失败: {e}"))
                        .kind(MsgKind::Error),
                )
                .await;
        }
    });

    RunningTask {
        handle,
        input: tx,
        cancel,
        active,
    }
}

/// 分发收到的消息：白名单校验 + 斜杠指令 + 按会话调度。
///
/// 会话键为 `chat_id` 或 `chat_id:thread_id`（Telegram 话题），
/// 每个会话各自运行一个对话任务，话题之间互不干扰。
/// 同一会话任务运行期间再收到消息，则通过通道注入，
/// 由对话循环在下一轮请求前合并处理（实现「打断」效果）。
async fn handle_msg(channel: Arc<dyn Channel>, mut rx: mpsc::Receiver<InboundMessage>) {
    let mut tasks: HashMap<String, RunningTask> = HashMap::new();

    while let Some(msg) = rx.recv().await {
        let chat_id = msg.chat_id.clone();
        let chat = log::chat(&chat_id);
        let what = match &msg.callback {
            Some(callback) => format!("⚙ {}", callback.data),
            None => format!("↓ {}", log::preview(&msg.content, 80)),
        };
        log::info("tg", format!("{chat} {what}"));

        // 白名单校验（服务消息、回调不回复，避免刷屏）
        if !channel.is_allowed(&msg.user_id) {
            if msg.content.is_empty() {
                continue;
            }
            log::warn("tg", format!("{chat} 白名单外用户 {} 被拒绝", msg.user_id));
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
            log::error("session", format!("记录话题标题失败：{e}"));
        }

        // 按钮回调交给指令模块
        if let Some(callback) = &msg.callback {
            if let Err(e) = command::deal_callback(channel.clone(), &chat_id, callback).await {
                log::error("cmd", format!("处理回调失败：{e}"));
            }
            continue;
        }

        // 只有纯附件、没有文字的消息也要处理
        if msg.content.is_empty() && msg.files.is_empty() {
            continue;
        }

        // 中止当前正在前台执行的工具调用；后台任务与对话本身不受影响
        if msg.content == "/stop" {
            let reply = stop_tools(&tasks, &chat_id).await;
            log::info("cmd", format!("{chat} /stop {reply}"));
            let _ = channel.send(OutboundMessage::text(chat_id, reply)).await;
            continue;
        }

        // /compact 与运行中的任务冲突：压缩结果会被任务保存的历史覆盖，直接丢弃
        if msg.content == "/compact"
            && tasks
                .get(&chat_id)
                .is_some_and(|task| !task.handle.is_finished())
        {
            log::warn("cmd", format!("{chat} 对话仍在输出，丢弃 /compact"));
            let _ = channel
                .send(OutboundMessage::text(
                    chat_id,
                    "⏳ 当前对话仍在输出，已忽略 /compact",
                ))
                .await;
            continue;
        }

        // 会重置/删除当前对话的指令，先停掉本会话任务，避免旧历史被写回覆盖。
        // 话题模式下的 /new 是新建 Topic，不影响任何任务。
        let reset_current = msg.content == "/clear"
            || ((msg.content == "/new" || msg.content.starts_with("/new "))
                && !chat_id.contains(':'));
        if reset_current && let Some(task) = tasks.remove(&chat_id) {
            log::info("cmd", format!("{chat} 重置对话，先中止运行中的任务"));
            task.cancel.lock().await.cancel();
            task.handle.abort();
        }

        // 斜杠指令
        if [
            "/new",
            "/clear",
            "/compact",
            "/restart",
            "/status",
            "/reasoning",
            "/context",
        ]
        .iter()
        .any(|cmd| msg.content.starts_with(cmd))
        {
            log::info("cmd", format!("{chat} {}", msg.content));
            let channel = channel.clone();
            let chat_id = chat_id.clone();
            let cmd = msg.content.clone();
            tokio::spawn(async move {
                if let Err(e) = command::exec_cmd(channel.clone(), &chat_id, &cmd).await {
                    log::error("cmd", format!("{chat} 指令 {cmd} 失败：{e}"));
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
            .is_some_and(|task| task.handle.is_finished())
        {
            tasks.remove(&chat_id);
        }

        // 附件以 `[类型]路径` 附在用户消息后面，由模型自行决定怎么处理
        let mut content = msg.content;
        for file in &msg.files {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(&format!("[{}]{}", file.kind, file.path));
            log::info("tg", format!("{chat} 附件 [{}]{}", file.kind, file.path));
        }

        if let Some(task) = tasks.get(&chat_id) {
            log::info(
                "agent",
                format!("{chat} 追加 {}", log::preview(&content, 80)),
            );
            let _ = task.input.send(content).await;
            continue;
        }

        log::info(
            "agent",
            format!("{chat} 开始 {}", log::preview(&content, 80)),
        );
        tasks.insert(
            chat_id.clone(),
            spawn_task(channel.clone(), chat_id, content),
        );
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 安装模式：`qfclaw --systemd` 只按当前目录生成用户级 systemd 服务，
    // 生成完就退出，不启动对话循环（避免和已在运行的服务抢 Telegram 更新）
    if std::env::args().any(|arg| arg == "--systemd") {
        service::ensure();
        return Ok(());
    }

    // 清理上次运行遗留的临时任务目录
    let _ = std::fs::remove_dir_all("qfclawtask");
    std::fs::create_dir_all("workspace/")?;
    // 后续的相对路径（config / messages / workspace）都基于 workspace 目录
    std::env::set_current_dir("workspace/")?;
    // 附件目录：每次启动清空，避免长期堆积
    let _ = std::fs::remove_dir_all("files");
    std::fs::create_dir_all("files")?;

    log::info(
        "main",
        format!(
            "qfclaw {} 启动，工作目录 {}",
            env!("CARGO_PKG_VERSION"),
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "?".to_string())
        ),
    );
    match config::ModelConfig::load() {
        Ok(cfg) => log::info(
            "main",
            format!(
                "模型 {} @ {}，推理 {}，上下文阈值 {} tokens",
                cfg.model_name, cfg.base_url, cfg.reasoning, cfg.context_window
            ),
        ),
        Err(e) => log::error("main", format!("读取模型配置失败：{e}")),
    }

    let channel: Arc<dyn Channel> = match telegram::TelegramChannel::new() {
        Ok(cfg) => Arc::new(cfg),
        Err(e) => {
            log::error("main", format!("初始化 Telegram 失败：{e}"));
            return Ok(());
        }
    };

    match channel.init().await {
        Ok(_) => log::info("main", "指令注册成功"),
        Err(e) => log::error("main", format!("指令注册失败：{e}")),
    }

    let (tx, rx) = mpsc::channel(32);

    let run_channel = channel.clone();
    let handle_updates = tokio::spawn(async move {
        if let Err(e) = run_channel.run(tx).await {
            log::error("tg", format!("接收循环退出：{e}"));
        }
    });
    let handle_messages = tokio::spawn(handle_msg(channel.clone(), rx));

    let _ = tokio::join!(handle_updates, handle_messages);
    Ok(())
}
