//! 配置读取。
//!
//! 集中读取 `config/` 下的配置，业务代码不再各自解析 JSON。


use anyhow::{anyhow, Result};

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
