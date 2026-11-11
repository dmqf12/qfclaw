//! 文件操作工具：读取 / 写入 / 删除 / 精确编辑。

use std::fs;
use std::io::{BufRead, BufReader, Read};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolContext, notify};

/// 读取内容的字符上限。
const MAX_READ_CHARS: usize = 80_000;
/// 写入内容的字符上限。
const MAX_WRITE_CHARS: usize = 80_000;
/// 二进制探测读取的字节数。
const BINARY_PROBE_BYTES: u64 = 8 * 1024;

/// 探测文件是否为二进制：包含 NUL 字节即视为二进制。
fn is_binary(file: &str) -> Result<bool, String> {
    let mut probe = Vec::new();
    fs::File::open(file)
        .map_err(|e| format!("无法读取文件 {file}: {e}"))?
        .take(BINARY_PROBE_BYTES)
        .read_to_end(&mut probe)
        .map_err(|e| format!("无法读取文件 {file}: {e}"))?;
    Ok(probe.contains(&0))
}

/// 字节位置所在的行号（1 起）。
fn line_of(content: &str, pos: usize) -> usize {
    content[..pos].bytes().filter(|b| *b == b'\n').count() + 1
}

/// 取指定行前后的片段（带行号），用于定位差异。
fn context_around(content: &str, line: usize, span: usize) -> String {
    let start = line.saturating_sub(span).max(1);
    let end = line + span;
    content
        .lines()
        .enumerate()
        .filter(|(i, _)| {
            let n = i + 1;
            n >= start && n <= end
        })
        .map(|(i, l)| format!("{}| {l}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 读取文本文件，返回带行号的内容。
///
/// `offset` 为起始行（1 起），`limit` 为最多读取行数；并按字符上限截断。
fn read_text(file: &str, offset: Option<usize>, limit: Option<usize>) -> String {
    match is_binary(file) {
        Err(e) => return e,
        Ok(true) => return format!("❌ 无法读取 {file}：这是二进制文件"),
        Ok(false) => {}
    }

    let start = offset.unwrap_or(1).max(1);
    let max_lines = limit.unwrap_or(usize::MAX);

    let handle = match fs::File::open(file) {
        Ok(f) => f,
        Err(e) => return format!("无法读取文件 {file}: {e}"),
    };

    let mut out = String::new();
    let mut chars = 0usize;
    let mut line_no = 0usize;
    let mut truncated = false;

    for line in BufReader::new(handle).lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => return format!("读取失败 {file}: {e}"),
        };
        line_no += 1;
        if line_no < start {
            continue;
        }
        if line_no >= start.saturating_add(max_lines) {
            truncated = true;
            break;
        }
        let entry = format!("{line_no}| {line}\n");
        let entry_chars = entry.chars().count();
        if chars + entry_chars > MAX_READ_CHARS {
            truncated = true;
            break;
        }
        chars += entry_chars;
        out.push_str(&entry);
    }

    if truncated {
        out.push_str("…（已截断，可用 offset/limit 继续读取）");
    }
    out.trim_end().to_string()
}

/// 读取 / 写入 / 删除文件。
pub struct FileTool;

#[async_trait]
impl Tool for FileTool {
    fn name(&self) -> &str {
        "operate_file"
    }

    fn definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "operate_file",
                "description": "读取、写入或删除本地文件系统中的文件。仅支持文本文件。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "文件的绝对或相对路径" },
                        "operation": { "type": "string", "description": "要进行的操作：read / write / delete" },
                        "content": { "type": "string", "description": "write 时写入的内容，留空则创建空文件" },
                        "offset": { "type": "integer", "description": "read 时的起始行（1 起），默认 1" },
                        "limit": { "type": "integer", "description": "read 时最多读取的行数，默认不限" }
                    },
                    "required": ["file", "operation"]
                }
            }
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> String {
        let file = args["file"].as_str().unwrap_or("");
        let operation = args["operation"].as_str().unwrap_or("");
        match operation {
            "read" => {
                let offset = args["offset"].as_u64().map(|v| v as usize);
                let limit = args["limit"].as_u64().map(|v| v as usize);
                notify(ctx, &format!("🔧读取：{file}")).await;
                read_text(file, offset, limit)
            }
            "write" => {
                let content = args["content"].as_str().unwrap_or("");
                if content.chars().count() > MAX_WRITE_CHARS {
                    return format!(
                        "❌ 写入内容过长（{} 字符 > {MAX_WRITE_CHARS}）",
                        content.chars().count()
                    );
                }
                // 文件已存在时先确认不是二进制，避免覆盖破坏数据；不存在则允许新建
                if fs::metadata(file).is_ok() {
                    match is_binary(file) {
                        Err(e) => return e,
                        Ok(true) => return format!("❌ 无法写入 {file}：这是二进制文件"),
                        Ok(false) => {}
                    }
                }
                notify(ctx, &format!("✏️写入：{file}")).await;
                match fs::write(file, content) {
                    Ok(_) => format!("写入成功: {file}"),
                    Err(e) => format!("写入失败 {file}: {e}"),
                }
            }
            "delete" => {
                notify(ctx, &format!("🗑️删除：{file}")).await;
                match fs::remove_file(file) {
                    Ok(_) => format!("✅删除成功: {file}"),
                    Err(e) => format!("❌删除失败 {file}: {e}"),
                }
            }
            _ => format!("未知操作: {operation}"),
        }
    }
}

/// 精确字符串替换，只输出改动片段，节省 token。
pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "精确替换文件中的一段文本。old_string 必须在文件中唯一出现。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "文件的绝对或相对路径" },
                        "old_string": { "type": "string", "description": "要被替换的原文，必须唯一" },
                        "new_string": { "type": "string", "description": "替换后的内容" }
                    },
                    "required": ["file", "old_string", "new_string"]
                }
            }
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> String {
        let file = args["file"].as_str().unwrap_or("");
        let old = args["old_string"].as_str().unwrap_or("");
        let new = args["new_string"].as_str().unwrap_or("");

        if old.is_empty() {
            return "❌ old_string 不能为空".to_string();
        }
        if new.chars().count() > MAX_WRITE_CHARS {
            return format!(
                "❌ new_string 过长（{} 字符 > {MAX_WRITE_CHARS}）",
                new.chars().count()
            );
        }
        if !std::path::Path::new(file).exists() {
            return format!("❌ 文件不存在: {file}");
        }
        match is_binary(file) {
            Err(e) => return e,
            Ok(true) => return format!("❌ 无法编辑 {file}：这是二进制文件"),
            Ok(false) => {}
        }

        notify(ctx, &format!("📝编辑：{file}")).await;

        let content = match fs::read_to_string(file) {
            Ok(c) => c,
            Err(e) => return format!("读取失败 {file}: {e}"),
        };

        // 定位：只允许唯一匹配，避免改错位置
        let pos = match content.matches(old).count() {
            0 => {
                // 用首行定位，方便排查空白 / 换行 / 全角半角差异
                let first_line = old.lines().next().unwrap_or(old);
                let hint = match content.find(first_line) {
                    Some(hit) => {
                        let line = line_of(&content, hit);
                        format!(
                            "文件第 {line} 行有相同的开头，请检查空白或换行差异：\n{}",
                            context_around(&content, line, 2)
                        )
                    }
                    None => format!("old_string 的首行在文件中不存在：{first_line}"),
                };
                return format!(
                    "❌ 未找到要替换的内容，请确认 old_string 与文件完全一致。\n{hint}"
                );
            }
            1 => content.find(old).unwrap_or(0),
            n => return format!("❌ old_string 匹配到 {n} 处，请提供更长的唯一上下文"),
        };

        let start = line_of(&content, pos);
        let end = start + old.lines().count().saturating_sub(1);
        let updated = content.replacen(old, new, 1);
        match fs::write(file, updated) {
            Ok(_) if start == end => format!("✅ 编辑成功: {file}（第 {start} 行）"),
            Ok(_) => format!("✅ 编辑成功: {file}（第 {start}-{end} 行）"),
            Err(e) => format!("写入失败 {file}: {e}"),
        }
    }
}
