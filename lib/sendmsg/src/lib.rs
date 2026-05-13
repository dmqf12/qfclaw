use std::fs;
use std::time::Duration;

use once_cell::sync::Lazy;
use reqwest::Client;
use serde_json::{json, Value};
use anyhow::Result;
use regex::Regex;



fn escape_markdown_v2(cmd: &str) -> String {
    let specials = ['_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!', '\\', '$'];
    let mut escaped = String::new();
    for c in cmd.chars() {
        if specials.contains(&c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}
fn markdownv2_fold(text: &str) -> String {
    // 1. 先压缩多余换行
    let collapsed = Regex::new(r"\n{2,}")
        .unwrap()
        .replace_all(text, "\n")
        .to_string();
    
    // 2. 转义特殊字符（注意：不转义换行符）
    let escaped = escape_markdown_v2(&collapsed);
    
    // 3. 添加折叠标记（这些新加的 > 不需要转义，因为是格式控制字符）
    let fold_text = format!("**>{}||", &escaped.replace("\n", "\n>"));
    
    fold_text
}


pub fn print_json(value: &Value) {
    println!("{}", serde_json::to_string_pretty(value).unwrap())
}


pub static BOT_TOKEN: Lazy<String> = Lazy::new(|| {
    fs::read_to_string("config/bot.json")
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|json| json["token"].as_str().map(String::from))
        .expect("无法读取 config/bot.json 文件或找不到 'token' 字段")
});

pub static ALLOW_ID: Lazy<i64> = Lazy::new(|| {
    fs::read_to_string("config/bot.json")
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|json| json["allow_id"].as_i64())
        .unwrap_or(0)
});

pub static BOT_BASE_URL: Lazy<String> = Lazy::new(|| {
    fs::read_to_string("config/bot.json")
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|json| json["base_url"].as_str().map(String::from))
        .unwrap_or("https://api.telegram.org/bot".to_string())
});



fn get_callback_data(msg: &Value) -> (String, String, u64) {
    //  let chat_id = msg["callback_query"]["message"]["chat"]["id"].as_i64().unwrap_or(*ALLOW_ID);
    let data = msg["callback_query"]["data"]
        .as_str()
        .unwrap_or_default();
    let callback_id = msg["callback_query"]["id"]
        .as_str()
        .unwrap_or_default();
    let msg_id = msg["callback_query"]["message"]["message_id"]
        .as_u64()
        .unwrap_or(9999999);
    print_json(msg);
    (callback_id.to_string(), data.to_string(), msg_id)
}


async fn reply_callback(callback_id: &str) {
    let client = Client::new();
    let url = &format!("{}{}/answerCallbackQuery", *BOT_BASE_URL, *BOT_TOKEN);
    let body = json!({ "callback_query_id": callback_id});
    //    , "text": "操作成功", "show_alert": false});
    if let Ok(result) = client.post(url).json(&body).send().await {
         if let Ok(status) = result.json().await {
            print_json(&status);
        }
    }
}
pub async fn deal_callback(msg: &Value) -> Result<bool> {
    let (callback_id, text, msg_id) = get_callback_data(msg);
    if text.contains("reasoning") {
        if text.contains("set") {
            if text.contains("draft") {
                clear_up(msg_id, msg_id, 0);
                _ = send_inline("请选择推理模式", json!([
            [{"text": "开启", "callback_data": "reasoning_draft_enabled"}, {"text": "适应", "callback_data": "reasoning_draft_adaptive"}] ])).await;
            }
            if text.contains("fold") {
                clear_up(msg_id, msg_id, 0);
                _ = send_inline("请选择推理模式", json!([
            [{"text": "开启", "callback_data": "reasoning_fold_enabled"}, {"text": "适应", "callback_data": "reasoning_fold_adaptive"}] ])).await;
            }
            return Ok(true)
        }
        let models_file = "config/models.json";
        let mut models = fs::File::open(models_file)
            .ok()
            .and_then(|file| serde_json::from_reader(file).ok())
            .unwrap_or_else(|| json!({}));
        let content = text.chars().skip(10).collect::<String>();
        models["config"]["reasoning"] = json!(content);
        serde_json::to_writer_pretty(fs::File::create(models_file)?, &models)?;
        reply_callback(&callback_id).await;
        clear_up(msg_id, msg_id, 0);
        _ = SendMessage::new("✅操作成功").clear().send().await;
        
    }
    Ok(true)
}

pub async fn send_inline(text: &str, inline_keyboard: Value) -> Result<u64> {
    let client = Client::new();
    let mut msg_id = 99999999;
    let body = json!({
        "chat_id": *ALLOW_ID,
        "text": text,
        "reply_markup": { "inline_keyboard": inline_keyboard } });
    for _i in 1..3 {
        let result = client.post(&format!("{}{}/sendMessage", *BOT_BASE_URL, *BOT_TOKEN))
            .json(&body)
            .send()
            .await?;
        let status: Value = result.json().await?;
        let status_ok = status["ok"].as_bool().unwrap_or(false);
        if !status_ok {
            println!("{}", status);
            println!("重新发送❌❌");
        } else {
            println!("ok: true, result: true");
            msg_id = status["result"]["message_id"]
                .as_u64()
                .unwrap_or_default();
            break
        }
    }
    Ok(msg_id)
}

pub struct SendMessage {
    text: String,
    id: i64,
    parse_mode: String,
    draft: String,
    do_clear: bool
}

impl SendMessage {
    pub fn new(text: &str) -> Self {
        SendMessage {
            text: text.to_string(),
            id: *ALLOW_ID,
            parse_mode: String::from("Markdown"),
            draft: String::from(""),
            do_clear: false
        }
    }

    pub fn parse(mut self, parse_mode: &str) -> Self {
        self.parse_mode = parse_mode.to_string();
        self
    }
    pub fn id(mut self, chat_id: i64) -> Self {
        self.id = chat_id;
        self
    }
    pub fn is_draft(mut self) -> Self {
        self.draft = String::from("Draft");
        self
    }
    pub fn fold(mut self) -> Self {
        self.text = markdownv2_fold(&self.text);
        self.parse_mode = "MarkdownV2".to_string();
        self
    }
    pub fn clear(mut self) -> Self {
        self.do_clear = true;
        self
    }
    pub async fn send(mut self) -> Result<(u64, String)> {
        if self.text.is_empty() {
            println!("无法发送空消息❎");
            return Ok((99999999, String::new()))
        }
        if self.id == 0 {
            return Ok((99999999, String::new()))
        }
        let client = Client::new();
        let mut msg_id = 99999999;
        let mut msg_text = String::new();
        for _i in 1..3 {
            let mut body = json!({
                "chat_id": self.id,
                "text": self.text,
                "parse_mode": self.parse_mode
                });
            if !self.draft.is_empty() {
                body["draft_id"] = json!(1)
            }
            let result = client.post(&format!("{}{}/sendMessage{}", *BOT_BASE_URL, *BOT_TOKEN, self.draft))
                .json(&body)
                .send()
                .await?;
            let status: Value = result.json().await?;
            let status_ok = status["ok"].as_bool().unwrap_or(false);
            if !status_ok {
                println!("{}", status);
                println!("重新发送❌❌");
                if let Some(p) = status.get("description") {
                    if p.to_string().contains("parse") {
                        self.parse_mode = "".to_string();
                        //self.text = format!("{}\nMarkdown解析失败\n{}", self.text, p.to_string());
                        if !self.draft.is_empty() {
                            break
                        }
                    } else if p.to_string().contains("non-empty") {
                        break
                    } else {
                        self.text = format!("❌消息发送失败！\ndescription: {}", &p.to_string());
                        break
                    }
                }
            } else {
                println!("ok: true, result: true");
                msg_id = status["result"]["message_id"]
                    .as_u64()
                    .unwrap_or_default();
                if self.do_clear {
                    clear_up(msg_id, msg_id, 0);
                }
                msg_text = status["result"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                break
            }
        }
        return Ok((msg_id, msg_text));
    }
}




pub fn clear_up(start_id: u64, end_id: u64, delay_secs: u64) {
    tokio::spawn(async move {
        // 延迟 n 秒后开始删除
        tokio::time::sleep(Duration::from_secs(delay_secs)).await;

        let client = Client::new();
        let mut body = json!({"chat_id": *ALLOW_ID});

        for id in (start_id..=end_id).rev() {
            if id > 9990000 {
                break
            }
            body["message_id"] = json!(id);
            for attempt in 0..2 {
                match client
                    .post(&format!(
                        "{}{}/deleteMessage",
                        *BOT_BASE_URL,
                        *BOT_TOKEN
                    ))
                    .json(&body)
                    .send()
                    .await
                {
                    Ok(_) => break,
                    Err(e) if attempt == 1 => eprintln!("删除 {id} 失败: {e}"),
                    Err(e) => eprintln!("删除 {id} 重试 {}: {e}", attempt + 1),
                }
            }
        }
    });
}