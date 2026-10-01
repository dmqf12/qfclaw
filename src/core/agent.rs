//! Agent 对话循环。
//!
//! 请求模型 → 执行工具 → 回填结果，直到模型不再调用工具。

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::channel::*;
use crate::core::llm::ChatRequest;
use crate::core::session;
use crate::tool::{ToolContext, registry};

/// 从响应中取出：正文、推理、工具调用、总 token。
fn extract_chat_result(chat_result: &Value) -> (String, String, Value, u64) {
    let message = &chat_result["choices"][0]["message"];
    let content = message["content"].as_str().unwrap_or_default();
    let reasoning = message["reasoning_content"].as_str().unwrap_or_default();
    let tool_calls = message
        .get("tool_calls")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let total_tokens = chat_result["usage"]["total_tokens"].as_u64().unwrap_or(0);
    (
        content.to_string(),
        reasoning.to_string(),
        tool_calls,
        total_tokens,
    )
}

/// 并发执行一轮工具调用，返回按原始顺序排列的 tool 消息。
async fn run_tools(
    channel: &Arc<dyn Channel>,
    chat_id: &str,
    cancel: &CancellationToken,
    calls: &[Value],
) -> Vec<Value> {
    let mut set = JoinSet::new();
    let mut order = Vec::new();

    for call in calls {
        let name = call["function"]["name"].as_str().unwrap_or("").to_string();
        let call_id = call["id"].as_str().unwrap_or("").to_string();
        let args: Value =
            serde_json::from_str(call["function"]["arguments"].as_str().unwrap_or(""))
                .unwrap_or(json!({}));
        let ctx = ToolContext {
            channel: channel.clone(),
            chat_id: chat_id.to_string(),
            cancel: cancel.clone(),
        };
        order.push(call_id.clone());
        set.spawn(async move {
            let content = registry().call(&name, args, &ctx).await;
            (call_id, content)
        });
    }

    let mut collected: Vec<(String, String)> = Vec::new();
    while let Some(res) = set.join_next().await {
        match res {
            Ok(pair) => collected.push(pair),
            Err(e) => eprintln!("工具执行失败: {e}"),
        }
    }

    order
        .into_iter()
        .filter_map(|call_id| {
            collected
                .iter()
                .find(|(id, _)| *id == call_id)
                .map(|(_, content)| {
                    json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": content,
                    })
                })
        })
        .collect()
}

pub async fn main(
    channel: Arc<dyn Channel>,
    chat_id: String,
    user_input: &str,
    mut rx: mpsc::Receiver<String>,
    cancel: CancellationToken,
) -> Result<()> {
    let mut chat_request = ChatRequest::init(user_input)?;

    loop {
        let _ = channel
            .send(
                OutboundMessage::text(chat_id.clone(), "🧠思考中...")
                    .kind(MsgKind::Thinking)
                    .ttl(3),
            )
            .await;

        let reply: Value = match chat_request.send().await {
            Ok(resp) => resp,
            Err(e) => {
                let _ = channel
                    .send(OutboundMessage::text(chat_id.clone(), e.to_string()).kind(MsgKind::Error))
                    .await;
                break;
            }
        };

        let (content, reasoning, tool_calls, total_tokens) = extract_chat_result(&reply);
        session::record_tokens(total_tokens)?;

        if !content.is_empty() {
            let _ = channel
                .send(OutboundMessage::text(chat_id.clone(), content.clone()))
                .await;
        }

        let mut messages = chat_request.messages.clone();

        // 对话进行中收到新消息：打断当前回复，追加用户输入后重新请求
        if let Ok(new_msg) = rx.try_recv() {
            if let Some(obj) = messages.last_mut().and_then(|m| m.as_object_mut()) {
                obj.remove("tool_calls");
            }
            messages.push(json!({ "role": "user", "content": new_msg }));
            println!("打断❓");
            chat_request.messages = messages.clone();
            session::save(&messages)?;
            continue;
        }

        if let Some(calls) = tool_calls.as_array()
            && !calls.is_empty()
        {
            messages.push(json!({
                "role": "assistant",
                "content": content,
                "reasoning_content": reasoning,
                "tool_calls": tool_calls
            }));

            let tool_msgs = run_tools(&channel, &chat_id, &cancel, calls).await;
            messages.extend(tool_msgs);

            chat_request.messages = messages.clone();
            session::save(&messages)?;
        } else {
            messages.push(
                json!({ "role": "assistant", "content": content, "reasoning_content": reasoning }),
            );
            chat_request.messages = messages.clone();
            session::save(&messages)?;
            break;
        }
    }
    Ok(())
}
