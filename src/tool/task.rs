//! 后台任务管理工具。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use serde_json::{Value, json};
use tokio::process::Child;
use tokio::sync::Mutex;

use super::{MAX_OUTPUT_CHARS, Tool, ToolContext, notify, truncate};

/// 转入后台的任务句柄。
pub struct TaskHandle {
    pub child: Child,
    pub output: Arc<Mutex<String>>,
    /// 进程组 id，终止时连同子进程一起结束。
    pub pid: Option<u32>,
    /// 任务目录（`qfclawtask/<id>`），任务结束后一并删除。
    pub dir: std::path::PathBuf,
}

/// 全部后台任务。
pub static TASKS: Lazy<Mutex<HashMap<String, TaskHandle>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub struct TaskTool;

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "operate_task"
    }

    fn definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "operate_task",
                "description": "管理或查看已启动任务的状态。可以获取实时标准输出/错误日志，或者强制终止正在运行的任务。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string", "description": "exec 函数返回的任务唯一标识符" },
                        "order": { "type": "string", "enum": ["check", "kill"], "description": "操作类型：check 获取输出；kill 终止任务" }
                    },
                    "required": ["task_id", "order"]
                }
            }
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> String {
        let task_id = args["task_id"].as_str().unwrap_or("");
        let order = args["order"].as_str().unwrap_or("check");
        notify(ctx, &format!("👌🏻查询：{task_id}")).await;

        let mut tasks = TASKS.lock().await;
        let task = match tasks.get_mut(task_id) {
            Some(t) => t,
            None => return "❌ 错误：未找到任务".to_string(),
        };

        match order {
            "kill" => {
                // 连同进程组一起结束，避免 bash 下的命令残留
                if let Some(pid) = task.pid {
                    let _ = std::process::Command::new("sh")
                        .arg("-c")
                        .arg(format!("kill -KILL -- -{pid} 2>/dev/null"))
                        .status();
                }
                let _ = task.child.kill().await;
                let dir = task.dir.clone();
                tasks.remove(task_id);
                let _ = std::fs::remove_dir_all(dir);
                format!("🛑 终止任务：{task_id}")
            }
            "check" => {
                let log = task.output.lock().await.clone();
                // 任务已退出：回传结束码并从表中移除，避免任务表无限增长
                if let Ok(Some(status)) = task.child.try_wait() {
                    let code = status.code().unwrap_or(-1);
                    let dir = task.dir.clone();
                    tasks.remove(task_id);
                    let _ = std::fs::remove_dir_all(dir);
                    return format!(
                        "✅ 任务已结束 (Code {code}):\n{}",
                        truncate(&log, MAX_OUTPUT_CHARS)
                    );
                }
                if log.is_empty() {
                    "等待输出...".to_string()
                } else {
                    truncate(&log, MAX_OUTPUT_CHARS)
                }
            }
            _ => "未知指令".to_string(),
        }
    }
}
