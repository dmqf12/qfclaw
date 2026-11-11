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
