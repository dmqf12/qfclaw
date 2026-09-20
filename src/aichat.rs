use std::fs;

use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::telegram::*;
use crate::toolcall::*;


fn extract_chat_result(chat_result: &Value) -> (String, String, Value, u64) {
    qffunc::print_json(chat_result);
    let content = chat_result["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default();
    let reasoning = chat_result["choices"][0]["message"]["reasoning_content"]
        .as_str()
        .unwrap_or_default();
    // 直接取出 Value，如果是 None 则创建新的空数组
    let tool_calls = chat_result["choices"][0]["message"]
        .get("tool_calls").cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let total_tokens = chat_result["usage"]["total_tokens"].as_u64().unwrap_or(0);
    (
        content.to_string(),
        reasoning.to_string(),
        tool_calls,
        total_tokens,
    )
}

struct ChatRequest {
    base_url: String,
    api_key: String,
    model: String,
    stream: String,
    thinking: String,
    messages: Vec<Value>,
    tools: Value,
    tool_choice: String,
    max_tokens: u32
}

impl ChatRequest {
    async fn send(&self) -> Result<Value> {
        let payload = json!({
            "model": &self.model,
            "stream": &self.stream,
            "thinking": {"type": &self.thinking},
            "messages": &self.messages,
            "tools": &self.tools,
            "tool_choice": &self.tool_choice,
            "max_tokens": &self.max_tokens
        });
        let client = Client::new();
        let url = format!("{}/chat/completions", &self.base_url);
        let response = client
            .post(url)
            .header("Authorization", format!("Bearer {}", &self.api_key))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let error_json: Value = response.json().await.context("请求失败")?;
            qffunc::print_json(&error_json);
            let error_message = error_json["error"]["message"]
                .as_str()
                .unwrap_or("请求失败");
            return Err(anyhow!("API 请求失败: {}", error_message));
        }

        let chat_response: Value = response.json().await?;
        Ok(chat_response)
    }
}

fn build_system_prompt() -> String {
    let path = "config";
    let files = ["SOUL.md", "Agent.md", "User.md"];
    files
        .iter()
        .filter_map(|f| fs::read_to_string(format!("{}/{}", path, f)).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

fn chat_init(chat_id: i64, user_input: &str) -> Result<ChatRequest> {
    println!("{}", user_input);
    if user_input.is_empty() {
        return Err(anyhow!("空消息"));
    }
    //  检查对话记录
    let messages_path = "messages/";
    fs::create_dir_all(messages_path)?; // 已存在时不会报错
    let msg_file = format!("{}/{}_messages.json", messages_path, chat_id);
    let mut messages: Vec<Value> =  match qffunc::file_to_json(&msg_file) {
        Ok(resp) => resp.as_array().cloned().unwrap_or_default(),
        Err(_) => vec![],
    };

    let system_prompt = build_system_prompt();
    messages.insert(0, json!({"role": "system", "content": &system_prompt}));

    //  获取模型配置信息
    let mut model_config = json!(null);
    let mut model: Value = json!(null);
    match qffunc::file_to_json("config/models.json") {
        Ok(resp) => {
            let model_name = resp["use_model"].as_str().unwrap_or_default();
            model_config = resp
                .get("config")
                .ok_or_else(|| anyhow!("models.json解析失败"))?
                .clone();
            model = resp
                .get(model_name)
                .ok_or_else(|| anyhow!("models.json解析失败"))?
                .clone();
        }
        Err(e) => return Err(anyhow!("model.json解析失败: {}", e)),
    };
    if model.is_null() {
        return Err(anyhow!("模型信息不存在"));
    }

    let api_key = model["token"].as_str().unwrap_or_default();
    let base_url = model["base_url"]
        .as_str()
        .unwrap_or("https://api.deepseek.com/v1");
    let model_name = model["model_name"].as_str().unwrap_or_default();
    let stream: bool = model_config["stream"].as_bool().unwrap_or(true);
    let reasoning = model_config["reasoning"].as_str().unwrap_or("disabled");
    messages.push(json!({"role": "user", "content": user_input}));

    let func: serde_json::Value = fs::File::open("config/tool_call.json")
        .ok()
        .and_then(|file| serde_json::from_reader(file).ok())
        .unwrap();
    //  发送并保存模型输出
    Ok(ChatRequest {
        model: model_name.to_string(),
        stream: stream.to_string(),
        thinking: reasoning.to_string(),
        messages: messages,
        tools: func,
        tool_choice: "auto".to_string(),
        max_tokens: 20480,
        api_key: api_key.to_string(),
        base_url: base_url.to_string()
    })
}

fn 记录token(chat_id: i64, total_tokens: u64) -> Result<()> {
    let session_file = &format!("messages/{}_session.json", chat_id);
    let mut session: Value = fs::File::open(session_file)
        .ok()
        .and_then(|file| serde_json::from_reader(file).ok())
        .unwrap_or_default();
    session["total_tokens"] = json!(total_tokens);
    serde_json::to_writer_pretty(fs::File::create(session_file)?, &session)?;
    Ok(())
}
fn 保存消息(message_name: i64, messages: &[Value]) -> Result<()> {
    let msg_file = format!("messages/{message_name}_messages.json");
    serde_json::to_writer_pretty(
        fs::File::create(format!("{}.tmp", msg_file))?,
        &messages[1..],
    )?;
    fs::rename(format!("{}.tmp", msg_file), &msg_file)?;
    Ok(())
}


pub async fn main(chat_id: i64, user_input: &str, mut rx: mpsc::Receiver<String>) -> Result<()> {
    let mut chat_request = chat_init(chat_id, user_input)?;

    // --- 创建 mpsc 通道用于发送 ToolRequest 给后台任务 ---
    let (tool_tx, tool_rx) = mpsc::channel::<ToolRequest>(32);
    tokio::spawn(toolcall(tool_rx)); // 后台任务持续运行，接收请求

    loop {
        _ = MsgBuilder::new("🧠思考中...").id(chat_id).clear().send().await;
        let reply: Value = match chat_request.send().await {
            Ok(resp) => resp,
            Err(e) => {
                let _ = MsgBuilder::new(&e.to_string()).id(chat_id).send().await;
                break;
            }
        };

        let (content, reasoning, tool_calls, total_tokens) = extract_chat_result(&reply);
        记录token(chat_id, total_tokens)?;

        if !reasoning.is_empty() {
            if false {
                let _msg_id = MsgBuilder::new(&format!("🧠Reasoning: {}", reasoning)).id(chat_id).fold().send().await;
            }
        }

        if !content.is_empty() {
            let _ = MsgBuilder::new(&content).id(chat_id).send().await;
        }
        let mut messages = chat_request.messages.clone();
        if let Ok(new_msg) = rx.try_recv() {
            if let Some(obj) = messages.last_mut().and_then(|m| m.as_object_mut()) {
                obj.remove("tool_calls");
            }
            messages.push(json!({"role": "user", "content": new_msg}));
            println!("打断❓");
            chat_request.messages = messages.clone();
            保存消息(chat_id, &messages)?;
            continue;
        }
        if let Some(arr) = tool_calls.as_array()
            && !arr.is_empty()
        {
            messages.push(json!({"role": "assistant", "content": content, "reasoning_content": reasoning, "tool_calls": tool_calls}));

            // --- 每次 tool_call 时创建一个 oneshot 通道用于接收反馈 ---
            let (tx_feedback, rx_feedback) = oneshot::channel::<Value>();

            let request = ToolRequest {
                payload: tool_calls,
                chat_id,
                resp_tx: tx_feedback, // 把 oneshot 的发送端传给任务
            };

            let mut tool_calls_result = json!([]);

            // 使用 mpsc 发送请求
            if tool_tx.send(request).await.is_ok() {
                // 等待 oneshot 反馈结果
                match rx_feedback.await {
                    Ok(feedback) => {
                        tool_calls_result = feedback;
                    }
                    Err(_) => println!("接收反馈失败（后台任务可能已崩溃或关闭）"),
                }
            }

            messages.extend(tool_calls_result.as_array().unwrap().clone());
            chat_request.messages = messages.clone();
            保存消息(chat_id, &messages)?;
        } else {
            messages.push(
                json!({"role": "assistant", "content": content, "reasoning_content": reasoning}),
            );
            chat_request.messages = messages.clone();
            保存消息(chat_id, &messages)?;
            break;
        }
    }
    Ok(())
}
