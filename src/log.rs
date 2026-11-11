//! 统一日志。
//!
//! 格式为 `[模块] 内容`：信息级写 stdout，警告/错误写 stderr，
//! 由 systemd（journalctl）负责记录时间戳。
//!
//! 模块名用短标签区分来源：`main`、`tg`（Telegram）、`agent`（对话循环）、
//! `llm`（模型请求）、`tool`（工具调用）、`session`（会话存储）、
//! `compact`（上下文压缩）、`cmd`（斜杠指令）。

/// 信息级日志：正常流程的关键节点。
pub fn info(tag: &str, msg: impl AsRef<str>) {
    println!("[{tag}] {}", msg.as_ref());
}

/// 警告级日志：能继续运行但值得注意的异常。
pub fn warn(tag: &str, msg: impl AsRef<str>) {
    eprintln!("[{tag}] ⚠️  {}", msg.as_ref());
}

/// 错误级日志：操作失败。
pub fn error(tag: &str, msg: impl AsRef<str>) {
    eprintln!("[{tag}] ❌  {}", msg.as_ref());
}

/// 截断长文本，用于日志里预览用户输入 / 工具参数 / 工具结果。
pub fn preview(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out.replace('\n', "⏎")
}
