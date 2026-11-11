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
use super::{MAX_OUTPUT_CHARS, Tool, ToolContext, truncate};
use crate::channel::{MsgKind, OutboundMessage};

/// 进程组守卫：工具被中断（子代理被 kill、任务 future 被丢弃）时，
/// 连同 `bash -c` 里的子进程一起结束，避免 `sleep` 之类变成孤儿进程。
struct GroupGuard(Option<u32>);

impl GroupGuard {
    /// 进程已被接管（正常结束或转入后台任务表）时解除守卫。
    fn release(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("kill -KILL -- -{pid} 2>/dev/null"))
                .status();
        }
    }
}

pub struct ExecTool;

/// 追加输出，只保留最新的 `MAX_OUTPUT_CHARS` 个字符。
async fn push_output(target: &Arc<Mutex<String>>, text: &str) {
    if text.is_empty() {
        return;
    }
    let mut lock = target.lock().await;
    lock.push_str(text);
    if lock.chars().count() > MAX_OUTPUT_CHARS {
        *lock = truncate(&lock, MAX_OUTPUT_CHARS);
    }
}

/// 增量 UTF-8 解码器。
///
/// 命令输出按 1024 字节分块读取，多字节字符（中文、emoji）可能正好被切在
/// 两块之间，直接对每块做 `from_utf8_lossy` 会得到 `�`。
/// 这里把没凑齐的尾巴留到下一块再解，只有真正非法的字节才替换成 `�`。
#[derive(Default)]
struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    /// 送入一块字节，返回其中已经完整、可以显示的部分。
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        self.decode(false)
    }

    /// 输入结束：把剩余字节按尽力而为的方式解码。
    fn finish(&mut self) -> String {
        self.decode(true)
    }

    fn decode(&mut self, finished: bool) -> String {
        let mut text = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(valid) => {
                    text.push_str(valid);
                    self.pending.clear();
                    return text;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if let Ok(prefix) = std::str::from_utf8(&self.pending[..valid]) {
                        text.push_str(prefix);
                    }
                    match e.error_len() {
                        // 明确的非法字节：替换后继续
                        Some(len) => {
                            text.push('�');
                            self.pending.drain(..valid + len);
                        }
                        // 只是数据不够：留着等下一块（输入结束时按坏字节处理）
                        None if !finished => {
                            self.pending.drain(..valid);
                            return text;
                        }
                        None => {
                            text.push('�');
                            self.pending.clear();
                            return text;
                        }
                    }
                }
            }
        }
    }
}

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

    // 执行提示（子代理上下文不发）：命令结束后这条消息会被结果原地替换，
    // 所以这里不设 TTL，由结果流程统一安排展示时长
    let start_msg = format!("⚡️执行：{}  ⌚️超时：{}", cmd_text, timeout_secs);
    if !ctx.silent
        && let Ok(id) = ctx
            .channel
            .send(
                OutboundMessage::text(ctx.chat_id.clone(), &start_msg)
                    .kind(MsgKind::Text)
                    .fold(),
            )
            .await
    {
        ctx.remember_notice(id, &start_msg).await;
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

    // 进程组 pid 与 bash 一致（process_group(0) 让它成为组长）
    let mut group = GroupGuard(child.id());

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // 异步捕获输出（stdout + stderr 合并），只保留最新部分
    let output = Arc::new(Mutex::new(String::new()));
    let output_writer = output.clone();
    let reader = tokio::spawn(async move {
        let mut reader = stdout.chain(stderr);
        let mut decoder = Utf8Decoder::default();
        let mut buf = [0u8; 1024];
        loop {
            let n = match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            push_output(&output_writer, &decoder.push(&buf[..n])).await;
        }
        push_output(&output_writer, &decoder.finish()).await;
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
            group.release();
            // 进程退出时管道里可能还有没读完的输出（stderr 尤其明显），
            // 等读取任务读到 EOF 再取结果；最多 2 秒，避免孙进程占着管道不放
            let _ = timeout(Duration::from_secs(2), reader).await;
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
            group.release();
            TASKS.lock().await.insert(
                task_id.clone(),
                TaskHandle {
                    child,
                    output: output.clone(),
                    pid,
                    dir: task_dir.clone(),
                },
            );
            let log = output.lock().await.clone();
            let msg = match outcome {
                Outcome::Timeout => format!("⏳任务超时转入后台，ID: {task_id}"),
                _ => format!("⏸ 用户中止，任务已转入后台，ID: {task_id}"),
            };
            format!("{}\n当前输出:\n{}", msg, log)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_decoder_handles_split_multibyte() {
        let text = "中文测试😀abc";
        let mut decoder = Utf8Decoder::default();
        let mut out = String::new();
        // 每个字节单独送一次，模拟最坏情况的分块边界
        for byte in text.as_bytes() {
            out.push_str(&decoder.push(&[*byte]));
        }
        out.push_str(&decoder.finish());
        assert_eq!(out, text);
    }

    #[test]
    fn utf8_decoder_replaces_invalid_bytes() {
        let mut decoder = Utf8Decoder::default();
        let mut out = decoder.push(&[0xff, b'a', 0xfe]);
        out.push_str(&decoder.finish());
        assert_eq!(out, "�a�");
    }
}
