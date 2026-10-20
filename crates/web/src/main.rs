//! `web`：HTTP + WebSocket 对话后端（M1）。
//!
//! - `GET /` 返回单页前端；`GET /ws` 建立 WebSocket。
//! - 浏览器聊天 → `event.chat.message`；`action.chat.reply` → 推给浏览器。
//! - 订阅温度/进程/任务事件并实时推送到浏览器。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use ea_proto::{action, event, method, ActionSpec};
use ea_sysapi::{Client, Incoming};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::info;

const INDEX_HTML: &str = include_str!("../static/index.html");

type Sessions = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<String>>>>;

#[derive(Clone)]
struct WebState {
    client: Arc<Client>,
    sessions: Sessions,
    session_counter: Arc<AtomicU64>,
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
    let listen = parse_listen_arg();

    let client = Arc::new(
        Client::connect(
            &bus_addr,
            "web",
            &["event.temperature.*", "event.process.*", "event.task.*"],
            &[action::CHAT_REPLY],
            &[ActionSpec {
                method: action::CHAT_REPLY.to_string(),
                description: "向指定会话回复聊天消息".to_string(),
                params_schema: json!({
                    "type": "object",
                    "properties": {"session": {"type": "string"}, "text": {"type": "string"}}
                }),
            }],
        )
        .await?,
    );
    info!("web connected to bus {bus_addr}");

    let state = WebState {
        client: client.clone(),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        session_counter: Arc::new(AtomicU64::new(0)),
    };

    let bus_client = client.clone();
    let bus_sessions = state.sessions.clone();
    tokio::spawn(async move {
        bus_loop(bus_client, bus_sessions).await;
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/ws", get(ws_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    info!("web listening on {listen}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn ws_handler(ws: WebSocketUpgrade, State(st): State<WebState>) -> axum::response::Response {
    ws.on_upgrade(move |socket| handle_socket(socket, st))
}

async fn handle_socket(socket: WebSocket, st: WebState) {
    let session = format!("s{}", st.session_counter.fetch_add(1, Ordering::Relaxed));
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    st.sessions.lock().unwrap().insert(session.clone(), tx);

    // 建立连接即推送一次当前注册表
    if let Ok(v) = st.client.call(method::SYS_REGISTRY_LIST, json!({})).await {
        let processes = v.get("processes").cloned().unwrap_or_else(|| Value::Array(vec![]));
        push_to(&st.sessions, &session, json!({"type": "status", "processes": processes}).to_string());
    }

    let (mut sender, mut receiver) = socket.split();

    let send_task = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if sender.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Text(text) = msg {
            let v: Value = match serde_json::from_str(&text.to_string()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match v.get("type").and_then(|t| t.as_str()) {
                Some("chat") => {
                    let chat_text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                    st.client
                        .emit(event::CHAT_MESSAGE, json!({"session": session, "text": chat_text}))
                        .await
                        .ok();
                }
                Some("control") => {
                    handle_control(&st, &session, &v).await;
                }
                _ => {}
            }
        }
    }

    send_task.abort();
    st.sessions.lock().unwrap().remove(&session);
}

/// 处理浏览器发来的控制命令，映射到总线 action 并回传结果。
async fn handle_control(st: &WebState, session: &str, v: &Value) {
    let cmd = v.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
    let (m, data) = match cmd {
        "heater.on" => (action::HEATER_START, json!({})),
        "heater.off" => (action::HEATER_STOP, json!({})),
        "ambient.set" => {
            let value = v.get("value").and_then(|x| x.as_f64()).unwrap_or(0.0);
            (action::TEMPERATURE_SET_AMBIENT, json!({"value": value, "hold": true}))
        }
        "ambient.resume" => (action::TEMPERATURE_SET_AMBIENT, json!({"value": 0.0, "hold": false})),
        "temperature.read" => (action::TEMPERATURE_READ, json!({})),
        _ => return,
    };
    let payload = match st.client.call(m, data).await {
        Ok(result) => json!({"type": "control_result", "cmd": cmd, "ok": true, "result": result}),
        Err(e) => json!({"type": "control_result", "cmd": cmd, "ok": false, "error": e.to_string()}),
    };
    push_to(&st.sessions, session, payload.to_string());
}

async fn bus_loop(client: Arc<Client>, sessions: Sessions) {
    loop {
        match client.next().await {
            None => break,
            Some(Incoming::Event { method, params }) => {
                if let Some(msg) = event_to_browser(&method, &params.data) {
                    broadcast(&sessions, &msg.to_string());
                }
            }
            Some(Incoming::Request { id, method: m, params }) => {
                if m == action::CHAT_REPLY {
                    let session = params.data.get("session").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let text = params.data.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    push_to(&sessions, &session, json!({"type": "reply", "text": text}).to_string());
                    let _ = client.reply(&id, json!({"ok": true})).await;
                } else if m == method::SYS_PING {
                    let _ = client.reply(&id, json!({"pong": true, "uptime_s": 0})).await;
                } else {
                    let _ = client
                        .reply_error(&id, ea_proto::error::METHOD_NOT_FOUND, "unknown method")
                        .await;
                }
            }
        }
    }
}

fn event_to_browser(method: &str, data: &Value) -> Option<Value> {
    match method {
        event::TEMPERATURE_UPDATED => Some(json!({
            "type": "temperature",
            "value": data.get("value").cloned().unwrap_or(Value::Null),
            "heater_on": data.get("heater_on").cloned().unwrap_or(Value::Null),
            "state": data.get("state").cloned().unwrap_or(Value::Null),
        })),
        event::TEMPERATURE_ALARM => {
            let level = data.get("level").and_then(|v| v.as_str()).unwrap_or("");
            let value = data.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let cn = if level == "low" { "低温" } else { "高温" };
            Some(json!({
                "type": "alarm",
                "level": level,
                "value": value,
                "text": format!("⚠️ {cn}告警：当前 {value:.1} ℃"),
            }))
        }
        event::TEMPERATURE_ALARM_CLEARED => {
            let level = data.get("level").and_then(|v| v.as_str()).unwrap_or("");
            let cn = if level == "low" { "低温" } else { "高温" };
            Some(json!({
                "type": "alarm_cleared",
                "level": level,
                "text": format!("✅ {cn}告警解除"),
            }))
        }
        event::PROCESS_DOWN => Some(json!({
            "type": "status",
            "process": data.get("name").cloned().unwrap_or(Value::Null),
            "down": true,
        })),
        event::PROCESS_UP => Some(json!({
            "type": "status",
            "process": data.get("name").cloned().unwrap_or(Value::Null),
            "down": false,
        })),
        _ => None,
    }
}

fn broadcast(sessions: &Sessions, text: &str) {
    let sessions = sessions.lock().unwrap();
    for tx in sessions.values() {
        let _ = tx.send(text.to_string());
    }
}

fn push_to(sessions: &Sessions, session: &str, text: String) {
    let sessions = sessions.lock().unwrap();
    if session.is_empty() {
        for tx in sessions.values() {
            let _ = tx.send(text.clone());
        }
    } else if let Some(tx) = sessions.get(session) {
        let _ = tx.send(text);
    }
}

fn parse_listen_arg() -> String {
    let mut listen = "0.0.0.0:8080".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--listen" {
            if let Some(v) = args.next() {
                listen = v;
            }
        }
    }
    listen
}
