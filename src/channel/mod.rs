//! 消息平台抽象层。
//!
//! 各平台通过实现 [`Channel`] 接入。业务层只使用这里的统一消息模型，
//! 不感知具体平台。

pub mod telegram;

use anyhow::{Result, bail};
use async_trait::async_trait;
use tokio::sync::mpsc;

/// 平台消息 ID。各平台类型不同，统一为字符串。
pub type MsgId = String;

/// 会话 ID。
///
/// 形如 `chat_id` 或 `chat_id:thread_id`——后者对应 Telegram 论坛话题
/// （Threaded Mode），业务层把它当作不透明的会话键，
/// 由平台实现负责拼装与拆分。
pub type ChatId = String;

/// 消息用途，决定平台如何呈现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgKind {
    /// 普通文本，按 Markdown 解析。
    Text,
    /// 纯文本，不做任何格式解析（用于指令回复等含特殊字符的内容）。
    Plain,
    /// 错误提示，同样不做格式解析。
    Error,
    /// 文件。
    File,
}

/// 内联按钮。
#[derive(Debug, Clone)]
pub struct Button {
    pub text: String,
    /// 点击后回传的数据。
    pub data: String,
}

impl Button {
    pub fn new(text: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            data: data.into(),
        }
    }
}

/// 按钮回调（用户点击内联按钮时产生）。
#[derive(Debug, Clone)]
pub struct Callback {
    /// 按钮携带的数据。
    pub data: String,
    /// 被点击消息的 ID。
    pub msg_id: MsgId,
}

/// 收到的消息。
#[derive(Debug, Clone)]
pub struct InboundMessage {
    /// 发送者用户 ID。
    pub user_id: String,
    pub chat_id: ChatId,
    pub content: String,
    /// 若为按钮回调则为 `Some`。
    pub callback: Option<Callback>,
    /// 话题标题（Telegram 的 `forum_topic_created` / `forum_topic_edited`
    /// 服务消息，或消息里 `reply_to_message` 携带的标题）。
    pub topic_name: Option<String>,
}

/// 待发送的消息。
#[derive(Debug, Clone)]
pub struct OutboundMessage {
    pub chat_id: ChatId,
    pub content: String,
    pub kind: MsgKind,
    /// 内联按钮（按行分组）。
    pub buttons: Vec<Vec<Button>>,
    /// 是否以折叠形式展示（用于冗长内容）。
    pub fold: bool,
    /// 存活秒数，到期后自动删除；`None` 表示保留。
    pub ttl: Option<u64>,
}

impl OutboundMessage {
    /// 创建一条文本消息。
    pub fn text(chat_id: impl Into<ChatId>, text: &str) -> Self {
        Self {
            chat_id: chat_id.into(),
            content: text.into(),
            kind: MsgKind::Text,
            buttons: Vec::new(),
            fold: false,
            ttl: None,
        }
    }

    /// 设置消息用途。
    pub fn kind(mut self, kind: MsgKind) -> Self {
        self.kind = kind;
        self
    }

    /// 设置内联按钮。
    pub fn buttons(mut self, buttons: Vec<Vec<Button>>) -> Self {
        self.buttons = buttons;
        self
    }

    /// 以折叠形式展示。
    pub fn fold(mut self) -> Self {
        self.fold = true;
        self
    }

    /// 设置存活秒数。
    pub fn ttl(mut self, secs: u64) -> Self {
        self.ttl = Some(secs);
        self
    }
}

/// 消息平台。
///
/// 实现者只负责「收发消息」与「格式转换」，不包含业务逻辑。
#[async_trait]
pub trait Channel: Send + Sync + 'static {
    /// 启动前初始化（注册命令等），默认无操作。
    async fn init(&self) -> Result<()> {
        Ok(())
    }

    /// 启动接收循环：持续拉取消息并推入 `tx`。
    async fn run(&self, tx: mpsc::Sender<InboundMessage>) -> Result<()>;

    /// 删除对话对应的话题（含话题内全部消息）。
    ///
    /// 平台不支持话题、或当前会话不是话题时返回错误。
    async fn delete_topic(&self, _chat_id: &str) -> Result<()> {
        bail!("当前平台不支持话题")
    }

    /// 在支持话题（Threaded Mode）的平台创建新话题，返回新会话键。
    ///
    /// 平台不支持时返回错误，调用方可退化为重置当前对话。
    async fn create_topic(&self, _chat_id: &str, _name: &str) -> Result<ChatId> {
        bail!("当前平台不支持创建话题")
    }

    /// 发送消息，返回其 ID。
    async fn send(&self, msg: OutboundMessage) -> Result<MsgId>;

    /// 删除消息。
    async fn delete(&self, chat_id: &str, id: &str) -> Result<()>;

    /// 是否为白名单用户（支持配置多个）。
    fn is_allowed(&self, user_id: &str) -> bool;
}
