//! 模型（LLM）客户端。

use std::fs;

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde_json::{json, Value};

use crate::config::ModelConfig;
use crate::core::session;
use crate::tool::registry;

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
    /// 构造请求：加载历史 + system 提示 + 用户输入。
    pub fn init(user_input: &str) -> Result<Self> {
        println!("{user_input}");
        if user_input.is_empty() {
            return Err(anyhow!("空消息"));
        }
        let cfg = ModelConfig::load()?;

        let mut messages = session::load_history()?;
        messages.insert(
            0,
            json!({ "role": "system", "content": build_system_prompt() }),
        );
        messages.push(json!({ "role": "user", "content": user_input }));

        Ok(Self {
            model: cfg.model_name,
            thinking: cfg.reasoning,
            messages,
            tools: registry().definitions(),
            tool_choice: "auto".to_string(),
            max_tokens: 20480,
            api_key: cfg.api_key,
            base_url: cfg.base_url,
        })
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
        let client = Client::new();
        let url = format!("{}/chat/completions", self.base_url);
        let response = client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let error_json: Value = response.json().await.context("请求失败")?;
            let message = error_json["error"]["message"].as_str().unwrap_or("请求失败");
            return Err(anyhow!("API 请求失败: {}", message));
        }

        Ok(response.json().await?)
    }
}

/// 拼接 system 提示（SOUL / Agent / User）。
fn build_system_prompt() -> String {
    let files = ["config/SOUL.md", "config/Agent.md", "config/User.md"];
    files
        .iter()
        .filter_map(|f| fs::read_to_string(f).ok())
        .collect::<Vec<_>>()
        .join("\n")
}
