//! Agent 对话循环。
//!
//! 请求模型 → 执行工具 → 回填结果，直到模型不再调用工具。
//! 主代理与子代理共用本循环：区别在会话存储、可用工具与是否向用户发消息。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::channel::*;
use crate::config::ModelConfig;
use crate::core::compact;
use crate::core::llm::ChatRequest;
use crate::core::session;
use crate::log;
use crate::tool::{ToolContext, registry};

/// 保存历史（不含 system）的回调。
pub type SaveFn<'a> = &'a (dyn Fn(&[Value]) -> Result<()> + Send + Sync);

/// 一轮完整对话的结果。
pub struct LoopResult {
    /// 本轮所有助手正文（多条时按顺序用换行合并）。
    pub content: String,
    /// 最后一次请求的 usage。
    pub usage: Value,
}

/// 从响应中取出：正文、推理、工具调用。
fn extract_chat_result(chat_result: &Value) -> (String, String, Value) {
    let message = &chat_result["choices"][0]["message"];
    let content = message["content"].as_str().unwrap_or_default();
    let reasoning = message["reasoning_content"].as_str().unwrap_or_default();
    let tool_calls = message
        .get("tool_calls")
        .cloned()
        .unwrap_or_else(|| json!([]));
    (content.to_string(), reasoning.to_string(), tool_calls)
}

/// usage 摘要（日志用）：`prompt / completion / total tokens`。
fn usage_brief(usage: &Value) -> String {
    let field = |key: &str| usage[key].as_u64().unwrap_or(0);
    format!(
        "tokens {}/{}/{}",
        field("prompt_tokens"),
        field("completion_tokens"),
        field("total_tokens")
    )
}

/// 并发执行一轮工具调用，返回按原始顺序排列的 tool 消息。
async fn run_tools(ctx: &ToolContext, calls: &[Value]) -> Vec<Value> {
    let mut set = JoinSet::new();
    let mut order = Vec::new();

    for call in calls {
        let name = call["function"]["name"].as_str().unwrap_or("").to_string();
        let call_id = call["id"].as_str().unwrap_or("").to_string();
        let args: Value =
            serde_json::from_str(call["function"]["arguments"].as_str().unwrap_or(""))
                .unwrap_or(json!({}));
        let call_ctx = ctx.child();
        order.push(call_id.clone());
        set.spawn(async move {
            let content = registry().call(&name, args, &call_ctx).await;
            (call_id, content)
        });
    }

    let mut collected: HashMap<String, String> = HashMap::new();
    while let Some(res) = set.join_next().await {
        match res {
            Ok((call_id, content)) => collected.insert(call_id, content),
            Err(e) => {
                log::error("tool", format!("工具任务崩溃：{e}"));
                None
            }
        };
    }

    order
        .into_iter()
        .filter_map(|call_id| {
            collected.get(&call_id).map(|content| {
                json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": content,
                })
            })
        })
        .collect()
}

/// 把多模态消息折叠成文本占位，避免后续请求重复计费。
fn fold_images(messages: &mut [Value]) {
    for message in messages.iter_mut() {
        if !message["content"].is_array() {
            continue;
        }
        let mut texts: Vec<String> = Vec::new();
        let mut images = 0usize;
        if let Some(parts) = message["content"].as_array() {
            for part in parts {
                match part["type"].as_str() {
                    Some("text") => {
                        if let Some(text) = part["text"].as_str() {
                            texts.push(text.to_string());
                        }
                    }
                    Some("file") | Some("image_url") => images += 1,
                    _ => {}
                }
            }
        }
        if images > 0 {
            texts.push(format!("[图片 x{images}]"));
        }
        message["content"] = json!(texts.join(" "));
    }
}

/// 跑完一轮完整对话。
///
/// - `rx`：主代理的打断通道，子代理传 `None`
/// - 是否向用户发消息由 `ctx.silent` 决定（子代理为 true）
/// - `on_save`：每轮结束后保存历史（不含 system）
pub async fn run_loop(
    ctx: &ToolContext,
    mut request: ChatRequest,
    mut rx: Option<&mut mpsc::Receiver<String>>,
    on_save: SaveFn<'_>,
) -> Result<LoopResult> {
    let mut contents: Vec<String> = Vec::new();
    let mut last_usage = json!({});
    let mut round: usize = 0;

    loop {
        round += 1;
        if !ctx.silent {
            ctx.channel
                .send(
                    OutboundMessage::text(ctx.chat_id.clone(), "🧠思考中...")
                        .kind(MsgKind::Text)
                        .ttl(3),
                )
                .await?;
        }

        let tag = if ctx.silent { "subagent" } else { "agent" };
        log::info(
            tag,
            format!(
                "{} 第 {round} 轮请求：{} 条消息",
                ctx.chat_id,
                request.messages.len()
            ),
        );
        let started = Instant::now();
        let reply: Value = match request.send().await {
            Ok(resp) => resp,
            Err(e) => {
                log::error(
                    tag,
                    format!(
                        "{} 第 {round} 轮请求失败（耗时 {:.1}s）：{e}",
                        ctx.chat_id,
                        started.elapsed().as_secs_f64()
                    ),
                );
                if !ctx.silent {
                    let _ = ctx
                        .channel
                        .send(
                            OutboundMessage::text(ctx.chat_id.clone(), &e.to_string())
                                .kind(MsgKind::Error),
                        )
                        .await;
                }
                return Err(e);
            }
        };

        let (content, reasoning, tool_calls) = extract_chat_result(&reply);
        last_usage = reply["usage"].clone();
        log::info(
            tag,
            format!(
                "{} 第 {round} 轮响应：耗时 {:.1}s，{}，正文 {} 字，推理 {} 字，工具调用 {} 个",
                ctx.chat_id,
                started.elapsed().as_secs_f64(),
                usage_brief(&last_usage),
                content.chars().count(),
                reasoning.chars().count(),
                tool_calls.as_array().map(Vec::len).unwrap_or(0)
            ),
        );

        if !content.is_empty() {
            contents.push(content.clone());
            if !ctx.silent {
                ctx.channel
                    .send(OutboundMessage::text(ctx.chat_id.clone(), &content))
                    .await?;
            }
        }

        let mut messages = request.messages.clone();
        // 本轮请求已带上图片，这里折叠成占位，避免后续轮次重复计费
        fold_images(&mut messages);

        // 对话进行中收到新消息：打断当前回复，追加用户输入后重新请求
        if let Some(rx) = rx.as_deref_mut()
            && let Ok(new_msg) = rx.try_recv()
        {
            if let Some(obj) = messages.last_mut().and_then(|m| m.as_object_mut()) {
                obj.remove("tool_calls");
            }
            messages.push(json!({ "role": "user", "content": new_msg }));
            log::info(
                "agent",
                format!(
                    "{} 打断上一轮，追加新消息：{}",
                    ctx.chat_id,
                    log::preview(&new_msg, 80)
                ),
            );
            request.messages = messages.clone();
            on_save(&messages)?;
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

            let tool_msgs = run_tools(ctx, calls).await;
            messages.extend(tool_msgs);

            // 把本轮查看的图片注入上下文，模型下一轮即可看到
            let images = ctx.take_images().await;
            if !images.is_empty() {
                let mut parts = vec![json!({
                    "type": "text",
                    "text": format!("（已查看 {} 张图片）", images.len())
                })];
                for file_id in &images {
                    parts.push(json!({ "type": "file", "file_id": file_id }));
                }
                messages.push(json!({ "role": "user", "content": parts }));
            }

            request.messages = messages.clone();
            on_save(&messages)?;
        } else {
            messages.push(
                json!({ "role": "assistant", "content": content, "reasoning_content": reasoning }),
            );
            request.messages = messages.clone();
            on_save(&messages)?;
            break;
        }
    }

    log::info(
        "agent",
        format!(
            "{} 对话完成：{round} 轮，正文 {} 字，{}",
            ctx.chat_id,
            contents.iter().map(|c| c.chars().count()).sum::<usize>(),
            usage_brief(&last_usage)
        ),
    );
    Ok(LoopResult {
        content: contents.join("\n"),
        usage: last_usage,
    })
}

/// 主代理入口：从会话历史构建请求，结果写回会话文件。
pub async fn main(
    channel: Arc<dyn Channel>,
    chat_id: String,
    user_input: &str,
    mut rx: mpsc::Receiver<String>,
    cancel: Arc<Mutex<CancellationToken>>,
    active: Arc<AtomicUsize>,
) -> Result<()> {
    // 上下文接近窗口上限时先压缩，再处理新消息
    if let Ok(cfg) = ModelConfig::load()
        && compact::needed(&chat_id, cfg.context_window)
    {
        log::info(
            "compact",
            format!(
                "{chat_id} 上次用量 {} tokens 超过阈值 {}，自动压缩",
                session::status(&chat_id).1,
                cfg.context_window
            ),
        );
        if let Err(e) = compact::compact(&chat_id).await {
            log::error("compact", format!("{chat_id} 自动压缩失败：{e}"));
        }
    }

    let request = ChatRequest::init(&chat_id, user_input)?;
    let ctx = ToolContext::new(channel, chat_id.clone(), cancel, active);

    let save_chat = chat_id.clone();
    let on_save = move |messages: &[Value]| session::save(&save_chat, messages);

    let result = run_loop(&ctx, request, Some(&mut rx), &on_save).await?;
    session::record_usage(&chat_id, &result.usage)?;
    Ok(())
}
