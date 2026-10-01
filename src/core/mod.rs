//! 业务核心层。
//!
//! 与具体消息平台无关：对话循环、模型客户端、会话存储、指令处理。

pub mod agent;
pub mod command;
pub mod llm;
pub mod session;
