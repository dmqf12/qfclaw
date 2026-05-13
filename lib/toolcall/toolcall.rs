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
use sendmsg::*;

async fn read(file: &str) -> String {
    notify(&format!("🔧读取：{}", file), true).await;


    let path = if file.starts_with('/') {
        file.to_string()
    } else {
        format!("workspace/{}", file)
    };

    match fs::read_to_string(&path) {
        Ok(s) => s.trim().to_string(),
        Err(e) => format!("无法读取文件 {}: {}", file, e),
    }
}

async fn write(file: &str, text: &str) -> String {
    notify(&format!("✏️写入：{}", file), true).await;
    let path = if file.starts_with('/') || file.starts_with('$') {
        file.to_string()
    } else {
        format!("workspace/{}", file)
    };

    match fs::write(&path, text) {
        Ok(_) => format!("写入成功: {}", file),
        Err(e) => format!("写入失败 {}: {}", file, e),
    }
}


async fn delete(file: &str) -> String {
    notify(&format!("🗑️删除：{}", file), true).await;
    let path = if file.starts_with('/') {
        file.to_string()
    } else {
        format!("workspace/{}", file)
    };

    match fs::remove_file(&path) {
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

// --- 核心业务逻辑 ---

async fn exec(params: Value) -> String {
    let cmd_text = params["command"].as_str().unwrap_or("");
    let timeout_secs = params["timeout"].as_u64().unwrap_or(10);
    let task_id = Uuid::new_v4().to_string();
    notify(&format!("⚡️执行：{}  ⌚️超时：{}", cmd_text, timeout_secs), true).await;
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(format!("cd workspace && {}", cmd_text))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn");

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let output_buf = Arc::new(Mutex::new(String::new()));
    let output_clone = output_buf.clone();

    // 1. 实时读取输出
    tokio::spawn(async move {
        let mut reader = stdout.chain(stderr);
        let mut buffer = [0; 1024];
        while let Ok(n) = reader.read(&mut buffer).await {
            if n == 0 { break; }
            output_clone.lock().await.push_str(&String::from_utf8_lossy(&buffer[..n]));
        }
    });

    // 2. 等待策略
    match timeout(Duration::from_secs(timeout_secs), child.wait()).await {
        Ok(status) => {
            let log = output_buf.lock().await.clone();
            let code = status.map(|s| s.code().unwrap_or(0)).unwrap_or(-1);
            format!("✅ 任务完成 (Code {}):\n{}", code, log)
        }
        Err(_) => {
            TASKS.lock().await.insert(
                task_id.clone(),
                TaskHandle { child, output: output_buf.clone() }
            );
            let current_log = output_buf.lock().await.clone();
            let msg = format!("⏳ 任务超时转入后台，ID: {}", task_id);
            notify(&msg, true).await;
            format!("{}\n当前输出:\n{}", msg, current_log)
        }
    }
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
