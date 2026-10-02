//! 命令执行工具。

use std::fs;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};
use uuid::Uuid;

use super::task::{TASKS, TaskHandle};
use super::{MAX_OUTPUT_CHARS, Tool, ToolContext, notify};
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

/// 保留最新输出：超出上限时丢弃最旧的字符。
fn keep_latest(text: &mut String) {
    let total = text.chars().count();
    if total > MAX_OUTPUT_CHARS {
        let cut = text
            .char_indices()
            .nth(total - MAX_OUTPUT_CHARS)
            .map(|(i, _)| i)
            .unwrap_or(text.len());
        text.replace_range(..cut, "");
    }
}

async fn run(args: &Value, ctx: &ToolContext) -> String {
    let cmd_text = args["command"].as_str().unwrap_or("");
    let timeout_secs = args["timeout"].as_u64().unwrap_or(10);
    let task_id = Uuid::new_v4().to_string();
    // 绝对路径：避免命令内 cd 切换目录后相对路径失效
    let abs_task_dir = std::env::current_dir()
        .unwrap()
        .join("qfclawtask")
        .join(&task_id)
        .to_string_lossy()
        .to_string();

    // 执行提示（命令结束后删除）
    let start_msg = format!("⚡️执行：{}  ⌚️超时：{}", cmd_text, timeout_secs);
    let msg_id = ctx
        .channel
        .send(
            OutboundMessage::text(ctx.chat_id.clone(), start_msg)
                .kind(MsgKind::Progress)
                .fold(),
        )
        .await
        .ok();

    // 准备脚本：激活虚拟环境 + sudo 包装
    let _ = Command::new("mkdir")
        .args(["-p", &abs_task_dir])
        .status()
        .await;
    let exec_content =
        format!("source workspace/pyvenv/bin/activate\n{cmd_text}");
    _ = fs::write(format!("{abs_task_dir}/exec.sh"), exec_content);
    let _ = Command::new("chmod")
        .args(["+x", &format!("{abs_task_dir}/exec.sh")])
        .status()
        .await;

    // 启动命令
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(format!("{abs_task_dir}/exec.sh"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn");

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
            keep_latest(&mut lock);
        }
    });

    // 等待：正常结束 / 超时 / 取消
    let outcome = tokio::select! {
        _ = ctx.cancel.cancelled() => Outcome::Cancelled,
        r = timeout(Duration::from_secs(timeout_secs), child.wait()) => match r {
            Ok(wait) => Outcome::Done(wait),
            Err(_) => Outcome::Timeout,
        },
    };

    let result = match outcome {
        Outcome::Done(wait) => {
            let log = output.lock().await.clone();
            let code = wait.map(|s| s.code().unwrap_or(0)).unwrap_or(-1);
            let _ = Command::new("rm").args(["-rf", &abs_task_dir]).status().await;
            format!("⚡任务完成 (Code {}):\n{}", code, log)
        }
        Outcome::Cancelled => {
            let _ = child.kill().await;
            let _ = Command::new("rm").args(["-rf", &abs_task_dir]).status().await;
            "🛑 任务已取消".to_string()
        }
        Outcome::Timeout => {
            // 转入后台，交由 operate_task 管理
            TASKS.lock().await.insert(
                task_id.clone(),
                TaskHandle {
                    child,
                    output: output.clone(),
                },
            );
            let log = output.lock().await.clone();
            let msg = format!("⏳任务超时转入后台，ID: {task_id}");
            notify(ctx, &msg).await;
            format!("{}\n当前输出:\n{}", msg, log)
        }
    };

    // 删除执行提示
    if let Some(id) = msg_id {
        let channel = ctx.channel.clone();
        let chat_id = ctx.chat_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let _ = channel.delete(&chat_id, &id).await;
        });
    }

    result
}
