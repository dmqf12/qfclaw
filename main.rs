use sendmsg::*;
use serde_json::{Value, json};
use std::time::Duration;


fn get_msg(msg: &Value) -> (String, i64) {
    let text = msg["message"]["text"].as_str().unwrap_or("").to_string();
    let chat_id = msg["message"]["chat"]["id"].as_i64().unwrap_or(7398595453);
    (text, chat_id)
}

async fn handle_request(payload: Value) -> bool {
    let (user_input, chat_id) = get_msg(&payload);
    let allow_list: [i64; 1] = [7398595453];
    
    if !allow_list.contains(&chat_id) {
        let _ = SendMessage::new("❌您没有权限使用此机器人")
            .id(chat_id)
            .send()
            .await;
        return false;
    }
    
    if payload.get("callback_query").is_some() {
        if let Err(e) = deal_callback(&payload).await {
            println!("{}", e.to_string())
        }
        return true;
    }
    
    if user_input.is_empty() {
        return true;
    }

    if ["/new", "/clear", "/restart", "/retry", "/status", "/reasoning"]
        .iter()
        .any(|&cmd| user_input.as_str().contains(cmd))
    {
        tokio::spawn(async move {
            if let Err(_) = command::exec_cmd(&user_input, &payload).await {
                let _ = SendMessage::new("❌指令执行失败").send().await;
            }
        });
        return true;
    }

    tokio::spawn(async move {
        if let Err(e) = aichat::main(&user_input).await {
            let _ = SendMessage::new(&e.to_string()).send().await;
        }
    });
    true
}

async fn getupdates_receive() {
    let client = reqwest::Client::new();
    let mut last_update_id = 1;
    let timeout = 30;
    loop {
        let url = format!(
            "https://api.telegram.org/bot{}/getUpdates?limit=1&offset={}&timeout={}",
            *BOT_TOKEN, last_update_id, timeout
        );
        match client.get(&url).timeout(Duration::from_secs(35)).send().await {
            Ok(response) => match response.json::<Value>().await {
                Ok(json_response) => {
                    if let Some(results) = json_response["result"].as_array() {
                        if !results.is_empty() {
                            print_json(&json!(results));
                        } else {
                            println!("暂无消息");
                        }
                        for update in results {
                            if let Some(id) = update["update_id"].as_i64() {
                                last_update_id = id + 1;
                            }
                            print_json(&update);
                            let _ = handle_request(json!(update.clone())).await;
                        }
                    }
                }
                Err(e) => eprintln!("JSON parse error: {}", e),
            },
            Err(e) => eprintln!("Request error: {}", e),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::main]
async fn main() {
    let _ = SendMessage::new("✅启动成功").send().await;
    let _ = command::exec_cmd("/status", &Value::Null).await;
    getupdates_receive().await;
}