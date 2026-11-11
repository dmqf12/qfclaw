//! 查看图片工具：把本地或网络图片交给模型。
//!
//! 图片通过 Files API 上传得到 `file_id`，再由对话循环注入上下文。
//! 上传成功的文件保留在服务端（按内容哈希缓存复用），
//! 只有上传失败（例如存储已满）时才清理最旧的文件后重试。
//!
//! URL 图片会先下载到 `../tmp/`（运行目录下的 tmp，启动时清空）。

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use reqwest::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::{Tool, ToolContext, notify};
use crate::config::ModelConfig;
use crate::log;

/// 单张图片体积上限。
const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
/// URL 图片的下载目录（相对 workspace）。
const TMP_DIR: &str = "../tmp";
/// file_id 缓存文件。
const CACHE_FILE: &str = "../config/deepseek/file_id.json";
/// 存储不足时的重试次数。
const UPLOAD_RETRIES: usize = 3;

/// 串行化缓存读改写。
static CACHE_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

pub struct ViewImageTool;

#[async_trait]
impl Tool for ViewImageTool {
    fn name(&self) -> &str {
        "view_image"
    }

    fn definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "view_image",
                "description": "查看本地文件或网络图片，图片内容会加入你的上下文。支持 jpg/jpeg/png/gif/webp，单张不超过 16MB。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "本地文件路径或 http(s) 图片地址"
                        }
                    },
                    "required": ["path"]
                }
            }
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> String {
        let path = args["path"].as_str().unwrap_or_default().trim();
        if path.is_empty() {
            return "❌ path 不能为空".to_string();
        }

        let cfg = match ModelConfig::load() {
            Ok(cfg) => cfg,
            Err(e) => return format!("❌ 读取模型配置失败: {e}"),
        };

        let is_url = path.starts_with("http://") || path.starts_with("https://");
        let (data, name) = if is_url {
            // 取消（/stop 或子代理超时）时立即中断下载，不等超时
            let cancel = ctx.cancel_token().await;
            tokio::select! {
                _ = cancel.cancelled() => return "🛑 用户手动中止调用".to_string(),
                result = download(path) => match result {
                    Ok(v) => v,
                    Err(e) => return e,
                },
            }
        } else {
            match std::fs::read(path) {
                Ok(data) => {
                    let name = Path::new(path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "image".to_string());
                    (data, name)
                }
                Err(e) => return format!("❌ 读取图片失败: {e}"),
            }
        };

        if data.len() as u64 > MAX_IMAGE_BYTES {
            return format!(
                "❌ 图片过大（{:.1}MB，上限 16MB）",
                data.len() as f64 / 1024.0 / 1024.0
            );
        }
        if !is_image_name(&name) {
            return format!("❌ 不支持的图片格式: {name}（仅支持 jpg/jpeg/png/gif/webp）");
        }

        notify(ctx, &format!("🖼 查看图片：{path}")).await;

        let hash = sha256_hex(&data);
        let file_id = match cached_file_id(&hash).await {
            Some(id) => id,
            None => {
                // 上传中途取消时直接放弃，避免长时间占用连接
                let cancel = ctx.cancel_token().await;
                let upload = upload_with_cleanup(&cfg, &data, &name);
                tokio::select! {
                    _ = cancel.cancelled() => return "🛑 用户手动中止调用".to_string(),
                    result = upload => match result {
                        Ok(id) => {
                            save_file_id(&hash, &id).await;
                            id
                        }
                        Err(e) => return format!("❌ 上传图片失败: {e}"),
                    },
                }
            }
        };

        ctx.add_image(file_id).await;
        format!("已加载图片：{path}")
    }
}

/// 是否为支持的图片文件名。
fn is_image_name(name: &str) -> bool {
    let ext = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "gif" | "webp")
}

fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 下载网络图片到 `../tmp/`，返回内容与文件名。
async fn download(url: &str) -> Result<(Vec<u8>, String), String> {
    let resp = Client::new()
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("❌ 下载图片失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("❌ 下载图片失败: HTTP {}", resp.status()));
    }

    // 按 MIME 决定扩展名，URL 里没有后缀时也能正确识别
    let ext = match resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
    {
        t if t.contains("png") => "png",
        t if t.contains("gif") => "gif",
        t if t.contains("webp") => "webp",
        t if t.contains("jpeg") || t.contains("jpg") => "jpg",
        _ => {
            let name = url
                .split('?')
                .next()
                .unwrap_or(url)
                .rsplit('/')
                .next()
                .unwrap_or_default();
            if is_image_name(name) {
                return save_download(resp, name).await;
            }
            return Err("❌ 无法识别图片类型".to_string());
        }
    };
    save_download(resp, &format!("image.{ext}")).await
}

/// 把响应体写入 tmp 目录。
async fn save_download(resp: reqwest::Response, name: &str) -> Result<(Vec<u8>, String), String> {
    let data = resp
        .bytes()
        .await
        .map_err(|e| format!("❌ 读取图片数据失败: {e}"))?
        .to_vec();
    if data.len() as u64 > MAX_IMAGE_BYTES {
        return Err(format!(
            "❌ 图片过大（{:.1}MB，上限 16MB）",
            data.len() as f64 / 1024.0 / 1024.0
        ));
    }
    tokio::fs::create_dir_all(TMP_DIR)
        .await
        .map_err(|e| format!("❌ 创建临时目录失败: {e}"))?;
    let safe = format!("{}/{}", TMP_DIR, name.replace(['/', '\\'], "_"));
    tokio::fs::write(&safe, &data)
        .await
        .map_err(|e| format!("❌ 保存图片失败: {e}"))?;
    Ok((data, name.to_string()))
}

/// 上传图片，存储不足时清理最旧的文件后重试。
async fn upload_with_cleanup(cfg: &ModelConfig, data: &[u8], name: &str) -> Result<String, String> {
    let mut attempt = 0;
    loop {
        match upload(cfg, data, name).await {
            Ok(id) => return Ok(id),
            Err(e) if is_storage_error(&e) && attempt < UPLOAD_RETRIES => {
                attempt += 1;
                free_oldest(cfg).await?;
            }
            Err(e) => return Err(e),
        }
    }
}

/// 上传单个文件到 Files API。
async fn upload(cfg: &ModelConfig, data: &[u8], name: &str) -> Result<String, String> {
    let part = reqwest::multipart::Part::bytes(data.to_vec()).file_name(name.to_string());
    let form = reqwest::multipart::Form::new()
        .text("purpose", "user_data")
        .part("file", part);
    let resp = Client::new()
        .post(format!("{}/files", cfg.files_base_url))
        .bearer_auth(&cfg.api_key)
        .multipart(form)
        .timeout(Duration::from_secs(180))
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("读取响应失败: {e}"))?;
    if !status.is_success() {
        return Err(text);
    }
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("解析响应失败: {e}: {text}"))?;
    value["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("响应中没有 id: {text}"))
}

/// 是否为存储空间不足类错误。
///
/// 只认明确的存储关键词，并排除限流等会误伤的情况——误判会导致删掉用户已上传的文件。
fn is_storage_error(err: &str) -> bool {
    let err = err.to_lowercase();
    if err.contains("rate") || err.contains("too many") || err.contains("timeout") {
        return false;
    }
    ["storage", "space", "quota", "full", "exceed"]
        .iter()
        .any(|key| err.contains(key))
}

/// 列出 Files API 上的文件。
async fn list_files(cfg: &ModelConfig) -> Result<Vec<Value>, String> {
    let resp = Client::new()
        .get(format!("{}/files", cfg.files_base_url))
        .bearer_auth(&cfg.api_key)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?;
    let text = resp
        .text()
        .await
        .map_err(|e| format!("读取响应失败: {e}"))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("解析响应失败: {e}: {text}"))?;
    Ok(value["data"].as_array().cloned().unwrap_or_default())
}

/// 删除一个文件。
async fn delete_file(cfg: &ModelConfig, id: &str) -> Result<(), String> {
    let resp = Client::new()
        .delete(format!("{}/files/{id}", cfg.files_base_url))
        .bearer_auth(&cfg.api_key)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("删除 {id} 失败: HTTP {}", resp.status()))
    }
}

/// 删除最旧的一批文件（约四分之一），为上传腾出空间。
///
/// 删除后同步清理 file_id 缓存，避免之后命中已经不存在于服务端的 file_id。
async fn free_oldest(cfg: &ModelConfig) -> Result<(), String> {
    let mut files = list_files(cfg).await?;
    if files.is_empty() {
        return Err("存储已满，且没有可删除的文件".to_string());
    }
    files.sort_by_key(|f| f["created_at"].as_u64().unwrap_or(0));
    let count = (files.len() / 4).max(1);
    let mut removed: Vec<String> = Vec::new();
    for file in files.iter().take(count) {
        if let Some(id) = file["id"].as_str()
            && delete_file(cfg, id).await.is_ok()
        {
            removed.push(id.to_string());
        }
    }
    if removed.is_empty() {
        return Err("存储已满，清理旧文件失败".to_string());
    }
    drop_cache_ids(&removed).await;
    Ok(())
}

/// 从缓存中移除指向指定 file_id 的条目。
async fn drop_cache_ids(ids: &[String]) {
    let _guard = CACHE_LOCK.lock().await;
    let mut cache = read_cache().await;
    if let Some(map) = cache.as_object_mut() {
        map.retain(|_, value| {
            value
                .as_str()
                .map(|id| !ids.iter().any(|removed| removed == id))
                .unwrap_or(true)
        });
    }
    if let Some(dir) = Path::new(CACHE_FILE).parent() {
        let _ = tokio::fs::create_dir_all(dir).await;
    }
    let _ = tokio::fs::write(CACHE_FILE, cache.to_string()).await;
}

/// 读取 file_id 缓存。
async fn read_cache() -> Value {
    tokio::fs::read_to_string(CACHE_FILE)
        .await
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

/// 命中缓存时返回 file_id。
async fn cached_file_id(hash: &str) -> Option<String> {
    read_cache()
        .await
        .get(hash)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// 写入 file_id 缓存。
async fn save_file_id(hash: &str, file_id: &str) {
    let _guard = CACHE_LOCK.lock().await;
    let mut cache = read_cache().await;
    if let Some(map) = cache.as_object_mut() {
        map.insert(hash.to_string(), json!(file_id));
    }
    if let Some(dir) = Path::new(CACHE_FILE).parent() {
        let _ = tokio::fs::create_dir_all(dir).await;
    }
    if let Err(e) = tokio::fs::write(CACHE_FILE, cache.to_string()).await {
        log::warn("image", format!("写入 file_id 缓存失败：{e}"));
    }
}
