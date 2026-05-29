mod aichat;
mod toolcall;
mod command;
pub mod sendmsg;
use crate::sendmsg::*;
use serde_json::{Value};
use std::time::Duration;
use tokio::sync::mpsc;

//  fn deal_file(msg: Value)
fn get_msg(msg: &Value) -> (String, i64) {
    print_json(&msg);
    let text = msg["message"]["text"].as_str().unwrap_or("").to_string();
    let chat_id = msg["message"]["chat"]["id"].as_i64().unwrap_or(*ALLOW_ID);
    (text, chat_id)
}

async fn handle_msg(mut rx: mpsc::Receiver<Value>) {
    // 在循环外定义，以维持单任务状态
    let mut current_task: Option<(tokio::task::JoinHandle<()>, mpsc::Sender<String>)> = None;

    while let Some(payload) = rx.recv().await {
        let (user_input, chat_id) = get_msg(&payload);
        let allow_list: [i64; 1] = [*ALLOW_ID];

        if !allow_list.contains(&chat_id) {
            _ = SendMessage::new(&format!("不在白名单，您的id：\n      {}", chat_id))
                .id(chat_id)
                .send()
                .await;
            continue;
        }

        if payload.get("callback_query").is_some() {
            if let Err(e) = deal_callback(&payload).await {
                println!("{}", e.to_string())
            }
            continue;
        }

        if user_input.is_empty() {
            continue;
        }
        if user_input == "/stop" {
            if let Some((handle, _)) = current_task.take() {
                handle.abort();
                _ = SendMessage::new("🛑 任务已停止").send().await;
            } else {
                let msg_id = SendMessage::new("⚠️ 当前没有正在运行的任务").send().await;
                clear_up(((msg_id[0] - 1)..=msg_id[0]).collect(), 3);
            }
            continue;
        }

        if ["/new", "/clear", "/restart", "/status", "/reasoning"]
            .iter()
            .any(|&cmd| user_input.as_str().contains(cmd))
        {
            tokio::spawn(async move {
                if let Err(_) = command::exec_cmd(&user_input, &payload).await {
                    _ = SendMessage::new("❌指令执行失败").send().await;
                }
            });
            continue;
        }

        // --- 核心逻辑修改：管理单任务后台进程 ---

        // 1. 检查当前任务是否已运行结束（如果是，则重置）
        if let Some((handle, _)) = &current_task {
            if handle.is_finished() {
                current_task = None;
            }
        }

        // 2. 如果没有任务正在运行，则启动它
        if current_task.is_none() {
            let (tx, rx_chat) = mpsc::channel::<String>(32);
            let first_input = user_input.clone();
            let handle = tokio::spawn(async move {
                // 假设 aichat::main 现在接收 rx_chat
                if let Err(e) = aichat::main(&first_input, rx_chat).await {
                    _ = SendMessage::new(&e.to_string()).send().await;
                }
            });
            current_task = Some((handle, tx));
        } else if let Some((_, tx)) = &current_task {
            // 3. 如果任务已在运行，通过通道发送新消息
            let _ = tx.send(user_input).await;
        }
    }
}


async fn getupdates_receive(tx: mpsc::Sender<Value>) {
    let client = reqwest::Client::new();
    let mut last_update_id = 1;
    let timeout = 60;
    loop {
        let url = format!(
            "{}{}/getUpdates?limit=1&offset={}&timeout={}",
            *BOT_BASE_URL,
            *BOT_TOKEN, last_update_id, timeout
        );
        match client.get(&url).timeout(Duration::from_secs(65)).send().await {
            Ok(response) => match response.json::<Value>().await {
                Ok(json_response) => {
                    if let Some(results) = json_response["result"].as_array() {
                        if results.is_empty() {
                            println!("暂无消息");
                        }
                        for update in results {
                            if let Some(id) = update["update_id"].as_i64() {
                                last_update_id = id + 1;
                            }
                            //    print_json(&update);
                            let _ = tx.send(update.clone()).await;
                        }
                    }
                }
                Err(e) => eprintln!("JSON parse error: {}", e),
            },
            Err(e) => println!("Request error: {}", e.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::main]
async fn main() {
    _ = SendMessage::new("✅启动成功").send().await;
    let _ = command::exec_cmd("/status", &Value::Null).await;

    // 创建通道
    let (tx, rx) = mpsc::channel(32);

    // 启动后台任务
    let handle_updates = tokio::spawn(getupdates_receive(tx));
    let handle_messages = tokio::spawn(handle_msg(rx));

    // 等待任务完成（实际上会一直运行）
    let _ = tokio::join!(handle_updates, handle_messages);
}
