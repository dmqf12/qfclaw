//! 工具层。
//!
//! 每个工具实现 [`Tool`]，由 [`ToolRegistry`] 统一注册与分发。
//! 工具通过 [`ToolContext`] 访问消息通道、会话与取消信号。

pub mod exec;
pub mod file;
pub mod send;
pub mod subagent;
pub mod task;
pub mod view_image;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::channel::{Channel, MsgId, MsgKind, OutboundMessage};
use crate::log;

/// 单次工具输出的字符上限。
pub const MAX_OUTPUT_CHARS: usize = 8_000;

/// 折叠提示消息的显示时长（秒）。
pub const NOTIFY_TTL_SECS: u64 = 10;

/// 短时效工具（看图、发文件）结果的显示时长（秒），比默认更短。
const SHORT_TTL_SECS: u64 = 3;

/// 按工具名取结果显示时长。
fn notify_ttl(name: &str) -> u64 {
    match name {
        "view_image" | "send_file" => SHORT_TTL_SECS,
        _ => NOTIFY_TTL_SECS,
    }
}

/// 回显给用户的工具结果最大字符数（给模型的完整结果不受此限制）。
const NOTIFY_RESULT_CHARS: usize = 2_000;

/// 「提示 + 结果」合并后的字符上限。
///
/// 折叠消息要按 MarkdownV2 转义（内容会膨胀），而 Telegram 单条上限是
/// 4096 个 UTF-16 码元，这里留出余量，避免编辑/发送失败。
const NOTIFY_MERGED_CHARS: usize = 3_000;

/// 工具执行上下文。
pub struct ToolContext {
    pub channel: Arc<dyn Channel>,
    pub chat_id: String,
    /// 中止当前工具调用用的令牌（`/stop` 时会整体换成新令牌）。
    cancel: Arc<Mutex<CancellationToken>>,
    /// 子代理上下文：不向用户发送任何提示。
    pub silent: bool,
    /// 嵌套层级：0 为主代理，大于 0 为子代理（禁止再次嵌套）。
    pub depth: u8,
    /// 本轮待注入上下文的图片 file_id（由 `view_image` 填充）。
    images: Arc<Mutex<Vec<String>>>,
    /// 正在执行的工具数量（用于判断 /stop 是否有东西可停）。
    active: Arc<AtomicUsize>,
    /// 本次工具的提示消息：`(消息 ID, 原文)`。
    /// 工具结束时把结果追加到这条消息里（调用内容和结果都能看到）。
    /// 每个工具走各自的 [`ToolContext::child`]，并发调用互不干扰。
    notice: Arc<Mutex<Option<(MsgId, String)>>>,
}

impl ToolContext {
    pub fn new(
        channel: Arc<dyn Channel>,
        chat_id: String,
        cancel: Arc<Mutex<CancellationToken>>,
        active: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            channel,
            chat_id,
            cancel,
            silent: false,
            depth: 0,
            images: Arc::new(Mutex::new(Vec::new())),
            active,
            notice: Arc::new(Mutex::new(None)),
        }
    }

    /// 取当前代际的取消令牌。
    pub async fn cancel_token(&self) -> CancellationToken {
        self.cancel.lock().await.clone()
    }

    /// 同层级的新上下文（并发执行工具时用），共享待注入图片列表。
    pub fn child(&self) -> Self {
        Self {
            channel: self.channel.clone(),
            chat_id: self.chat_id.clone(),
            cancel: self.cancel.clone(),
            silent: self.silent,
            depth: self.depth,
            images: self.images.clone(),
            active: self.active.clone(),
            notice: Arc::new(Mutex::new(None)),
        }
    }

    /// 交给子代理的上下文：静默，层级 +1。
    pub fn subagent(&self) -> Self {
        Self {
            channel: self.channel.clone(),
            chat_id: self.chat_id.clone(),
            cancel: self.cancel.clone(),
            silent: true,
            depth: self.depth + 1,
            images: Arc::new(Mutex::new(Vec::new())),
            active: self.active.clone(),
            notice: Arc::new(Mutex::new(None)),
        }
    }

    /// 标记一个工具开始执行。
    fn enter_tool(&self) {
        self.active.fetch_add(1, Ordering::SeqCst);
    }

    /// 标记一个工具执行结束。
    fn exit_tool(&self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }

    /// 记住本次工具的提示消息，结束时会把结果追加进去。
    async fn remember_notice(&self, id: MsgId, text: &str) {
        *self.notice.lock().await = Some((id, text.to_string()));
    }

    /// 本次工具的提示消息 `(ID, 原文)`（还没有则为 `None`）。
    async fn last_notice(&self) -> Option<(MsgId, String)> {
        self.notice.lock().await.clone()
    }

    /// 记录一张待注入上下文的图片。
    pub async fn add_image(&self, file_id: String) {
        self.images.lock().await.push(file_id);
    }

    /// 取走本轮待注入的图片。
    pub async fn take_images(&self) -> Vec<String> {
        std::mem::take(&mut *self.images.lock().await)
    }
}

/// 一个可由模型调用的工具。
#[async_trait]
pub trait Tool: Send + Sync {
    /// 工具名，与模型调用一致。
    fn name(&self) -> &str;

    /// 提供给模型的函数定义。
    fn definition(&self) -> Value;

    /// 执行工具，返回给模型的文本结果。
    async fn call(&self, args: Value, ctx: &ToolContext) -> String;
}

/// 工具注册表。
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    fn new() -> Self {
        Self {
            tools: vec![
                Box::new(file::FileTool),
                Box::new(file::EditTool),
                Box::new(exec::ExecTool),
                Box::new(task::TaskTool),
                Box::new(send::SendTool),
                Box::new(subagent::SubagentTool),
                Box::new(subagent::OpSubagentTool),
                Box::new(view_image::ViewImageTool),
            ],
        }
    }

    /// 提供给模型的全部工具定义。
    ///
    /// `include_subagent` 为 false 时不暴露 `subagent` / `op_subagent`（子代理内部使用）。
    pub fn definitions(&self, include_subagent: bool) -> Value {
        Value::Array(
            self.tools
                .iter()
                .filter(|t| include_subagent || !matches!(t.name(), "subagent" | "op_subagent"))
                .map(|t| t.definition())
                .collect(),
        )
    }

    /// 按名调用工具。
    pub async fn call(&self, name: &str, args: Value, ctx: &ToolContext) -> String {
        if ctx.depth > 0 && name == "subagent" {
            return "❌ 子代理不能再创建子代理".to_string();
        }
        let chat = log::chat(&ctx.chat_id);
        log::info(
            "tool",
            format!(
                "{chat} 调用 {name} {}",
                log::preview(&args.to_string(), 120)
            ),
        );
        let started = Instant::now();
        let result = {
            ctx.enter_tool();
            let result = match self.tools.iter().find(|t| t.name() == name) {
                Some(tool) => tool.call(args, ctx).await,
                None => format!("未知工具: {name}"),
            };
            ctx.exit_tool();
            result
        };
        log::info(
            "tool",
            format!(
                "{chat} {name} {:.2}s {}字",
                started.elapsed().as_secs_f64(),
                result.chars().count()
            ),
        );
        notify_result(ctx, name, &result).await;
        result
    }
}

static REGISTRY: Lazy<ToolRegistry> = Lazy::new(ToolRegistry::new);

/// 全局工具注册表。
pub fn registry() -> &'static ToolRegistry {
    &REGISTRY
}

/// 发送一条进度提示（短暂展示后自动删除；子代理上下文静默）。
pub async fn notify(ctx: &ToolContext, msg: &str) {
    log::info("tool", format!("{} {msg}", log::chat(&ctx.chat_id)));
    if ctx.silent {
        return;
    }
    // 不设 TTL：这条提示会在工具结束时被结果原地更新，
    // 再由结果流程统一安排「展示 N 秒后删除」。
    if let Ok(id) = ctx
        .channel
        .send(
            OutboundMessage::text(ctx.chat_id.clone(), msg)
                .kind(MsgKind::Text)
                .fold(),
        )
        .await
    {
        ctx.remember_notice(id, msg).await;
    }
}

/// 回显工具执行结果（折叠、自动删除；子代理上下文静默）。
pub async fn notify_result(ctx: &ToolContext, name: &str, result: &str) {
    if ctx.silent {
        return;
    }
    let total = result.chars().count();
    let brief: String = result.chars().take(NOTIFY_RESULT_CHARS).collect();
    let header = if total > NOTIFY_RESULT_CHARS {
        format!("📤 {name} 结果（{total} 字符，已截断）")
    } else {
        format!("📤 {name} 结果")
    };

    let text = format!("{header}:\n{brief}");

    // 首选：把结果追加到本次工具的提示消息里（调用内容与结果同一条消息）
    if let Some((id, original)) = ctx.last_notice().await {
        // 提示本身可能不短（例如写入内容的预览），合并后仍要留足余量
        let used = original.chars().count() + header.chars().count() + 4;
        let room = NOTIFY_MERGED_CHARS.saturating_sub(used);
        let merged = format!("{original}\n\n{header}:\n{}", head(&brief, room));
        let message = OutboundMessage::text(ctx.chat_id.clone(), &merged)
            .kind(MsgKind::Text)
            .fold();
        if ctx.channel.edit(&message, &id).await.is_ok() {
            // 结果展示 notify_ttl(name) 秒后删除
            let _ = ctx
                .channel
                .delete_later(&ctx.chat_id, &id, notify_ttl(name))
                .await;
            return;
        }
    }

    // 兜底：提示消息已被删除（工具执行超过提示存活时间）或平台不支持编辑
    let _ = ctx
        .channel
        .send(
            OutboundMessage::text(ctx.chat_id.clone(), &text)
                .kind(MsgKind::Text)
                .fold()
                .ttl(notify_ttl(name)),
        )
        .await;
}

/// 取文本开头 `max` 个字符，超出部分用省略号代替。
fn head(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}…")
}

/// 截断过长文本（保留最新部分）。
pub fn truncate(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let cut = text
        .char_indices()
        .nth(total - max)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    text[cut..].to_string()
}
