use std::fs;
use tokio::process::{Command, Child};
use tokio::sync::Mutex;
use tokio::io::AsyncReadExt;
use tokio::time::{timeout, Duration};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use serde_json::{json, Value};
use uuid::Uuid;
use tokio::sync::{oneshot};
use crate::sendmsg::*;

async fn read(file: &str) -> String {
    notify(&format!("🔧读取：{}", file), true).await;
    match fs::read_to_string(file) {
        Ok(s) => s.trim().to_string(),
        Err(e) => format!("无法读取文件 {}: {}", file, e),
    }
}

async fn write(file: &str, text: &str) -> String {
    notify(&format!("✏️写入：{}", file), true).await;
    match fs::write(file, text) {
        Ok(_) => format!("写入成功: {}", file),
        Err(e) => format!("写入失败 {}: {}", file, e),
    }
}


async fn delete(file: &str) -> String {
    notify(&format!("🗑️删除：{}", file), true).await;
    match fs::remove_file(file) {
        Ok(_) => format!("✅删除成功: {}", file),
        Err(e) => format!("❌删除失败 {}: {}", file, e),
    }
}



async fn operate_file(parame: &Value) -> String {
    let file = parame["file"].as_str().unwrap_or("");
    let operation = parame["operation"].as_str().unwrap_or("");
    if operation == "write" {
        let text = parame["content"].as_str().unwrap_or("");
        return write(file, text).await
    }
    if operation == "delete" {
        return delete(file).await
    }
    if operation == "read" {
        return read(file).await
    }
    String::new()
}


pub struct ToolRequest {
    pub payload: Value,
    pub resp_tx: oneshot::Sender<Value>,
}

struct TaskHandle {
    child: Child,
    output: Arc<Mutex<String>>,
}

lazy_static::lazy_static! {
    static ref TASKS: Arc<Mutex<HashMap<String, TaskHandle>>> = Arc::new(Mutex::new(HashMap::new()));
}


async fn notify(msg: &str, clear: bool) {
    println!("{}", msg);
    if let Ok((msg_id, _)) = SendMessage::new(msg).fold().send().await {
        if clear {
            clear_up(msg_id, msg_id, 5);
        }
    }
}




async fn exec(params: Value) -> String {
    let cmd_text = params["command"].as_str().unwrap_or("");
    let timeout_secs = params["timeout"].as_u64().unwrap_or(10);
    let task_id = Uuid::new_v4().to_string();
    let task_dir = format!("qfclawtask/{}", task_id);

    // 1. 发送初始消息
    let start_msg = format!("⚡️执行：{}  ⌚️超时：{}", cmd_text, timeout_secs);
    let (msg_id, _) = SendMessage::new(&start_msg).fold().send().await.unwrap_or_default();

    // 2. 环境准备 (改为等待 status 确保完成，而不是 sleep)
    let _ = Command::new("mkdir").args(["-p", &task_dir]).status().await;

    // 写入脚本 (注意：sudo 增加绝对路径 /usr/bin/sudo 避免函数递归)
    let qfsudo_content = format!(
        "tmpfile=$(mktemp {}/XXXXXXXX) && printf \"%s\\n\" \"$*\" > \"$tmpfile\" && chmod +x \"$tmpfile\" && /usr/bin/sudo \"$tmpfile\"",
        task_dir
    );
    let exec_content = format!("source workspace/pyvenv/bin/activate\ncat() {{ file \"$1\" | grep -q text && /bin/cat \"$1\" || echo \"跳过二进制文件: $1\"; }}\nsudo() {{ {}/qfsudo \"$@\"; }}\n{}", task_dir, cmd_text);

    _ = fs::write(format!("{}/qfsudo", task_dir), qfsudo_content);
    _ = fs::write(format!("{}/exec.sh", task_dir), exec_content);

    // 立即执行 chmod 并等待完成
    let _ = Command::new("chmod")
        .args(["+x", &format!("{}/exec.sh", task_dir), &format!("{}/qfsudo", task_dir)])
        .status()
        .await;

    // 3. 启动主进程
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(format!("{}/exec.sh", task_dir)) // 删掉了 && #rm，清理逻辑放在 Rust 里更安全
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn");

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let output_buf = Arc::new(Mutex::new(String::new()));
    let output_clone = output_buf.clone();

    // 4. 异步捕获输出 (stdout 和 stderr)
    tokio::spawn(async move {
        let mut combined_reader = stdout.chain(stderr); // 注意：需引入 AsyncReadExt
        let mut buffer = [0; 1024];
        while let Ok(n) = combined_reader.read(&mut buffer).await {
            if n == 0 { break; }
            let mut lock = output_clone.lock().await;
            lock.push_str(&String::from_utf8_lossy(&buffer[..n]));
        }
    });

    // 5. 执行等待逻辑
    let res_text = match timeout(Duration::from_secs(timeout_secs), child.wait()).await {
        Ok(wait_result) => {
            let log = output_buf.lock().await.clone();
            let code = wait_result.map(|s| s.code().unwrap_or(0)).unwrap_or(-1);
            // 任务成功结束，清理临时目录
            let _ = Command::new("rm").args(["-rf", &task_dir]).status().await;
            format!("⚡任务完成 (Code {}):\n{}", code, log)
        }
        Err(_) => {
            // 超时处理：将 child 句柄存入全局 Task 字典（假设 TASKS 已定义）
            TASKS.lock().await.insert(
                task_id.clone(),
                TaskHandle { child, output: output_buf.clone() }
            );
            let current_log = output_buf.lock().await.clone();
            let msg = format!("⏳任务超时转入后台，ID: {}", task_id);
            notify(&msg, true).await;
            format!("{}\n当前输出:\n{}", msg, current_log)
        }
    };

    clear_up(msg_id, msg_id, 3);
    res_text
}


async fn operate_task(params: Value) -> String {
    let task_id = params["task_id"].as_str().unwrap_or("");
    let order = params["order"].as_str().unwrap_or("check");
    notify(&format!("👌🏻查询：{}", task_id), true).await;
    let mut tasks = TASKS.lock().await;
    let task = match tasks.get_mut(task_id) {
        Some(t) => t,
        None => return "❌ 错误：未找到任务".to_string(),
    };

    match order {
        "kill" => {
            let _ = task.child.kill().await;
            tasks.remove(task_id);
            let msg = format!("🛑 终止任务：{}", task_id);
            notify(&msg, true).await;
            msg
        }
        "check" => {
            let log = task.output.lock().await;
            if log.is_empty() { "等待输出...".to_string() } else { log.clone() }
        }
        _ => "未知指令".to_string(),
    }
}

// --- 接口分发 ---

pub async fn toolcall(mut rx: tokio::sync::mpsc::Receiver<ToolRequest>) {
    while let Some(req) = rx.recv().await {
        let mut results = Vec::new();
        if let Some(array) = req.payload.as_array() {
            for element in array {
                let name = element["function"]["name"].as_str().unwrap_or_default();
                let call_id = element["id"].as_str().unwrap_or_default();
                let args: Value = serde_json::from_str(
                    element["function"]["arguments"].as_str().unwrap_or_default()
                ).unwrap_or(json!({}));

                let run_result = match name {
                    "exec" => exec(args).await,
                    "operate_task" => operate_task(args).await,
                    _ => operate_file(&args).await,
                };

                results.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": run_result
                }));
            }
        }
        let _ = req.resp_tx.send(json!(results));
    }
}
