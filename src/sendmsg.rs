use std::fs;
use std::time::Duration;

use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Client;
use serde_json::{Value, json};

fn escape_markdown_v2(cmd: &str) -> String {
    let specials = [
        '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!',
        '\\', '$',
    ];
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
    let data = msg["callback_query"]["data"].as_str().unwrap_or_default();
    let callback_id = msg["callback_query"]["id"].as_str().unwrap_or_default();
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
    if let Ok(_) = client.post(url).json(&body).send().await {
        /*if let Ok(status) = result.json().await {
            print_json(&status);
        }  */
        return;
    }
}
pub async fn deal_callback(msg: &Value) -> Result<bool> {
    let (callback_id, text, msg_id) = get_callback_data(msg);
    if text.contains("reasoning") {
        if text.contains("set") {
            if text.contains("draft") {
                clear_up(vec!(msg_id), 0);
                _ = send_inline("请选择推理模式", json!([
            [{"text": "开启", "callback_data": "reasoning_draft_enabled"}, {"text": "适应", "callback_data": "reasoning_draft_adaptive"}] ])).await;
            }
            if text.contains("fold") {
                clear_up(vec!(msg_id), 0);
                _ = send_inline("请选择推理模式", json!([
            [{"text": "开启", "callback_data": "reasoning_fold_enabled"}, {"text": "适应", "callback_data": "reasoning_fold_adaptive"}] ])).await;
            }
            return Ok(true);
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
        clear_up(vec!(msg_id), 0);
        _ = SendMessage::new("✅操作成功").clear().send().await;
    }
    Ok(true)
}

pub async fn send_inline(text: &str, inline_keyboard: Value) -> Result<u64> {
    let client = Client::new();
    let mut msg_id = 9999999;
    let body = json!({
        "chat_id": *ALLOW_ID,
        "text": text,
        "reply_markup": { "inline_keyboard": inline_keyboard } });
    for _i in 1..3 {
        let result = client
            .post(&format!("{}{}/sendMessage", *BOT_BASE_URL, *BOT_TOKEN))
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
            msg_id = status["result"]["message_id"].as_u64().unwrap_or_default();
            break;
        }
    }
    Ok(msg_id)
}


fn unicode_slice(s: &str, start: usize, end: usize) -> String {
    let mut indices = s.char_indices();
    let start_byte = indices.nth(start).map(|(i, _)| i).unwrap_or(s.len());
    let end_byte = indices.nth(end - start - 1).map(|(i, _)| i).unwrap_or(s.len());
    s[start_byte..end_byte].to_string()
}



pub struct SendMessage {
    text: String,
    id: i64,
    parse_mode: String,
    draft: String,
    do_clear: bool,
}

impl SendMessage {
    pub fn new(text: &str) -> Self {
        SendMessage {
            text: text.to_string(),
            id: *ALLOW_ID,
            parse_mode: String::from("Markdown"),
            draft: String::from(""),
            do_clear: false,
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

    pub async fn send(mut self) -> Vec<u64> {
        let mut msg_id = vec![];
        if self.text.is_empty() || self.id == 0{
            return msg_id
        }
        let client = Client::new();
        let mut failed_times = 0;
        let mut new_text = self.text.clone();
        loop {
            if new_text.chars().count() > 4096 {
                if self.draft.is_empty() {
                    self.text = unicode_slice(&new_text, 0, 4096);
                    new_text = unicode_slice(&new_text, 4096, usize::MAX);
                } else {
                    self.text = unicode_slice(&new_text, new_text.chars().count() - 4096, usize::MAX);
                    new_text = "".to_string();
                }
            } else {
                self.text = new_text;
                new_text = "".to_string();
            }
            let mut body = json!({
            "chat_id": self.id,
            "text": self.text,
            "parse_mode": self.parse_mode
            });
            if !self.draft.is_empty() {
                body["draft_id"] = json!(1)
            }
            let mut status = json!( { "ok": false } );
            if let Ok(result) = client.post(&format!("{}{}/sendMessage{}",*BOT_BASE_URL, *BOT_TOKEN, self.draft)).json(&body).send().await {
                if let Ok(resp) = result.json().await {
                    status = resp;
                }
            } else {
                failed_times = failed_times + 1
            }
            if failed_times > 2 {
                return msg_id
            }
            let status_ok = status["ok"].as_bool().unwrap_or(false);
            if !status_ok {
                if let Some(p) = status.get("description") {
                    if p.to_string().contains("parse") {
                        self.parse_mode = "".to_string();
                        if !self.draft.is_empty() {
                            break;
                        }
                        println!("{}", status);
                        println!("重新发送❌❌");
                    } else if p.to_string().contains("empty") {
                        println!("无法发送空消息❎");
                        break
                    } else {
                        println!("{}", status);
                        println!(
                            "{}",
                            format!("❌消息发送失败！\ndescription: {}", &p.to_string())
                        );
                    }
                }
            } else {
                if !self.draft.is_empty() {
                    println!("ok......");
                } else {
                    println!("发送成功");
                }
                msg_id.push(status["result"]["message_id"].as_u64().unwrap_or_default());
                if self.do_clear {
                    clear_up(msg_id.clone(), 0);
                }
                if new_text.is_empty() {
                    break
                }
            }
        }
        if msg_id.is_empty() {
            return vec![9999999]
        } else {
            return msg_id
        }
    }
}

pub fn clear_up(ids: Vec<u64>, delay_secs: u64) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(delay_secs)).await;

        let client = Client::new();
        let mut body = json!({"chat_id": *ALLOW_ID});
        let mut session: Value = fs::File::open("messages/session.json")
            .ok()
            .and_then(|file| serde_json::from_reader(file).ok())
            .unwrap_or_default();
        let mut cleared = session["already_cleared"].as_array().unwrap_or(&vec![]).clone();

        for id in ids.iter().rev() {
            if *id > 9990000 {
                break;
            }
            if cleared.contains(&json!(id)) {
                continue;
            }
            cleared.push(json!(id));
            body["message_id"] = json!(id);
            for attempt in 0..2 {
                match client
                    .post(&format!("{}{}/deleteMessage", *BOT_BASE_URL, *BOT_TOKEN))
                    .json(&body)
                    .send()
                    .await
                {
                    Ok(_) => break,
                    Err(e) if attempt == 1 => eprintln!("删除 {} 失败: {e}", id),
                    Err(e) => eprintln!("删除 {} 重试 {}: {e}", id, attempt + 1),
                }
            }
        }

        session["already_cleared"] = json!(cleared);
        if let Ok(file) = fs::File::create("messages/session.json") {
            _ = serde_json::to_writer_pretty(file, &session);
        }
    });
}
