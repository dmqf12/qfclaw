//! 统一日志。
//!
//! 输出形如 `[tg] #65179 ↓ 你好`：模块标签按来源着色，会话只显示短标识，
//! 避免每行都重复一长串 `chat=7398595453:65179`。
//!
//! 颜色用 ANSI 转义直接写进消息，journalctl / 终端会原样显示；
//! 设置环境变量 `NO_COLOR=1` 可关闭。

/// 复位颜色。
const RESET: &str = "\x1b[0m";

/// 模块标签颜色。
fn color(tag: &str) -> &'static str {
    match tag {
        "tg" => "\x1b[36m",       // 青
        "agent" => "\x1b[32m",    // 绿
        "subagent" => "\x1b[92m", // 亮绿
        "llm" => "\x1b[34m",      // 蓝
        "tool" => "\x1b[33m",     // 黄
        "compact" => "\x1b[35m",  // 品红
        "cmd" => "\x1b[97m",      // 亮白
        _ => "\x1b[90m",          // 灰
    }
}

/// 是否禁用颜色。
fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some()
}

/// 信息级日志：正常流程的关键节点。
pub fn info(tag: &str, msg: impl AsRef<str>) {
    let head = if no_color() {
        format!("[{tag}]")
    } else {
        format!("{}{RESET}", paint(color(tag), &format!("[{tag}]")))
    };
    println!("{head} {}", msg.as_ref());
}

/// 警告级日志：能继续运行但值得注意的异常。
pub fn warn(tag: &str, msg: impl AsRef<str>) {
    let line = format!("[{tag}] ⚠ {}", msg.as_ref());
    eprintln!("{}", paint("\x1b[33m", &line));
}

/// 错误级日志：操作失败。
pub fn error(tag: &str, msg: impl AsRef<str>) {
    let line = format!("[{tag}] ✖ {}", msg.as_ref());
    eprintln!("{}", paint("\x1b[31m", &line));
}

/// 给文本包上颜色。
fn paint(color: &str, text: &str) -> String {
    if no_color() {
        text.to_string()
    } else {
        format!("{color}{text}{RESET}")
    }
}

/// 会话短标识：`123:456`（话题）→ `#456`，私聊 `1234567890` → `#7890`。
pub fn chat(key: &str) -> String {
    if let Some((_, thread)) = key.split_once(':') {
        return format!("#{thread}");
    }
    let chars: Vec<char> = key.chars().collect();
    let tail: String = chars[chars.len().saturating_sub(4)..].iter().collect();
    format!("#{tail}")
}

/// 截断长文本，用于日志里预览用户输入 / 工具参数 / 工具结果。
pub fn preview(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out.replace('\n', "⏎")
}
