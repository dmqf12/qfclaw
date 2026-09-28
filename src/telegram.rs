use std::fs;
use std::time::Duration;

use anyhow::{ bail, Result };
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Client;
use serde_json::{Value, json};

pub fn escape_markdown_v2(cmd: &str) -> String {
    let specials = [
        '_', '*', '[', ']', '(', ')', '~', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!',
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


pub static BOT_TOKEN: Lazy<String> = Lazy::new(|| {
    fs::read_to_string("config/bot.json")
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|json| json["token"].as_str().map(String::from))
        .expect("无法读取 config/bot.json 文件或找不到 'token' 字段")
});

pub static ALLOW_USER_ID: Lazy<i64> = Lazy::new(|| {
    fs::read_to_string("config/bot.json")
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|json| json["allow_user_id"].as_i64())
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
    //  let chat_id = msg["callback_query"]["message"]["chat"]["id"].as_i64().unwrap_or(*ALLOW_USER_ID);
    let data = msg["callback_query"]["data"].as_str().unwrap_or_default();
    let callback_id = msg["callback_query"]["id"].as_str().unwrap_or_default();
    let msg_id = msg["callback_query"]["message"]["message_id"]
        .as_u64()
        .unwrap_or(9999999);
    qffunc::print_json(msg);
    (callback_id.to_string(), data.to_string(), msg_id)
}

async fn reply_callback(callback_id: &str) {
    let client = Client::new();
    let url = &format!("{}{}/answerCallbackQuery", *BOT_BASE_URL, *BOT_TOKEN);
    let body = json!({ "callback_query_id": callback_id});
    //    , "text": "操作成功", "show_alert": false});
    _ = client.post(url).json(&body).send().await;
}
pub async fn deal_callback(chat_id: i64, msg: &Value) -> Result<bool> {
    let (callback_id, text, msg_id) = get_callback_data(msg);
    if text.contains("reasoning") {
        if text.contains("set") {
            if text.contains("draft") {
                delete_msg(chat_id, vec!(msg_id), 0, true);
                _ = send_inline(chat_id, "请选择推理模式", json!([
            [{"text": "开启", "callback_data": "reasoning_draft_enabled"}, {"text": "适应", "callback_data": "reasoning_draft_adaptive"}] ])).await;
            }
            if text.contains("fold") {
                delete_msg(chat_id, vec!(msg_id), 0, true);
                _ = send_inline(chat_id, "请选择推理模式", json!([
            [{"text": "开启", "callback_data": "reasoning_fold_enabled"}, {"text": "适应", "callback_data": "reasoning_fold_adaptive"}] ])).await;
            }
            return Ok(true);
        }
        let models_file = "config/models.json";
        let mut models = fs::File::open(models_file)
            .ok()
            .and_then(|file| serde_json::from_reader(file).ok())
            .unwrap_or_else(|| json!({}));
        let params: Vec<&str> = text.split("_").collect();
        models["config"]["reasoning"] = json!(&params[1]);
        models["config"]["show_reasoning"] = json!(&params[2]);
        serde_json::to_writer_pretty(fs::File::create(models_file)?, &models)?;
        reply_callback(&callback_id).await;
        delete_msg(chat_id, vec!(msg_id), 0, true);
        _ = MsgBuilder::new("✅操作成功").send().await?.delete(3);
    }
    Ok(true)
}

pub async fn send_inline(chat_id: i64, text: &str, inline_keyboard: Value) -> Result<u64> {
    let client = Client::new();
    let mut msg_id = 9999999;
    let body = json!({
        "chat_id": chat_id,
        "text": text,
        "reply_markup": { "inline_keyboard": inline_keyboard } });
    for _i in 1..3 {
        let result = client
            .post(format!("{}{}/SendMessage", *BOT_BASE_URL, *BOT_TOKEN))
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


// fn unicode_slice(s: &str, start: usize, end: usize) -> String {
//     let mut indices = s.char_indices();
//     let start_byte = indices.nth(start).map(|(i, _)| i).unwrap_or(s.len());
//     let end_byte = indices.nth(end - start - 1).map(|(i, _)| i).unwrap_or(s.len());
//     s[start_byte..end_byte].to_string()
// }



pub struct MsgBuilder {
    pub text: String,
    pub chat_id: i64,
    pub parse_mode: String,
    pub msg_id: Option<u64>
}

impl MsgBuilder {
    pub fn new(text: &str) -> Self {
        MsgBuilder {
            text: text.to_string(),
            chat_id: *ALLOW_USER_ID,
            parse_mode: String::from("Markdown"),
            msg_id: None,
        }
    }

    pub fn id(mut self, chat_id: i64) -> Self {
        self.chat_id = chat_id;
        self
    }
    pub fn fold(mut self) -> Self {
        self.text = markdownv2_fold(&self.text);
        self.parse_mode = "MarkdownV2".to_string();
        self
    }
    pub fn delete(self, delay: u64) -> Result<()> {
        if let Some(msg_id) = self.msg_id {
            delete_msg(self.chat_id, vec![msg_id], delay, true);
        }
        Ok(())
    }

    pub async fn send(mut self) -> Result<Self> {
        if self.text.is_empty() || self.chat_id == 0 {
            bail!("无法发送空消息")
        }
        let client = Client::new();
        let body = json!({
            "chat_id": self.chat_id,
            "text": self.text,
            "parse_mode": self.parse_mode,
        });
        let result = client.post(format!("{}{}/SendMessage",*BOT_BASE_URL, *BOT_TOKEN)).json(&body).send().await?;
        let response: Value = result.json().await?;
        _ = response["ok"].as_bool().unwrap();
        match response["result"]["message_id"].as_u64() {
            Some(id) => {
                self.msg_id = Some(id);
                Ok(self)
            },
            None => bail!("发送失败"),
        }
    }
}

pub fn delete_msg(chat_id: i64, ids: Vec<u64>, delay_secs: u64, should_save: bool) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(delay_secs)).await;

        let client = Client::new();
        let mut body = json!({"chat_id": chat_id});
        let mut session: Value = if should_save {
            fs::File::open(format!("messages/{}_session.json", chat_id))
                .ok()
                .and_then(|file| serde_json::from_reader(file).ok())
                .unwrap_or_default()
        } else {
            Value::Null
        };
        let mut cleared = if should_save {
            session["already_cleared"].as_array().unwrap_or(&vec![]).clone()
        } else {
            vec![]
        };

        for id in ids.iter().rev() {
            if *id > 9990000 {
                break;
            }
            if should_save && cleared.contains(&json!(id)) {
                continue;
            }
            if should_save {
                cleared.push(json!(id));
            }
            body["message_id"] = json!(id);
            for attempt in 0..2 {
                match client
                    .post(format!("{}{}/deleteMessage", *BOT_BASE_URL, *BOT_TOKEN))
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

        if should_save {
            session["already_cleared"] = json!(cleared);
            if let Ok(file) = fs::File::create(format!("messages/{}_session.json", chat_id)) {
                _ = serde_json::to_writer_pretty(file, &session);
            }
        }
    });
}
