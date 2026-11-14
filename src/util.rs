//! 通用工具：JSON 文件读取。

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

/// 读取并解析 JSON 文件。
pub fn read_json_file(path: &str) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("读取 {path} 失败"))?;
    serde_json::from_str(&text).with_context(|| format!("解析 {path} 失败"))
}

/// 读取 JSON 文件里的顶层字段（必须是对象）。
pub fn read_json_section(path: &str, key: &str) -> Result<Value> {
    let json = read_json_file(path)?;
    match json.get(key) {
        Some(value) if value.is_object() => Ok(value.clone()),
        Some(_) => Err(anyhow!("{path} 中的 {key} 不是对象")),
        None => Err(anyhow!("{path} 缺少 {key}")),
    }
}
