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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use once_cell::sync::Lazy;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::agent::{LoopResult, run_loop};
use super::compact;
use super::llm::{ChatRequest, build_base_prompt};
use crate::config::ModelConfig;
use crate::log;
use crate::tool::{ToolContext, registry};

const DIR: &str = "../messages/subagent";

/// `wait=true` 时前台最多等多久，超过就转后台继续跑。
const WAIT_TIMEOUT_SECS: u64 = 60;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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
    let mut doc = match crate::util::read_json_file(&file) {
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

/// 最新一条助手消息（供 `op_subagent` 拉取）。
pub fn latest(chat: &str, id: &str) -> Option<String> {
    load_messages(chat, id)
        .iter()
        .rev()
        .find(|m| m["role"] == "assistant")
        .and_then(|m| m["content"].as_str())
        .map(str::to_string)
        .filter(|text| !text.is_empty())
}

/// 正在运行的子代理：中止句柄 + 退出通知。
struct Running {
    abort: tokio::task::AbortHandle,
    done: Arc<tokio::sync::Notify>,
}

/// 正在运行的子代理（`chat|id` → 句柄），供 `op_subagent` 结束运行。
static RUNNING: Lazy<Mutex<HashMap<String, Running>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn running_key(chat: &str, id: &str) -> String {
    format!("{chat}|{}", id.trim())
}

/// 结束正在运行的子代理并等它真正退出，返回是否真的有在运行。
pub async fn kill(chat: &str, id: &str) -> bool {
    let Some(running) = RUNNING.lock().await.remove(&running_key(chat, id)) else {
        return false;
    };
    // 先取等待器再 abort，避免任务结束太快而错过通知
    let done = running.done.notified();
    running.abort.abort();
    // 兜底 5 秒：正常情况 abort 后毫秒级退出
    let _ = tokio::time::timeout(Duration::from_secs(5), done).await;
    true
}

/// 删除子代理的对话文件，返回文件是否存在过。
pub fn remove(chat: &str, id: &str) -> Result<bool> {
    match fs::remove_file(path(chat, id)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// 子代理状态：是否在运行、消息数、记忆字数、用量、最后活动时间。
pub async fn status(chat: &str, id: &str) -> String {
    let id = id.trim();
    let running = RUNNING.lock().await.contains_key(&running_key(chat, id));
    let file = path(chat, id);
    let exists = fs::metadata(&file).is_ok();
    if !exists && !running {
        return format!("🤖 子代理 {id}：无记录（未运行，也没有对话文件）");
    }
    let doc = load_doc(chat, id);
    let messages = doc["messages"].as_array().map_or(0, |list| list.len());
    let memory = doc["memory"].as_str().unwrap_or_default().chars().count();
    let usage = doc["usage"]["total_tokens"].as_u64().unwrap_or(0);
    let last = match fs::metadata(&file).and_then(|m| m.modified()) {
        Ok(time) => time
            .duration_since(UNIX_EPOCH)
            .map(|d| ago(now().saturating_sub(d.as_secs())))
            .unwrap_or_else(|_| "未知".to_string()),
        Err(_) => "无文件".to_string(),
    };
    format!(
        "🤖 子代理 {id}\n状态：{}\n消息：{messages} 条\n记忆：{memory} 字\n用量：{usage} tokens\n最后活动：{last}",
        if running {
            "🟢 运行中"
        } else {
            "⚪ 空闲"
        }
    )
}

/// 列出本会话的全部子代理及其状态。
pub async fn list(chat: &str) -> String {
    let mut prefix = chat.replace(':', ".");
    if !prefix.contains('.') {
        prefix.push_str(".0");
    }
    let prefix = format!("{prefix}.");
    let Ok(dir) = fs::read_dir(DIR) else {
        return "📋 本会话还没有子代理".to_string();
    };

    let running: Vec<String> = RUNNING.lock().await.keys().cloned().collect();
    let mut rows: Vec<(String, bool, usize, u64, String)> = Vec::new();
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(id) = name
            .strip_prefix(&prefix)
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        let id = id.to_string();
        let is_running = running.contains(&running_key(chat, &id));
        let doc = load_doc(chat, &id);
        let messages = doc["messages"].as_array().map_or(0, |l| l.len());
        let usage = doc["usage"]["total_tokens"].as_u64().unwrap_or(0);
        let last = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| ago(now().saturating_sub(d.as_secs())))
            .unwrap_or_else(|| "未知".to_string());
        rows.push((id, is_running, messages, usage, last));
    }
    if rows.is_empty() {
        return "📋 本会话还没有子代理".to_string();
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = format!("📋 本会话子代理（{} 个）", rows.len());
    for (id, running, messages, usage, last) in rows {
        out.push_str(&format!(
            "\n\n{}{id}\n  消息 {messages} 条 · 用量 {usage} tokens · 最后活动 {last}",
            if running { "🟢 " } else { "⚪ " }
        ));
    }
    out
}

/// 把「距今秒数」格式化成可读文本。
fn ago(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs} 秒前"),
        60..=3599 => format!("{} 分钟前", secs / 60),
        3600..=86399 => format!("{} 小时前", secs / 3600),
        _ => format!("{} 天前", secs / 86400),
    }
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
pub async fn run(ctx: &ToolContext, id: &str, content: &str, get_msg: bool, wait: bool) -> String {
    let id = id.trim();
    if id.is_empty() {
        return "❌ id 不能为空".to_string();
    }

    let chat = ctx.chat_id.as_str();

    // 取回最新消息只读文件，不参与串行锁，任何情况下都能立刻返回
    if get_msg {
        return match latest(chat, id) {
            Some(text) => text,
            None => "子代理暂无可返回的消息".to_string(),
        };
    }

    let content = content.trim();
    if content.is_empty() {
        return "❌ content 不能为空".to_string();
    }

    let token = ctx.cancel_token().await;

    // 等锁：同一 id 可能正在后台跑。等结果时最多等 WAIT_TIMEOUT_SECS；
    // 不等结果时拿不到锁就直接返回，不排队。
    let lock = lock_for(chat, id).await;
    let guard = if wait {
        tokio::select! {
            guard = lock.lock_owned() => guard,
            _ = tokio::time::sleep(Duration::from_secs(WAIT_TIMEOUT_SECS)) => {
                return not_waiting(id, wait);
            }
            _ = token.cancelled() => {
                return format!(
                    "🛑 已停止等待子代理 {id}，它仍在运行，稍后用 op_subagent 的 get_msg 取回结果；要继续该子代理，直接再用相同 id 调用 subagent"
                );
            }
        }
    } else {
        match lock.try_lock_owned() {
            Ok(guard) => guard,
            Err(_) => return not_waiting(id, wait),
        }
    };

    maybe_compact(chat, id).await;

    let history = load_messages(chat, id);
    let mut messages = vec![json!({ "role": "system", "content": system_prompt(chat, id) })];
    messages.extend(history);
    messages.push(json!({ "role": "user", "content": content }));

    let mut request = match ChatRequest::from_messages(messages) {
        Ok(request) => request,
        Err(e) => return format!("❌ 子代理启动失败: {e}"),
    };
    request.set_tools(registry().definitions(false));

    let sub_ctx = ctx.subagent();
    let (usage_chat, usage_id) = (chat.to_string(), id.to_string());
    let on_save = {
        let (save_chat, save_id) = (chat.to_string(), id.to_string());
        move |messages: &[Value]| save_messages(&save_chat, &save_id, messages)
    };

    // 子代理跑在独立任务里：前台只等 timeout 秒或取消信号，超时/`/stop`
    // 都不中断它，而是放手让它继续在后台跑（结果随时可用 `get_msg` 取回）。
    let key = running_key(chat, id);
    let done_key = key.clone();
    let done = Arc::new(tokio::sync::Notify::new());
    let task_done = done.clone();
    let mut handle = Box::pin(tokio::spawn(async move {
        let _guard = guard;
        let result = run_loop(&sub_ctx, request, None, &on_save).await;
        if let Ok(LoopResult { usage, .. }) = &result {
            let _ = record_usage(&usage_chat, &usage_id, usage);
        }
        RUNNING.lock().await.remove(&done_key);
        task_done.notify_one();
        result
    }));
    // 登记到运行表，`op_subagent kill` 可随时结束它
    RUNNING.lock().await.insert(
        key,
        Running {
            abort: handle.abort_handle(),
            done,
        },
    );

    // 不等结果：任务已在后台跑起来，立刻返回
    if !wait {
        drop(handle);
        log::info(
            "subagent",
            format!("{} 子代理 {id} 转后台运行（未等待结果）", log::chat(chat)),
        );
        return format!(
            "🔀 子代理 {id} 已在后台运行，稍后用 op_subagent 的 get_msg 取回结果；要继续该子代理，直接再用相同 id 调用 subagent"
        );
    }

    let mut cancelled = false;
    let joined = tokio::select! {
        joined = &mut handle => Some(joined),
        _ = tokio::time::sleep(Duration::from_secs(WAIT_TIMEOUT_SECS)) => None,
        _ = token.cancelled() => {
            cancelled = true;
            None
        }
    };

    if let Some(joined) = joined {
        return match joined {
            Ok(Ok(LoopResult { content, .. })) => {
                if content.is_empty() {
                    "子代理没有返回内容".to_string()
                } else {
                    content
                }
            }
            Ok(Err(e)) => format!("❌ 子代理执行失败: {e}"),
            Err(e) => format!("❌ 子代理任务异常: {e}"),
        };
    }

    // 转后台：drop JoinHandle 只是脱离，不会中止任务
    drop(handle);
    log::info(
        "subagent",
        format!(
            "{} 子代理 {id} {}，转入后台继续运行",
            log::chat(chat),
            if cancelled {
                "被 /stop 中止前台等待".to_string()
            } else {
                format!("前台等待超过 {WAIT_TIMEOUT_SECS}s")
            }
        ),
    );
    if cancelled {
        format!(
            "🛑 子代理 {id} 已转后台继续运行，稍后用 op_subagent 的 get_msg 取回结果；要继续该子代理，直接再用相同 id 调用 subagent"
        )
    } else {
        format!(
            "⏱ 子代理 {id} 前台等待超过 {WAIT_TIMEOUT_SECS}s，已转后台继续运行，稍后用 op_subagent 的 get_msg 取回结果；要继续该子代理，直接再用相同 id 调用 subagent"
        )
    }
}

/// 「没等到结果」的统一提示：区分没等（后台跑）与等超时。
fn not_waiting(id: &str, wait: bool) -> String {
    if wait {
        format!(
            "⏱ 子代理 {id} 正在运行，等待超过 {WAIT_TIMEOUT_SECS}s，稍后用 op_subagent 的 get_msg 取回结果；要继续该子代理，直接再用相同 id 调用 subagent"
        )
    } else {
        format!(
            "🔀 子代理 {id} 正在运行，本次未等待结果，稍后用 op_subagent 的 get_msg 取回结果；要继续该子代理，直接再用相同 id 调用 subagent"
        )
    }
}
