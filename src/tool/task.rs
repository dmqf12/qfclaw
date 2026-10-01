//! 后台任务管理工具。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::process::Child;
use tokio::sync::Mutex;

use super::{MAX_OUTPUT_CHARS, Tool, ToolContext, notify, truncate};

/// 转入后台的任务句柄。
pub struct TaskHandle {
    pub child: Child,
    pub output: Arc<Mutex<String>>,
}

lazy_static::lazy_static! {
    /// 全部后台任务。
    pub static ref TASKS: Mutex<HashMap<String, TaskHandle>> = Mutex::new(HashMap::new());
}

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
                let _ = task.child.kill().await;
                tasks.remove(task_id);
                format!("🛑 终止任务：{task_id}")
            }
            "check" => {
                let log = task.output.lock().await;
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
