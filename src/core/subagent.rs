//! 子代理：独立上下文的持久对话。
//!
//! 每个子代理对应一个文件 `messages/subagent/<uid>.<topicid>.<子代理id>.json`
//! （私聊没有话题时 topicid 记为 `0`）：
//!
//! ```json
//! { "memory": "", "usage": {}, "messages": [] }
//! ```
//!
//! 子代理使用与主代理相同的对话循环，但：不向用户发消息、工具表里没有
//! `subagent`（禁止嵌套）、同一 `id` 串行执行。
//!
//! 与主代理一样，上下文超过 `context_window` 时先把历史压缩进 `memory`：
//! 子代理不会触发 `/compact`，只能靠自动压缩控制体积。

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use once_cell::sync::Lazy;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::time::timeout as timeout_at;

use super::agent::{LoopResult, run_loop};
use super::compact;
use super::llm::{ChatRequest, build_base_prompt};
use crate::config::ModelConfig;
use crate::log;
use crate::tool::{MAX_OUTPUT_CHARS, ToolContext, registry, truncate};

const DIR: &str = "../messages/subagent";

fn file_key(id: &str) -> String {
    id.replace(['/', '\\'], "_")
}

/// 子代理文件路径：`<uid>.<topicid>.<子代理 id>.json`。
///
/// `chat` 为会话键（`uid` 或 `uid:topicid`），私聊补 `0` 作 topicid。
fn path(chat: &str, id: &str) -> String {
    let mut key = chat.replace(':', ".");
    if !key.contains('.') {
        key.push_str(".0");
    }
    format!("{DIR}/{}.{}.json", key, file_key(id))
}

fn empty_doc() -> Value {
    json!({ "memory": "", "usage": {}, "messages": [] })
}

/// 读取子代理文件；缺失返回空结构，损坏则备份后返回空结构。
fn load_doc(chat: &str, id: &str) -> Value {
    let file = path(chat, id);
    let mut doc = match qffunc::file_to_json(&file) {
        Ok(doc) if doc.is_object() => doc,
        _ => {
            // 文件存在却解析失败：先备份，避免被覆盖
            if fs::metadata(&file).is_ok() {
                let backup = format!("{file}.corrupt");
                if let Err(e) = fs::rename(&file, &backup) {
                    log::warn("subagent", format!("备份损坏的子代理文件失败：{e}"));
                }
            }
            empty_doc()
        }
    };
    if !doc["messages"].is_array() {
        doc["messages"] = json!([]);
    }
    if !doc["usage"].is_object() {
        doc["usage"] = json!({});
    }
    if !doc["memory"].is_string() {
        doc["memory"] = json!("");
    }
    doc
}

/// 原子写入子代理文件。
fn store_doc(chat: &str, id: &str, doc: &Value) -> Result<()> {
    fs::create_dir_all(DIR)?;
    let file = path(chat, id);
    let tmp = format!("{file}.tmp");
    serde_json::to_writer_pretty(fs::File::create(&tmp)?, doc)?;
    fs::rename(&tmp, file)?;
    Ok(())
}

fn load_messages(chat: &str, id: &str) -> Vec<Value> {
    load_doc(chat, id)["messages"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// 长期记忆（压缩后的历史摘要）。
fn memory(chat: &str, id: &str) -> String {
    load_doc(chat, id)["memory"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// 最近一次请求的总 token 数。
fn usage_total(chat: &str, id: &str) -> u64 {
    load_doc(chat, id)["usage"]["total_tokens"]
        .as_u64()
        .unwrap_or(0)
}

/// 保存历史（去掉首条 system），保留记忆与 usage。
fn save_messages(chat: &str, id: &str, messages: &[Value]) -> Result<()> {
    let mut doc = load_doc(chat, id);
    doc["messages"] = json!(messages.get(1..).unwrap_or_default());
    store_doc(chat, id, &doc)
}

fn record_usage(chat: &str, id: &str, usage: &Value) -> Result<()> {
    if !usage.is_object() {
        return Ok(());
    }
    let mut doc = load_doc(chat, id);
    doc["usage"] = usage.clone();
    store_doc(chat, id, &doc)
}

/// 压缩子代理上下文：摘要写入 `memory`，消息只留最后一条用户消息。
async fn compact_context(chat: &str, id: &str) -> Result<(usize, usize)> {
    let memory = memory(chat, id);
    let messages = load_messages(chat, id);
    let summary = compact::summarize(&memory, &messages).await?;
    let keep = compact::keep_last_user(&messages);

    let mut doc = load_doc(chat, id);
    doc["memory"] = json!(summary);
    doc["messages"] = json!(keep);
    store_doc(chat, id, &doc)?;

    Ok((messages.len(), keep.len()))
}

/// 超过上下文阈值时先压缩（同一个 id 已串行，不会并发压缩）。
async fn maybe_compact(chat: &str, id: &str) {
    let Ok(cfg) = ModelConfig::load() else {
        return;
    };
    if usage_total(chat, id) <= cfg.context_window {
        return;
    }
    match compact_context(chat, id).await {
        Ok((before, after)) => log::info(
            "subagent",
            format!("子代理 {id} 已压缩上下文：{before} 条消息 -> {after} 条"),
        ),
        Err(e) => log::warn("subagent", format!("子代理 {id} 自动压缩失败：{e}")),
    }
}

/// 最新一条助手消息。
fn latest(chat: &str, id: &str) -> Option<String> {
    load_messages(chat, id)
        .iter()
        .rev()
        .find(|m| m["role"] == "assistant")
        .and_then(|m| m["content"].as_str())
        .map(str::to_string)
        .filter(|text| !text.is_empty())
}

/// 汇总本轮已产生的助手消息（不含工具调用与工具输出）。
fn round_output(messages: &[Value]) -> String {
    messages
        .iter()
        .filter(|message| message["role"].as_str() == Some("assistant"))
        .filter_map(|message| message["content"].as_str())
        .filter(|content| !content.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 每个子代理一把锁，保证同一 `id` 串行执行。
static LOCKS: Lazy<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

async fn lock_for(chat: &str, id: &str) -> Arc<Mutex<()>> {
    let key = format!("{chat}|{id}");
    let mut locks = LOCKS.lock().await;
    locks
        .entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// 子代理的 system 提示：人设 + 子代理守则 + 历史记忆。
fn system_prompt(chat: &str, id: &str) -> String {
    let mut prompt = format!(
        "{}\n\n## 子代理\n你是子代理 {id}，独立完成交给你的任务。\n\
         - 你看不到主对话，信息不足时按最合理的假设处理，不要反问。\n\
         - 只输出结论与必要证据，不寒暄。",
        build_base_prompt()
    );
    let memory = memory(chat, id);
    if !memory.trim().is_empty() {
        prompt.push_str("\n\n## 历史记忆\n");
        prompt.push_str(memory.trim());
    }
    prompt
}

/// 执行一次子代理调用，返回给主代理的文本。
///
/// - `get_msg` 为 true：只返回该子代理最新一条消息，忽略 `content`
/// - 否则：把 `content` 作为用户消息追加，跑完整对话循环后返回本轮结论
pub async fn run(
    ctx: &ToolContext,
    id: &str,
    content: &str,
    get_msg: bool,
    timeout_secs: u64,
) -> String {
    let id = id.trim();
    if id.is_empty() {
        return "❌ id 不能为空".to_string();
    }

    let chat = ctx.chat_id.as_str();
    let lock = lock_for(chat, id).await;
    let _guard = lock.lock().await;

    if get_msg {
        return match latest(chat, id) {
            Some(text) => truncate(&text, MAX_OUTPUT_CHARS),
            None => "子代理暂无可返回的消息".to_string(),
        };
    }

    let content = content.trim();
    if content.is_empty() {
        return "❌ content 不能为空".to_string();
    }

    maybe_compact(chat, id).await;

    let history = load_messages(chat, id);
    let before = history.len();
    let mut messages = vec![json!({ "role": "system", "content": system_prompt(chat, id) })];
    messages.extend(history);
    messages.push(json!({ "role": "user", "content": content }));

    let mut request = match ChatRequest::from_messages(messages) {
        Ok(request) => request,
        Err(e) => return format!("❌ 子代理启动失败: {e}"),
    };
    request.set_tools(registry().definitions(false));

    let sub_ctx = ctx.subagent();
    let (save_chat, save_id) = (chat.to_string(), id.to_string());
    let on_save = move |messages: &[Value]| save_messages(&save_chat, &save_id, messages);
    let run = run_loop(&sub_ctx, request, None, &on_save);

    match timeout_at(Duration::from_secs(timeout_secs.max(1)), run).await {
        Ok(Ok(LoopResult { content, usage })) => {
            let _ = record_usage(chat, id, &usage);
            if content.is_empty() {
                "子代理没有返回内容".to_string()
            } else {
                truncate(&content, MAX_OUTPUT_CHARS)
            }
        }
        Ok(Err(e)) => format!("❌ 子代理执行失败: {e}"),
        Err(_) => {
            // 超时：中间结果已由 on_save 落盘，优先返回本轮已产生的输出
            let all = load_messages(chat, id);
            let this_round = all.get(before.min(all.len())..).unwrap_or_default();
            let mut output = round_output(this_round);
            if output.trim().is_empty() {
                output = latest(chat, id).unwrap_or_default();
            }
            let output = truncate(&output, 4000);
            if output.trim().is_empty() {
                format!("⏱ 子代理超时（{timeout_secs}s），尚无输出")
            } else {
                format!("⏱ 子代理超时（{timeout_secs}s）\n本轮输出:\n{output}")
            }
        }
    }
}
