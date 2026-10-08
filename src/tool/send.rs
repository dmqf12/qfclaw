use async_trait::async_trait;
use serde_json::{json, Value};
use crate::channel::*;

use super::{Tool, ToolContext, notify};

pub struct SendTool;

#[async_trait]
impl Tool for SendTool {
    fn name(&self) -> &str {
        "send_file"
    }

    fn definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "send_file",
                "description": "向用户发送视频，图片，音频文件或者文本消息, 图片不能超过10mb其他文件不能超过50mb",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "文件的绝对或相对路径" }
                    },
                    "required": ["file"]
                }
            }
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> String {
        let file = args["file"].as_str().unwrap_or("").to_string();
        notify(ctx, &format!("发送{}", file)).await;
        match ctx.channel.send(OutboundMessage::text(&ctx.chat_id, &file).kind(MsgKind::File)).await {
            Ok(_) => String::from(format!("{}发送成功", file)),
            Err(e) => String::from(format!("{}发送失败:{}", file, e)),
        }
    }
}
