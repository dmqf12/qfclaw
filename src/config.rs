//! 配置读取。
//!
//! 集中读取 `config/` 下的配置，业务代码不再各自解析 JSON。

use anyhow::{Result, anyhow};

/// 模型配置。
pub struct ModelConfig {
    pub api_key: String,
    pub base_url: String,
    /// Files API 根地址（上传图片用），未配置时从 `base_url` 推导。
    pub files_base_url: String,
    pub model_name: String,
    /// 推理模式：enabled / adaptive / disabled。
    pub reasoning: String,
    /// 上下文窗口阈值：超过该 token 数就先压缩历史。
    pub context_window: u64,
}

impl ModelConfig {
    pub fn load() -> Result<Self> {
        let json = qffunc::file_to_json("../config/models.json")
            .map_err(|e| anyhow!("models.json 解析失败: {e}"))?;

        let use_model = json["use_model"].as_str().unwrap_or_default();
        let model = json
            .get(use_model)
            .ok_or_else(|| anyhow!("models.json 中未找到模型: {use_model}"))?;

        let base_url = model["base_url"]
            .as_str()
            .unwrap_or("https://api.deepseek.com/v1")
            .to_string();
        // Files API 根地址：优先取配置，否则从模型地址去掉版本前缀推导
        let files_base_url = json["config"]["files_base_url"]
            .as_str()
            .map(|s| s.trim_end_matches('/').to_string())
            .unwrap_or_else(|| {
                base_url
                    .trim_end_matches('/')
                    .trim_end_matches("/v1")
                    .to_string()
            });

        Ok(Self {
            api_key: model["token"].as_str().unwrap_or_default().to_string(),
            base_url,
            files_base_url,
            model_name: model["model_name"].as_str().unwrap_or_default().to_string(),
            reasoning: json["config"]["reasoning"]
                .as_str()
                .unwrap_or("disabled")
                .to_string(),
            context_window: json["config"]["context_window"].as_u64().unwrap_or(800_000),
        })
    }
}
