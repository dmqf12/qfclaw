use std::fs;
use std::io::{BufRead, BufReader};
use std::process::Command;

use serde_json::{json, Value};
use reqwest::Client;
use chrono::Utc;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::Sha256;
use hmac::{KeyInit, Hmac, Mac};

use sendmsg::*;



async fn get_coin_price(coin: &str) -> String {
    let price = String::new();
    let coin = coin.to_uppercase();
    // 尝试 OKX
    let url = format!("https://www.okx.com/api/v5/public/mark-price?instId={}-USDT-SWAP", coin);
    if let Ok(resp) = reqwest::get(&url).await {
        if let Ok(result) = resp.json::<Value>().await {
            //  println!("{}", result);
            return result["data"][0]["markPx"]
                .as_str()
                .unwrap_or("")
                .to_string()
        }
    }
    // 尝试 MEXC Contract
    let url = format!("https://contract.mexc.com/api/v1/contract/ticker?symbol={}_USDT", coin);
    if let Ok(resp) =  reqwest::get(&url).await {
        if let Ok(result) = resp.json::<Value>().await {
            //  println!("{}", result);
            return result["data"]["lastPrice"]
                .as_f64()
                .unwrap_or_default()
                .to_string()
        }
    }
    price
}


async fn get_balance() -> f64 {
    // 读取 .data 文件中的前三行
    let file = match fs::File::open(".data") {
        Ok(f) => f,
        Err(_) => return 0.0,
    };
    let lines: Vec<String> = BufReader::new(file)
        .lines()
        .take(3)
        .filter_map(Result::ok)
        .map(|s| s.trim().to_string())
        .collect();
    if lines.len() < 3 {
        return 0.0;
    }
    let api_key = &lines[0];
    let secret_key = &lines[1];
    let passphrase = &lines[2];

    let path = "/api/v5/account/balance";
    let params = "?ccy=USDT";
    let url = format!("https://okx.dmqf.me{}{}", path, params);

    // 生成时间戳 (UTC, 毫秒)
    let now = Utc::now();
    let timestamp = format!("{}.{:03}Z", now.format("%Y-%m-%dT%H:%M:%S"), now.timestamp_subsec_millis());

    // 构造签名消息
    let message = format!("{}{}{}{}", timestamp, "GET", path, params);
    let mut mac = Hmac::<Sha256>::new_from_slice(secret_key.as_bytes()).unwrap();
    mac.update(message.as_bytes());
    let signature = BASE64.encode(mac.finalize().into_bytes());

    let client = Client::new();
    let response = client
        .get(&url)
        .header("OK-ACCESS-KEY", api_key)
        .header("OK-ACCESS-PASSPHRASE", passphrase)
        .header("OK-ACCESS-TIMESTAMP", timestamp)
        .header("OK-ACCESS-SIGN", signature)
        .send()
        .await;
    match response {
        Ok(resp) => {
            if let Ok(json) = resp.json::<Value>().await {
                if let Some(total_eq) = json["data"][0]["totalEq"].as_str() {
                    return total_eq.parse::<f64>().unwrap_or(0.0).mul_add(1000.0, 0.5).floor() / 1000.0;
                }
            }
            0.0
        }
        Err(_) => 0.0,
    }
}

async fn cex_info(text: &str) -> String {
    let mut result = String::new();
    for part in text.split_whitespace() {
        if part == "balance" {
            result = format!("{}\n{}: {}", result, part, get_balance().await.to_string() + " USD");
        } else {
            let price = get_coin_price(part).await;
            if !price.is_empty(){
                result = format!("{}\n{}: {}", result, part, price);
            } else {
                result = format!("{}\n{}: 未获取到价格", result, part);
            }
        }
    }
    result

}

async fn read(file: &str) -> String {
    println!("🔧读取：{}", file);
    let _ = SendMessage::new(&format!("🔧读取：{}", file)).parse("").send().await;


    let path = if file.starts_with('/') {
        file.to_string()
    } else {
        format!("workspace/{}", file)
    };

    match fs::read_to_string(&path) {
        Ok(s) => s.trim().to_string(),
        Err(e) => format!("无法读取文件 {}: {}", file, e),
    }
}

async fn write(file: &str, text: &str) -> String {
    println!("✏️写入：{}", file);
    let _ = SendMessage::new(&format!("✏️写入：{}", file)).parse("").send().await;
    let path = if file.starts_with('/') || file.starts_with('$') {
        file.to_string()
    } else {
        format!("workspace/{}", file)
    };

    match fs::write(&path, text) {
        Ok(_) => format!("写入成功: {}", file),
        Err(e) => format!("写入失败 {}: {}", file, e),
    }
}

async fn exec(text: &str) -> String {
    println!("⚡执行：{}", text);
    let _ = SendMessage::new(&format!("⚡执行：{}", text)).fold().send().await;
    let result = match Command::new("bash")
        .arg("-c")
        .arg(format!("cd workspace\n{}", text))
        .output()
    {
        Ok(r) => r,
        Err(_) => return "执行失败".to_string(),
    };
    let mut output = String::from_utf8_lossy(&result.stdout).to_string();
    let error = String::from_utf8_lossy(&result.stderr).to_string();
    if !error.is_empty() {
        output = format!("{}错误信息：\n{}", output, error);
    }
    output
}

async fn delete(file: &str) -> String {
    println!("🗑️删除：{}", file);
    let _ = SendMessage::new(&format!("🗑️删除：{}", file)).parse("").send().await;
    let path = if file.starts_with('/') {
        file.to_string()
    } else {
        format!("workspace/{}", file)
    };

    match fs::remove_file(&path) {
        Ok(_) => format!("✅删除成功: {}", file),
        Err(e) => format!("❌删除失败 {}: {}", file, e),
    }
}

fn split_input(input: &str) -> (i32, &str) {
    let mut lines = input.lines().filter(|l| !l.is_empty());
    match (lines.next(), lines.next()) {
        (Some(num), Some(text)) => (num.parse().unwrap_or(5), text),
        (Some(text), None) => (5, text),
        _ => (5, ""),
    }
}


fn just_exec(text: &str) -> String {
    let result = match Command::new("bash")
        .arg("-c")
        .arg(format!("cd workspace\n{}", text))
        .output()
    {
        Ok(r) => r,
        Err(_) => return "执行失败".to_string(),
    };
    let mut output = String::from_utf8_lossy(&result.stdout).to_string();
    let error = String::from_utf8_lossy(&result.stderr).to_string();
    if !error.is_empty() {
        output = format!("{}错误信息：\n{}", output, error);
    }
    output
}


async fn search(text: &str) -> String {
    let (num, content) = split_input(text);
    println!("🔍搜索：{} {}条", content, num);
    let _ = SendMessage::new(&format!("🔍搜索：{} {}条", content, num)).parse("").send().await;
    let result = just_exec(&format!("python3 skill/search.py \"{}\" {}", content, num));
    result
}


async fn tool_run(tool_name: &str, parame: &Value) -> String {
    if tool_name == "read" {
        let file = parame["file"]
            .as_str()
            .unwrap_or("");
        return read(file).await
    }
    if tool_name == "write" {
        let file = parame["file"]
            .as_str()
            .unwrap_or("");
        let text = parame["text"]
            .as_str()
            .unwrap_or("");
        return write(file, text).await
    }
    if tool_name == "exec" {
        let command = parame["command"]
            .as_str()
            .unwrap_or("");
        return exec(command).await
    }
    if tool_name == "delete" {
        let file = parame["file"]
            .as_str()
            .unwrap_or("");
        return delete(file).await
    }
    if tool_name == "search" {
        let text = parame["text"]
            .as_str()
            .unwrap_or("");
        return search(text).await
    }
    if tool_name == "cex_info" {
        let text = parame["text"]
            .as_str()
            .unwrap_or("");
        return cex_info(text).await
    }
    String::new()
}

pub async fn toolcall(func: Value) -> Value {
    println!("{}", func);
    let mut result = Vec::new();
    if let Some(array) = func.as_array() {
        for element in array {
            let name = element["function"]["name"]
                .as_str()
                .unwrap_or_default();
            let arg = element["function"]["arguments"]
                .as_str()
                .unwrap_or_default();
            let call_id = element["id"]
                .as_str()
                .unwrap_or_default();
            let v: Value = serde_json::from_str(&arg)
                .unwrap_or(json!({}));
            result.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": tool_run(&name, &v).await}));
        }
    }
    return json!(result)
}
