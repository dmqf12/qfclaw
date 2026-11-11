//! 上下文压缩：把历史对话总结进 `memory`，清空消息列表。
//!
//! 触发后向模型发送一条「只做总结」的请求（不带系统提示词、不带工具），
//! 把结果写入历史文件的 `memory` 字段，`messages` 只保留最后一条用户消息。
//! 主代理（会话文件）与子代理（子代理文件）共用这里的摘要逻辑。

use anyhow::{Result, bail};
use serde_json::Value;

use super::llm::ChatRequest;
use super::session;
use crate::log;

/// 压缩提示词。
const SUMMARY_PROMPT: &str = "\
你是对话压缩器。把下面的「历史记忆」和「近期对话」合并成一段详细摘要，作为后续对话的长期记忆。

要求：
- 保留：用户的目标与约束、已确认的事实、进行中的任务与进度、关键文件路径、重要结论与待办事项
- 去掉：寒暄、重复内容、已被推翻的方案
- 用「用户」和「助手」指代双方，按时间顺序叙述
- 只输出摘要正文，不要任何前缀、标题或解释";

/// 一次压缩的结果。
pub struct Report {
    /// 压缩前的消息条数。
    pub before_messages: usize,
    /// 压缩前最近一次请求的 token 数。
    pub before_tokens: u64,
    /// 压缩后保留的消息条数。
    pub after_messages: usize,
    /// 记忆字符数。
    pub memory_chars: usize,
}

/// 是否需要压缩（基于上一次请求的 token 数）。
pub fn needed(chat: &str, context_window: u64) -> bool {
    session::status(chat).1 > context_window
}

/// 压缩指定会话，返回压缩前后对比。
pub async fn compact(chat: &str) -> Result<Report> {
    let memory = session::memory(chat);
    let messages = session::load_history(chat)?;
    let before_tokens = session::status(chat).1;
    let key = log::chat(chat);
    log::info(
        "compact",
        format!(
            "{key} 开始压缩 {}条 记忆{}字 用量{}",
            messages.len(),
            memory.chars().count(),
            before_tokens
        ),
    );

    let started = std::time::Instant::now();
    let summary = summarize(&memory, &messages).await?;
    let keep = keep_last_user(&messages);
    session::apply_compact(chat, &summary, &keep)?;

    log::info(
        "compact",
        format!(
            "{key} 压缩完成 {}条→{}条 记忆{}字 {:.1}s",
            messages.len(),
            keep.len(),
            summary.chars().count(),
            started.elapsed().as_secs_f64()
        ),
    );

    Ok(Report {
        before_messages: messages.len(),
        before_tokens,
        after_messages: keep.len(),
        memory_chars: summary.chars().count(),
    })
}

/// 把「已有记忆 + 消息列表」交给模型，得到合并后的摘要。
pub async fn summarize(memory: &str, messages: &[Value]) -> Result<String> {
    let source = build_source(memory, messages);
    if source.trim().is_empty() {
        bail!("没有可压缩的对话内容");
    }

    // 总结请求：不带系统提示词、不带工具
    let request = ChatRequest::summary(SUMMARY_PROMPT, &source)?;
    let reply = request.send().await?;
    let summary = reply["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    if summary.is_empty() {
        bail!("压缩失败：模型没有返回摘要");
    }
    Ok(summary)
}

/// 压缩后应当保留的消息：最后一条用户消息。
pub fn keep_last_user(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .rev()
        .find(|message| message["role"].as_str() == Some("user"))
        .cloned()
        .into_iter()
        .collect()
}

/// 拼接待压缩文本（历史记忆 + 近期对话）。
fn build_source(memory: &str, messages: &[Value]) -> String {
    let mut source = String::new();
    if !memory.trim().is_empty() {
        source.push_str("## 历史记忆\n");
        source.push_str(memory.trim());
        source.push_str("\n\n");
    }
    source.push_str("## 近期对话\n");
    for message in messages {
        let role = match message["role"].as_str() {
            Some("user") => "用户",
            Some("assistant") => "助手",
            Some("tool") => "工具",
            _ => continue,
        };
        let content = message["content"].as_str().unwrap_or_default();
        if !content.trim().is_empty() {
            source.push_str(&format!("{role}：{content}\n"));
        }
    }
    source
}
