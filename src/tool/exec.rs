//! 命令执行工具。

use std::fs;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};

use super::task::{TASKS, TaskHandle};
use super::{MAX_OUTPUT_CHARS, NOTIFY_TTL_SECS, Tool, ToolContext, notify, truncate};
use crate::channel::{MsgKind, OutboundMessage};

pub struct ExecTool;

/// 命令的结束方式。
enum Outcome {
    Done(std::io::Result<std::process::ExitStatus>),
    Timeout,
    Cancelled,
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str {
        "exec"
    }

    fn definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "exec",
                "description": "执行命令。如果在指定时间内未完成，将返回当前输出并转入后台运行。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "要执行的命令" },
                        "timeout": { "type": "integer", "description": "最大等待时长（秒），默认 10", "default": 10 }
                    },
                    "required": ["command"]
                }
            }
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> String {
        run(&args, ctx).await
    }
}

async fn run(args: &Value, ctx: &ToolContext) -> String {
    let cmd_text = args["command"].as_str().unwrap_or("");
    let timeout_secs = args["timeout"].as_u64().unwrap_or(10);
    let task_id = format!("{:08x}", fastrand::u32(..));
    // 绝对路径：避免命令内 cd 切换目录后相对路径失效
    let task_dir = match std::env::current_dir() {
        Ok(dir) => dir.join("../qfclawtask").join(&task_id),
        Err(e) => return format!("❌ 无法定位工作目录: {e}"),
    };
    let abs_task_dir = task_dir.to_string_lossy().to_string();

    // 执行提示（命令结束后删除；子代理上下文不发）
    let start_msg = format!("⚡️执行：{}  ⌚️超时：{}", cmd_text, timeout_secs);
    if !ctx.silent {
        let _ = ctx
            .channel
            .send(
                OutboundMessage::text(ctx.chat_id.clone(), &start_msg)
                    .kind(MsgKind::Text)
                    .fold()
                    .ttl(NOTIFY_TTL_SECS),
            )
            .await;
    }

    // 准备脚本：激活虚拟环境后再执行命令
    if let Err(e) = fs::create_dir_all(&abs_task_dir) {
        return format!("❌ 创建任务目录失败: {e}");
    }
    let script = format!("source pyvenv/bin/activate\n{cmd_text}");
    if let Err(e) = fs::write(format!("{abs_task_dir}/exec.sh"), script) {
        return format!("❌ 写入执行脚本失败: {e}");
    }

    // 启动命令
    let mut child = match Command::new("bash")
        .arg(format!("{abs_task_dir}/exec.sh"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 独立进程组：取消时可以连同子进程一起结束
        .process_group(0)
        // 任务被取消/超时丢弃时一并结束子进程，避免残留
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return format!("❌ 启动命令失败: {e}"),
    };

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // 异步捕获输出（stdout + stderr 合并），只保留最新部分
    let output = Arc::new(Mutex::new(String::new()));
    let output_writer = output.clone();
    tokio::spawn(async move {
        let mut reader = stdout.chain(stderr);
        let mut buf = [0u8; 1024];
        while let Ok(n) = reader.read(&mut buf).await {
            if n == 0 {
                break;
            }
            let mut lock = output_writer.lock().await;
            lock.push_str(&String::from_utf8_lossy(&buf[..n]));
            if lock.chars().count() > MAX_OUTPUT_CHARS {
                *lock = truncate(&lock, MAX_OUTPUT_CHARS);
            }
        }
    });

    // 等待：正常结束 / 超时 / 取消
    let cancel = ctx.cancel_token().await;
    let outcome = tokio::select! {
        _ = cancel.cancelled() => Outcome::Cancelled,
        r = timeout(Duration::from_secs(timeout_secs), child.wait()) => match r {
            Ok(wait) => Outcome::Done(wait),
            Err(_) => Outcome::Timeout,
        },
    };

    match outcome {
        Outcome::Done(wait) => {
            let log = output.lock().await.clone();
            // code() 为 None 表示被信号终止，不能当成成功
            let code = match wait {
                Ok(status) => status.code().unwrap_or(-1),
                Err(_) => -1,
            };
            let _ = fs::remove_dir_all(&abs_task_dir);
            format!("⚡任务完成 (Code {}):\n{}", code, log)
        }
        // 超时与用户中止都保留现场：进程转入后台，由 operate_task 继续管理
        outcome @ (Outcome::Timeout | Outcome::Cancelled) => {
            let pid = child.id();
            TASKS.lock().await.insert(
                task_id.clone(),
                TaskHandle {
                    child,
                    output: output.clone(),
                    pid,
                },
            );
            let log = output.lock().await.clone();
            let msg = match outcome {
                Outcome::Timeout => format!("⏳任务超时转入后台，ID: {task_id}"),
                _ => format!("⏸ 用户中止，任务已转入后台，ID: {task_id}"),
            };
            notify(ctx, &msg).await;
            format!("{}\n当前输出:\n{}", msg, log)
        }
    }
}
