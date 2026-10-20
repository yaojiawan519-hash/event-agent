//! `llm-gw`：LLM 网关（M3，原生）。
//!
//! 提供 `sys.llm.chat`，把运行时发来的消息/工具转发到 OpenAI 兼容端点
//! （默认 DeepSeek），解析返回的 `content` / `tool_calls` / `usage`。
//!
//! 配置读 `config/config.json` 的 `llm` 段（`api_url` / `api_key` / `model` 等）；
//! `--config <path>` 可覆盖（测试时指向 mock 端点）。

use std::time::Duration;

use ea_proto::{method, ActionSpec};
use ea_sysapi::{Client, Incoming};
use serde_json::{json, Value};
use tracing::info;

#[derive(Debug, Clone)]
struct LlmConfig {
    api_url: String,
    api_key: String,
    model: String,
    timeout_s: u64,
    temperature: f64,
    max_tokens: u32,
}

#[derive(Debug)]
enum ChatError {
    /// 可重试（429 / 5xx）。
    Retryable { status: u16 },
    /// 不可重试（网络/解析/其它 4xx）。
    Fatal(String),
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let bus_addr = ea_sysapi::parse_bus_arg();
    let config_path = parse_config_arg();
    let cfg = load_config(&config_path)?;

    let client = Client::connect(
        &bus_addr,
        "llm-gw",
        &[],
        &[method::SYS_LLM_CHAT],
        &[ActionSpec {
            method: method::SYS_LLM_CHAT.to_string(),
            description: "调用大模型推理（OpenAI 兼容）".to_string(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "messages": {"type": "array"},
                    "tools": {"type": "array"},
                    "temperature": {"type": "number"},
                    "max_tokens": {"type": "integer"}
                }
            }),
        }],
    )
    .await?;
    info!("llm-gw connected to {bus_addr}, model={}", cfg.model);

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(cfg.timeout_s))
        .build()?;

    loop {
        match client.next().await {
            None => break,
            Some(Incoming::Request { id, method: m, params }) => match m.as_str() {
                method::SYS_LLM_CHAT => handle_chat(&client, &http, &cfg, &id, &params.data).await,
                method::SYS_PING => {
                    let _ = client.reply(&id, json!({"pong": true})).await;
                }
                _ => {
                    let _ = client
                        .reply_error(&id, ea_proto::error::METHOD_NOT_FOUND, "unknown method")
                        .await;
                }
            },
            Some(Incoming::Event { .. }) => {}
        }
    }
    Ok(())
}

async fn handle_chat(
    client: &Client,
    http: &reqwest::Client,
    cfg: &LlmConfig,
    id: &str,
    data: &Value,
) {
    let messages = data.get("messages").cloned().unwrap_or_else(|| json!([]));
    let tools = data.get("tools").cloned().unwrap_or_else(|| json!([]));
    let temperature = data
        .get("temperature")
        .and_then(|v| v.as_f64())
        .unwrap_or(cfg.temperature);
    let max_tokens = data
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(cfg.max_tokens as u64) as u32;

    let body = json!({
        "model": cfg.model,
        "messages": messages,
        "tools": tools,
        "temperature": temperature,
        "max_tokens": max_tokens,
    });

    let mut last_err = String::new();
    for attempt in 0..3 {
        match post_chat(http, cfg, &body).await {
            Ok(result) => {
                // 只记录 token 用量，不记录消息正文。
                if let Some(u) = result.get("usage") {
                    info!(usage = %u, "llm chat usage");
                }
                let _ = client.reply(id, result).await;
                return;
            }
            Err(ChatError::Retryable { status }) => {
                last_err = format!("upstream {status}");
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_secs(2 * (attempt + 1) as u64)).await;
                }
            }
            Err(ChatError::Fatal(msg)) => {
                last_err = msg;
                break;
            }
        }
    }
    let _ = client
        .reply_error(
            id,
            ea_proto::error::LLM_UNAVAILABLE,
            &format!("llm unavailable: {last_err}"),
        )
        .await;
}

async fn post_chat(
    http: &reqwest::Client,
    cfg: &LlmConfig,
    body: &Value,
) -> Result<Value, ChatError> {
    let url = format!("{}/chat/completions", cfg.api_url.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .bearer_auth(&cfg.api_key)
        .json(body)
        .send()
        .await
        .map_err(|e| ChatError::Fatal(format!("request failed: {e}")))?;

    let status = resp.status();
    if status.as_u16() == 429 || status.is_server_error() {
        return Err(ChatError::Retryable { status: status.as_u16() });
    }
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(ChatError::Fatal(format!("upstream {status}: {text}")));
    }

    let v: Value = resp
        .json()
        .await
        .map_err(|e| ChatError::Fatal(format!("parse response: {e}")))?;
    Ok(parse_chat_response(v))
}

fn parse_chat_response(v: Value) -> Value {
    let message = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"));

    let content = message
        .and_then(|m| m.get("content"))
        .cloned()
        .unwrap_or(Value::Null);

    let tool_calls = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .map(|tc| {
                    let f = tc.get("function").unwrap_or(tc);
                    let arguments = f
                        .get("arguments")
                        .and_then(|a| a.as_str())
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .or_else(|| f.get("arguments").cloned())
                        .unwrap_or_else(|| json!({}));
                    json!({
                        "id": tc.get("id").cloned().unwrap_or(Value::Null),
                        "name": f.get("name").cloned().unwrap_or(Value::Null),
                        "arguments": arguments,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let usage = v.get("usage").cloned().unwrap_or(Value::Null);

    json!({"content": content, "tool_calls": tool_calls, "usage": usage})
}

fn load_config(path: &str) -> Result<LlmConfig, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(path)?;
    let v: Value = serde_json::from_str(&text)?;
    let llm = v.get("llm").cloned().unwrap_or(Value::Null);
    Ok(LlmConfig {
        api_url: llm
            .get("api_url")
            .and_then(|x| x.as_str())
            .unwrap_or("https://api.deepseek.com/v1")
            .to_string(),
        api_key: llm.get("api_key").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        model: llm
            .get("model")
            .and_then(|x| x.as_str())
            .unwrap_or("DeepSeek_V4_pro")
            .to_string(),
        timeout_s: llm.get("timeout_s").and_then(|x| x.as_u64()).unwrap_or(60),
        temperature: llm.get("temperature").and_then(|x| x.as_f64()).unwrap_or(0.2),
        max_tokens: llm.get("max_tokens").and_then(|x| x.as_u64()).unwrap_or(1024) as u32,
    })
}

fn parse_config_arg() -> String {
    let mut cfg = "config/config.json".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--config" {
            if let Some(v) = args.next() {
                cfg = v;
            }
        }
    }
    cfg
}
