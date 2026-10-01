//! 工具层。
//!
//! 每个工具实现 [`Tool`]，由 [`ToolRegistry`] 统一注册与分发。
//! 工具通过 [`ToolContext`] 访问消息通道、会话与取消信号。

pub mod exec;
pub mod file;
pub mod task;

use std::sync::Arc;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::channel::{Channel, MsgKind, OutboundMessage};

/// 单次工具输出的字符上限。
pub const MAX_OUTPUT_CHARS: usize = 4_000;

/// 工具执行上下文。
pub struct ToolContext {
    pub channel: Arc<dyn Channel>,
    pub chat_id: String,
    /// 任务被取消时触发（`/stop`）。
    pub cancel: CancellationToken,
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
            ],
        }
    }

    /// 全部工具的模型定义。
    pub fn definitions(&self) -> Value {
        Value::Array(self.tools.iter().map(|t| t.definition()).collect())
    }

    /// 按名调用工具。
    pub async fn call(&self, name: &str, args: Value, ctx: &ToolContext) -> String {
        match self.tools.iter().find(|t| t.name() == name) {
            Some(tool) => tool.call(args, ctx).await,
            None => format!("未知工具: {name}"),
        }
    }
}

static REGISTRY: Lazy<ToolRegistry> = Lazy::new(ToolRegistry::new);

/// 全局工具注册表。
pub fn registry() -> &'static ToolRegistry {
    &REGISTRY
}

/// 发送一条进度提示（短暂展示后自动删除）。
pub async fn notify(ctx: &ToolContext, msg: &str) {
    println!("{msg}");
    let _ = ctx
        .channel
        .send(
            OutboundMessage::text(ctx.chat_id.clone(), msg)
                .kind(MsgKind::Progress)
                .fold()
                .ttl(5),
        )
        .await;
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
