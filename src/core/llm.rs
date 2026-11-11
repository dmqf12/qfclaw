//! 模型（LLM）客户端。

use std::fs;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use serde_json::{Value, json};

use crate::config::ModelConfig;
use crate::core::session;
use crate::log;
use crate::tool::registry;

/// 单次回复的最大 token 数。
const MAX_OUTPUT_TOKENS: u32 = 20_480;

/// 单次请求的超时（长回复可能生成较久）。
const REQUEST_TIMEOUT_SECS: u64 = 600;

/// 一次对话请求。
pub struct ChatRequest {
    base_url: String,
    api_key: String,
    model: String,
    thinking: String,
    pub messages: Vec<Value>,
    tools: Value,
    tool_choice: String,
    max_tokens: u32,
}

impl ChatRequest {
    /// 构造请求：加载该会话历史 + system 提示 + 用户输入。
    ///
    /// `chat` 为会话键（`chat_id` 或 `chat_id:thread_id`）。
    pub fn init(chat: &str, user_input: &str) -> Result<Self> {
        if user_input.is_empty() {
            return Err(anyhow!("空消息"));
        }

        let mut messages = session::load_history(chat)?;
        messages.insert(
            0,
            json!({ "role": "system", "content": build_system_prompt(chat) }),
        );
        messages.push(json!({ "role": "user", "content": user_input }));

        Self::build(messages)
    }

    /// 构造不带工具的压缩请求。
    pub fn summary(system: &str, content: &str) -> Result<Self> {
        let mut request = Self::build(vec![
            json!({ "role": "system", "content": system }),
            json!({ "role": "user", "content": content }),
        ])?;
        request.tools = json!([]);
        request.tool_choice = "none".to_string();
        Ok(request)
    }

    fn build(messages: Vec<Value>) -> Result<Self> {
        let cfg = ModelConfig::load()?;
        Ok(Self {
            model: cfg.model_name,
            thinking: cfg.reasoning,
            messages,
            tools: registry().definitions(true),
            tool_choice: "auto".to_string(),
            max_tokens: MAX_OUTPUT_TOKENS,
            api_key: cfg.api_key,
            base_url: cfg.base_url,
        })
    }

    /// 用已有消息构造请求（子代理用，不读写会话文件）。
    ///
    /// `messages` 需自带 system 消息。
    pub fn from_messages(messages: Vec<Value>) -> Result<Self> {
        if messages.is_empty() {
            return Err(anyhow!("空消息列表"));
        }
        Self::build(messages)
    }

    /// 替换可用工具定义。
    pub fn set_tools(&mut self, tools: Value) {
        self.tools = tools;
    }

    /// 发起一次请求，返回模型响应。
    pub async fn send(&self) -> Result<Value> {
        let payload = json!({
            "model": &self.model,
            "thinking": {"type": &self.thinking},
            "messages": &self.messages,
            "tools": &self.tools,
            "tool_choice": &self.tool_choice,
            "max_tokens": &self.max_tokens
        });
        // 长回复可能生成较久，用超时兜底避免请求永久挂起
        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .unwrap_or_else(|_| Client::new());
        let url = format!("{}/chat/completions", self.base_url);
        log::info(
            "llm",
            format!(
                "请求 {}：{} 条消息，{} 个工具，tool_choice={}，max_tokens={}",
                self.model,
                self.messages.len(),
                self.tools.as_array().map(Vec::len).unwrap_or(0),
                self.tool_choice,
                self.max_tokens
            ),
        );
        let started = Instant::now();
        let response = client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .inspect_err(|e| {
                log::error(
                    "llm",
                    format!(
                        "请求发送失败（耗时 {:.1}s）：{e}",
                        started.elapsed().as_secs_f64()
                    ),
                )
            })?;

        let status = response.status();
        if !status.is_success() {
            let error_json: Value = response.json().await.context("请求失败")?;
            let message = error_json["error"]["message"]
                .as_str()
                .unwrap_or("请求失败");
            log::error(
                "llm",
                format!(
                    "HTTP {status}（耗时 {:.1}s）：{message}",
                    started.elapsed().as_secs_f64()
                ),
            );
            return Err(anyhow!("API 请求失败: {}", message));
        }

        let reply: Value = response.json().await?;
        log::info(
            "llm",
            format!(
                "响应 {status}：耗时 {:.1}s，完成原因 {}，usage {}",
                started.elapsed().as_secs_f64(),
                reply["choices"][0]["finish_reason"].as_str().unwrap_or("-"),
                reply["usage"]["total_tokens"].as_u64().unwrap_or(0)
            ),
        );
        Ok(reply)
    }
}

/// 基础 system 提示（SOUL / Agent / User）。
pub fn build_base_prompt() -> String {
    let files = [
        "../config/Soul.md",
        "../config/Agent.md",
        "../config/User.md",
    ];
    files
        .iter()
        .filter_map(|f| fs::read_to_string(f).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 主代理的 system 提示：基础提示 + 该会话的长期记忆。
pub fn build_system_prompt(chat: &str) -> String {
    let mut prompt = build_base_prompt();

    let memory = session::memory(chat);
    if !memory.trim().is_empty() {
        prompt.push_str("\n\n## 历史记忆\n");
        prompt.push_str(memory.trim());
    }
    prompt
}
