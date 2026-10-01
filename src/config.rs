//! 配置读取。
//!
//! 集中读取 `config/` 下的配置，业务代码不再各自解析 JSON。

use std::fs;

use anyhow::{anyhow, Result};
use serde_json::Value;

/// 平台（Bot）配置。
pub struct BotConfig {
    pub token: String,
    pub base_url: String,
    pub commands: Vec<Value>,
    /// 允许使用的用户 ID。
    pub allow_user_id: String,
}

impl BotConfig {
    pub fn load() -> Self {
        let json: Value = fs::read_to_string("config/bot.json")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);

        Self {
            token: json["token"].as_str().unwrap_or_default().to_string(),
            base_url: json["base_url"]
                .as_str()
                .unwrap_or("https://api.telegram.org/bot")
                .to_string(),
            commands: json["commands"].as_array().cloned().unwrap_or_default(),
            allow_user_id: json["allow_user_id"].as_i64().unwrap_or(0).to_string(),
        }
    }
}

/// 模型配置。
pub struct ModelConfig {
    pub api_key: String,
    pub base_url: String,
    pub model_name: String,
    /// 推理模式：enabled / adaptive / disabled。
    pub reasoning: String,
}

impl ModelConfig {
    pub fn load() -> Result<Self> {
        let json = qffunc::file_to_json("config/models.json")
            .map_err(|e| anyhow!("models.json 解析失败: {e}"))?;

        let use_model = json["use_model"].as_str().unwrap_or_default();
        let model = json
            .get(use_model)
            .ok_or_else(|| anyhow!("models.json 中未找到模型: {use_model}"))?;

        Ok(Self {
            api_key: model["token"].as_str().unwrap_or_default().to_string(),
            base_url: model["base_url"]
                .as_str()
                .unwrap_or("https://api.deepseek.com/v1")
                .to_string(),
            model_name: model["model_name"].as_str().unwrap_or_default().to_string(),
            reasoning: json["config"]["reasoning"]
                .as_str()
                .unwrap_or("disabled")
                .to_string(),
        })
    }
}
