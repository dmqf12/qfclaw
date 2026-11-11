//! 子代理工具：把自包含的子任务交给独立上下文的子代理。

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolContext, notify};
use crate::core::subagent;

pub struct SubagentTool;

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        "subagent"
    }

    fn definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "subagent",
                "description": "把一个自包含的子任务交给子代理独立完成，返回其结论。适合调研、批量处理、长链路操作；不要用它做需要和用户确认的事。相同 id 复用同一份上下文。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "要发送给子代理的消息（必须自包含，子代理看不到当前对话）"
                        },
                        "id": {
                            "type": "string",
                            "description": "子代理 id，相同 id 会复用同一份上下文"
                        },
                        "get_msg": {
                            "type": "boolean",
                            "description": "为 true 时只返回该子代理最新的一条消息，忽略 content"
                        },
                        "timeout": {
                            "type": "integer",
                            "description": "前台等待秒数，默认 60。超时或用户 /stop 都不会中断子代理，它会转后台继续运行，之后用 get_msg 取回结果"
                        }
                    },
                    "required": ["id"]
                }
            }
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> String {
        let id = args["id"].as_str().unwrap_or_default();
        let content = args["content"].as_str().unwrap_or_default();
        let get_msg = args["get_msg"].as_bool().unwrap_or(false);
        let timeout = args["timeout"].as_u64().unwrap_or(60);

        if get_msg {
            notify(ctx, &format!("🤖 取回子代理 {id} 的最新消息")).await;
        } else {
            notify(ctx, &format!("🤖 子代理 {id}：{content}")).await;
        }

        subagent::run(ctx, id, content, get_msg, timeout).await
    }
}

/// 子代理管理：结束运行、删除对话文件、拉取最新消息。
pub struct OpSubagentTool;

#[async_trait]
impl Tool for OpSubagentTool {
    fn name(&self) -> &str {
        "op_subagent"
    }

    fn definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "op_subagent",
                "description": "管理子代理：kill 结束正在运行的子代理；clear 删除该子代理的对话文件（先结束运行）；get_msg 拉取该子代理最新一条消息。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "子代理 id"
                        },
                        "action": {
                            "type": "string",
                            "enum": ["get_msg", "kill", "clear"],
                            "description": "操作类型：get_msg 拉取最新消息；kill 结束运行（保留对话文件）；clear 删除对话文件"
                        }
                    },
                    "required": ["id", "action"]
                }
            }
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> String {
        let id = args["id"].as_str().unwrap_or_default().trim().to_string();
        let action = args["action"].as_str().unwrap_or("get_msg");
        if id.is_empty() {
            return "❌ id 不能为空".to_string();
        }
        let chat = ctx.chat_id.as_str();

        match action {
            "kill" => {
                notify(ctx, &format!("🛑 结束子代理 {id}")).await;
                if subagent::kill(chat, &id).await {
                    format!("🛑 已结束子代理 {id} 的运行（对话文件保留）")
                } else {
                    format!("⚠️ 子代理 {id} 当前没有在运行")
                }
            }
            "clear" => {
                notify(ctx, &format!("🧹 删除子代理 {id} 的对话文件")).await;
                let killed = subagent::kill(chat, &id).await;
                // abort 是异步的，稍等一拍再删文件，避免后台任务回写
                if killed {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                match subagent::remove(chat, &id) {
                    Ok(true) => format!(
                        "🧹 已删除子代理 {id} 的对话文件{}",
                        if killed { "（并结束了运行）" } else { "" }
                    ),
                    Ok(false) => format!("⚠️ 子代理 {id} 没有对话文件"),
                    Err(e) => format!("❌ 删除失败: {e}"),
                }
            }
            "get_msg" => match subagent::latest(chat, &id) {
                Some(text) => text,
                None => format!("子代理 {id} 暂无可返回的消息"),
            },
            other => format!("❌ 未知操作: {other}"),
        }
    }
}
