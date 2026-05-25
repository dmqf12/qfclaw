use std::fs;

use anyhow::{Context, Result, anyhow};
use futures::StreamExt;
use rand;
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio::sync::{mpsc, oneshot};

use crate::sendmsg::*;
use crate::toolcall::*;

struct StreamProcessor {
    full_response: String,
    full_reasoning: String,
    start_response: bool,
    tool_calls_map: BTreeMap<u64, (String, String)>,
    usage: Option<Value>,
    show_reasoning_mode: String,
}

impl StreamProcessor {
    fn new() -> Self {
        Self {
            full_response: String::new(),
            full_reasoning: String::new(),
            start_response: false,
            tool_calls_map: BTreeMap::new(),
            usage: None,
            show_reasoning_mode: String::new(),
        }
    }

    async fn process_chunk(
        &mut self,
        chunk: &[u8],
        show_reasoning_mode: &str,
    ) -> Result<Option<Value>, anyhow::Error> {
        self.show_reasoning_mode = show_reasoning_mode.to_string();
        let text = String::from_utf8_lossy(chunk);
        for line in text.lines().map(|s| s.trim()) {
            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" {
                    return Ok(self.finalize().await);
                }
                let _ = self.process_data_line(data).await;
            }
        }
        Ok(None)
    }

    async fn process_data_line(&mut self, data: &str) -> Result<(), anyhow::Error> {
        let mut chunk: Value = json!({});
        match serde_json::from_str::<Value>(data) {
            Ok(resp) => chunk = resp,
            Err(e) => println!("{}---{}", e, data),
        }

        // 1. 记录 usage
        if let Some(usage) = chunk.get("usage") {
            self.usage = Some(usage.clone());
        }

        // 2. 提取 delta
        let delta = &chunk["choices"][0]["delta"];

        // 3. 处理 reasoning
        if let Some(reasoning) = delta["reasoning_content"].as_str() {
            self.full_reasoning.push_str(reasoning);
            if !self.start_response && self.full_reasoning.len() % 32 == 0 {
                tokio::spawn(
                    SendMessage::new(&format!("Reasoning 🧠\n{}", self.full_reasoning))
                        .is_draft()
                        .send(),
                );
            }
        }

        // 4. 处理 content
        if let Some(content) = delta["content"].as_str() {
            if !self.start_response {
                self.start_response = true;
                self.flush_reasoning().await;
            }
            self.full_response.push_str(content);
            if self.full_response.len() % 32 == 0 {
                tokio::spawn(SendMessage::new(&self.full_response).is_draft().send());
            }
        }

        // 5. 处理 tool_calls
        if let Some(tool_calls) = delta["tool_calls"].as_array() {
            for tc in tool_calls {
                if let Some(idx) = tc["index"].as_u64() {
                    let entry = self.tool_calls_map.entry(idx).or_default();
                    if let Some(function) = tc.get("function") {
                        if let Some(name) = function["name"].as_str() {
                            entry.0.push_str(name);
                        }
                        if let Some(args) = function["arguments"].as_str() {
                            entry.1.push_str(args);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn finalize(&mut self) -> Option<Value> {
        let mut message = json!({ "role": "assistant" });
        if !self.full_reasoning.is_empty() {
            message["reasoning_content"] = json!(self.full_reasoning);
        }
        if !self.full_response.is_empty() {
            message["content"] = json!(self.full_response);
        }

        if !self.tool_calls_map.is_empty() {
            message["tool_calls"] = json!(
                self.tool_calls_map
                    .iter()
                    .map(|(idx, (n, a))| {
                        json!({
                            "id": format!("call_{}_{}", idx, rand::random::<u32>()),
                            "type": "function",
                            "function": { "name": n, "arguments": a }
                        })
                    })
                    .collect::<Vec<_>>()
            );
        }

        Some(json!({
            "choices": [{ "finish_reason": "stop", "message": message }],
            "usage": self.usage.take().unwrap_or(json!({}))
        }))
    }

    async fn flush_reasoning(&self) {
        if !self.full_reasoning.is_empty() {
            let msg_send = SendMessage::new(&format!("Reasoning 🧠\n{}", self.full_reasoning));
            if self.show_reasoning_mode == "draft" {
                let msg_id = msg_send.send().await;
                clear_up(msg_id, 5);
            } else {
                _ = msg_send.fold().send().await;
            }
        }
    }
}

fn extract_chat_result(chat_result: &Value) -> (String, String, Value, u64) {
    print_json(chat_result);
    let content = chat_result["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default();
    let reasoning = chat_result["choices"][0]["message"]["reasoning_content"]
        .as_str()
        .unwrap_or_default();
    // 直接取出 Value，如果是 None 则创建新的空数组
    let tool_calls = chat_result["choices"][0]["message"]
        .get("tool_calls")
        .map(|v| v.clone())
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let total_tokens = chat_result["usage"]["total_tokens"].as_u64().unwrap_or(0);
    (
        content.to_string(),
        reasoning.to_string(),
        tool_calls,
        total_tokens,
    )
}

async fn chat(
    api_key: &str,
    base_url: &str,
    payload: &Value,
    show_reasoning_mode: &str,
) -> Result<serde_json::Value> {
    let client = Client::new();
    let url = format!("{}/chat/completions", base_url);
    let stream_out = payload["stream"].as_bool().unwrap_or(true);
    let response = client
        .post(url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await?;

    if !response.status().is_success() {
        let error_json: Value = response.json().await.context("请求失败")?;
        print_json(&error_json);
        let error_message = error_json["error"]["message"]
            .as_str()
            .unwrap_or("请求失败");
        return Err(anyhow!("API 请求失败: {}", error_message));
    }

    if !stream_out {
        let chat_response: Value = response.json().await?;
        return Ok(chat_response);
    } else {
        let mut processor = StreamProcessor::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("读取流失败")?;
            if let Some(final_value) = processor
                .process_chunk(&chunk, &show_reasoning_mode)
                .await?
            {
                return Ok(final_value);
            }
        }
        Err(anyhow!("流已结束但未收到完整响应"))
    }
}

fn build_system_prompt() -> String {
    let path = "workspace";
    let files = ["SOUL.md", "Agent.md", "SKILL.md", "User.md"];
    files
        .iter()
        .filter_map(|f| fs::read_to_string(format!("{}/{}", path, f)).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

fn init(user_input: &str) -> Result<(Value, Vec<Value>, String, String, String)> {
    println!("{}", user_input);
    if user_input.is_empty() {
        return Err(anyhow!("空消息"));
    }
    //  检查对话记录
    let messages_path = "messages/";
    fs::create_dir_all(&messages_path)?; // 已存在时不会报错
    let msg_file = format!("{}/messages.json", &messages_path);
    let mut messages: Vec<Value> = fs::File::open(&msg_file)
        .ok()
        .and_then(|file| serde_json::from_reader(file).ok())
        .unwrap_or_else(|| vec![]);

    let system_prompt = build_system_prompt();
    messages.insert(0, json!({"role": "system", "content": &system_prompt}));

    //  获取模型配置信息
    let mut model_config = json!(null);
    let mut model: Value = json!(null);
    if let Ok(model_file) = fs::File::open("config/models.json") {
        let model_list: Value = match serde_json::from_reader(model_file) {
            Ok(resp) => resp,
            Err(e) => return Err(anyhow!("model.json解析失败: {}", e)),
        };
        let model_name = model_list["use_model"].as_str().unwrap_or_default();
        model_config = model_list
            .get("config")
            .ok_or_else(|| anyhow!("models.json解析失败"))?
            .clone();
        model = model_list
            .get(model_name)
            .ok_or_else(|| anyhow!("models.json解析失败"))?
            .clone();
    }
    if model.is_null() {
        return Err(anyhow!("models.json解析失败或不存在"));
    }

    let api_key = model["token"].as_str().unwrap_or_default();
    let base_url = model["base_url"]
        .as_str()
        .unwrap_or("https://api.deepseek.com/v1");
    let model_name = model["model_name"].as_str().unwrap_or_default();
    let stream: bool = model_config["stream"].as_bool().unwrap_or(true);
    let mut show_reasoning_mode = model_config["reasoning"].as_str().unwrap_or("enabled");
    let mut reasoning = "";
    if show_reasoning_mode.contains("draft") {
        reasoning = show_reasoning_mode.split('_').last().unwrap();
        show_reasoning_mode = "draft";
    }
    if show_reasoning_mode.contains("fold") {
        reasoning = show_reasoning_mode.split('_').last().unwrap();
        show_reasoning_mode = "fold";
    }
    if reasoning.is_empty() {
        reasoning = show_reasoning_mode;
    }
    messages.push(json!({"role": "user", "content": user_input}));

    let func: serde_json::Value = fs::File::open("config/function_call.json")
        .ok()
        .and_then(|file| serde_json::from_reader(file).ok())
        .unwrap();
    //  发送并保存模型输出
    let payload = json!({
        "model": model_name,
        "stream": stream,
        "thinking": {"type":reasoning},
        "messages": messages,
        "tools": func,
        "tool_choice":  "auto",
        "max_tokens": 20480
    });
    return Ok((
        payload,
        messages,
        base_url.to_string(),
        api_key.to_string(),
        show_reasoning_mode.to_string(),
    ));
}

fn 记录token(total_tokens: u64) -> Result<()> {
    let session_file = "messages/session.json";
    let mut session: Value = fs::File::open(session_file)
        .ok()
        .and_then(|file| serde_json::from_reader(file).ok())
        .unwrap_or_default();
    session["total_tokens"] = json!(total_tokens);
    serde_json::to_writer_pretty(fs::File::create(session_file)?, &session)?;
    Ok(())
}
fn 保存消息(messages: &Vec<Value>) -> Result<()> {
    let msg_file = "messages/messages.json";
    serde_json::to_writer_pretty(
        fs::File::create(format!("{}.tmp", msg_file))?,
        &messages[1..],
    )?;
    fs::rename(format!("{}.tmp", msg_file), &msg_file)?;
    Ok(())
}

fn 截取消息(mut messages: Vec<Value>) -> Vec<Value> {
    if let Some(obj) = messages.last_mut().and_then(|m| m.as_object_mut()) {
        obj.remove("tool_calls");
    }
    messages
}

pub async fn main(user_input: &str, mut rx: mpsc::Receiver<String>) -> Result<()> {
    let (mut payload, mut messages, base_url, api_key, show_reasoning_mode) = init(user_input)?;

    // --- 创建 mpsc 通道用于发送 ToolRequest 给后台任务 ---
    let (tool_tx, tool_rx) = mpsc::channel::<ToolRequest>(32);
    tokio::spawn(toolcall(tool_rx)); // 后台任务持续运行，接收请求

    loop {
        let reply: Value = match chat(&api_key, &base_url, &payload, &show_reasoning_mode).await {
            Ok(resp) => resp,
            Err(e) => {
                let _ = SendMessage::new(&e.to_string()).send().await;
                break;
            }
        };

        let (content, reasoning, tool_calls, total_tokens) = extract_chat_result(&reply);
        记录token(total_tokens)?;

        if !content.is_empty() {
            let _ = SendMessage::new(&content).send().await;
        }

        if let Ok(new_msg) = rx.try_recv() {
            messages = 截取消息(messages);
            messages.push(json!({"role": "user", "content": new_msg}));
            println!("打断❓");
            payload["messages"] = Value::Array(messages.clone());
            保存消息(&messages)?;
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
            payload["messages"] = Value::Array(messages.clone());
            保存消息(&messages)?;
        } else {
            messages.push(
                json!({"role": "assistant", "content": content, "reasoning_content": reasoning}),
            );
            payload["messages"] = Value::Array(messages.clone());
            保存消息(&messages)?;
            break;
        }
    }
    Ok(())
}
