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
                "description": "把子任务交给子代理并取回结论。同一个 id 可以反复调用：每次调用都是给这个子代理追加一条消息，它会带着已有上下文继续，相当于持续对话。要继续、追问、补充信息、让它做下一步，直接再用相同 id 调用即可，不要 kill 也不要换新 id。适合调研、批量处理、长链路操作；不要用它做需要和用户确认的事。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "要发送给子代理的消息（必须自包含，子代理看不到当前对话）"
                        },
                        "id": {
                            "type": "string",
                            "description": "子代理 id。相同 id 复用同一份上下文，重复调用即为追加消息"
                        },
                        "get_msg": {
                            "type": "boolean",
                            "description": "只读取该子代理最新一条消息，忽略 content；不会让它继续工作。要继续工作请正常调用并传 content"
                        },
                        "wait": {
                            "type": "boolean",
                            "description": "是否在前台等待子代理返回结果，默认 true。为 false 时立即转到后台运行、马上返回，之后用 op_subagent 的 get_msg 取回结果"
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
        let wait = args["wait"].as_bool().unwrap_or(true);

        if get_msg {
            notify(ctx, &format!("🤖 取回子代理 {id} 的最新消息")).await;
        } else {
            notify(
                ctx,
                &format!(
                    "🤖 子代理 {id}  {}\n{content}",
                    if wait {
                        "⌚️等待结果"
                    } else {
                        "🔀后台运行"
                    }
                ),
            )
            .await;
        }

        subagent::run(ctx, id, content, get_msg, wait).await
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
                "description": "查看与管理子代理，不用于和子代理对话。list 列出本会话全部子代理及运行状态；status 查看某个子代理的详细状态（是否运行中、消息数、记忆字数、用量、最后活动时间）；get_msg 拉取最新一条消息；kill 结束正在运行的任务（对话上下文保留，之后仍可用相同 id 继续）；clear 删除对话文件（先结束运行）。只是想追加消息或让子代理继续时，不要用本工具，直接调用 subagent 并复用相同 id。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "子代理 id；list 之外的操作必填"
                        },
                        "action": {
                            "type": "string",
                            "enum": ["list", "status", "get_msg", "kill", "clear"],
                            "description": "操作类型：list 列出全部子代理；status 查看单个子代理状态；get_msg 拉取最新消息；kill 结束运行（保留对话文件）；clear 删除对话文件"
                        }
                    },
                    "required": ["action"]
                }
            }
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> String {
        let id = args["id"].as_str().unwrap_or_default().trim().to_string();
        let action = args["action"].as_str().unwrap_or("status");
        let chat = ctx.chat_id.as_str();

        // list 面向整个会话，不需要 id
        if action == "list" {
            notify(ctx, "📋 列出子代理").await;
            return subagent::list(chat).await;
        }

        if id.is_empty() {
            return "❌ id 不能为空".to_string();
        }

        match action {
            "status" => subagent::status(chat, &id).await,
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
                // kill 内部会等子代理真正退出，之后删文件才不会被子代理回写
                let killed = subagent::kill(chat, &id).await;
                match subagent::remove(chat, &id) {
                    Ok(true) => format!(
                        "🧹 已删除子代理 {id} 的对话文件{}",
                        if killed {
                            "（并结束了运行）"
                        } else {
                            ""
                        }
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
