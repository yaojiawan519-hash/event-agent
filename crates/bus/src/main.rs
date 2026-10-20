//! `bus`：消息总线 + 环境存储。
//!
//! 监听 TCP，NDJSON 帧；为每个注册进程维护有界入站队列；
//! 事件广播、操作单投、`env.*` 持久化到 SQLite。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ea_proto::{
    error, method, ActionSpec, Envelope, JSONRPC_VERSION, Message, Notification, Request, Response,
    RpcError,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const MAX_LINE: usize = 1024 * 1024;
const PENDING_TIMEOUT: Duration = Duration::from_secs(30);
const REGISTER_TIMEOUT: Duration = Duration::from_secs(5);

struct Process {
    name: String,
    subscribe: Vec<String>,
    provide: Vec<String>,
    actions: Vec<ActionSpec>,
    connected_at: String,
    tx: mpsc::Sender<String>,
}

struct PendingReq {
    requester: String,
    deadline: Instant,
}

struct State {
    processes: HashMap<String, Process>,
    providers: HashMap<String, String>, // provide 前缀 -> 进程名
    pending: HashMap<String, PendingReq>,
    env_kv: HashMap<String, Value>,
    db: rusqlite::Connection,
    started_at: Instant,
}

struct Config {
    listen: String,
    db: String,
}

fn parse_args() -> Config {
    let mut listen = "127.0.0.1:7700".to_string();
    let mut db = "/data/env.db".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--listen" => {
                if let Some(v) = args.next() {
                    listen = v;
                }
            }
            "--db" => {
                if let Some(v) = args.next() {
                    db = v;
                }
            }
            _ => {}
        }
    }
    Config { listen, db }
}

fn init_db(path: &str) -> Result<rusqlite::Connection, rusqlite::Error> {
    let conn = rusqlite::Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS env_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS env_hist (id INTEGER PRIMARY KEY, ts TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS event_log (id INTEGER PRIMARY KEY, ts TEXT NOT NULL, method TEXT NOT NULL, params TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS env_hist_key_ts ON env_hist(key, ts);",
    )?;
    Ok(conn)
}

fn load_env_kv(conn: &rusqlite::Connection) -> HashMap<String, Value> {
    let mut map = HashMap::new();
    let mut stmt = match conn.prepare("SELECT key, value FROM env_kv") {
        Ok(s) => s,
        Err(_) => return map,
    };
    let rows = stmt.query_map([], |row| {
        let key: String = row.get(0)?;
        let value: String = row.get(1)?;
        Ok((key, value))
    });
    if let Ok(rows) = rows {
        for r in rows.flatten() {
            if let Ok(v) = serde_json::from_str(&r.1) {
                map.insert(r.0, v);
            }
        }
    }
    map
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = parse_args();
    let conn = init_db(&cfg.db)?;
    let env_kv = load_env_kv(&conn);

    let state = Arc::new(Mutex::new(State {
        processes: HashMap::new(),
        providers: HashMap::new(),
        pending: HashMap::new(),
        env_kv,
        db: conn,
        started_at: Instant::now(),
    }));

    let sweep_state = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            sweep_pending(&sweep_state);
        }
    });

    let listener = TcpListener::bind(&cfg.listen).await?;
    info!("bus listening on {}", cfg.listen);
    loop {
        let (stream, addr) = listener.accept().await?;
        debug!("connection from {}", addr);
        let st = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(st, stream).await {
                warn!("connection {} closed: {}", addr, e);
            }
        });
    }
}

async fn handle_conn(
    state: Arc<Mutex<State>>,
    stream: TcpStream,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = stream.set_nodelay(true);
    let (reader, mut writer) = stream.into_split();

    let (tx, mut tx_rx) = mpsc::channel::<String>(1024);
    tokio::spawn(async move {
        while let Some(line) = tx_rx.recv().await {
            if writer.write_all(line.as_bytes()).await.is_err()
                || writer.write_all(b"\n").await.is_err()
                || writer.flush().await.is_err()
            {
                break;
            }
        }
    });

    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    // 第一条消息必须是 sys.register 请求，否则 5 s 后关闭连接。
    let first = tokio::time::timeout(REGISTER_TIMEOUT, reader.read_line(&mut line)).await;
    let req = match first {
        Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return Ok(()),
        Ok(Ok(_)) => {
            let msg: Message = match serde_json::from_str(line.trim_end()) {
                Ok(m) => m,
                Err(_) => return Ok(()),
            };
            match msg {
                Message::Request(r) if r.method == method::SYS_REGISTER => r,
                _ => return Ok(()),
            }
        }
    };

    let name = match do_register(&state, &tx, &req) {
        Ok(name) => name,
        Err(()) => return Ok(()),
    };
    debug!("process {name} registered");

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.len() > MAX_LINE {
            break;
        }
        let msg: Message = match serde_json::from_str(line.trim_end()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        dispatch(&state, &name, msg);
    }

    unregister(&state, &name);
    info!("process {name} disconnected");
    Ok(())
}

fn do_register(
    state: &Arc<Mutex<State>>,
    tx: &mpsc::Sender<String>,
    req: &Request,
) -> Result<String, ()> {
    let data = &req.params.data;
    let name = data.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let subscribe: Vec<String> = data
        .get("subscribe")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let provide: Vec<String> = data
        .get("provide")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let actions: Vec<ActionSpec> = data
        .get("actions")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let mut st = state.lock().unwrap();

    let fail = |msg: &str, code: i64| {
        let err = RpcError { code, message: msg.to_string(), data: None };
        send_line(tx, resp_line(&req.id, None, Some(err)));
    };

    if name.is_empty() {
        fail("name is required", error::INVALID_PARAMS);
        return Err(());
    }
    if st.processes.contains_key(&name) {
        fail(&format!("duplicate process name: {name}"), error::DUPLICATE_NAME);
        return Err(());
    }
    for p in &provide {
        if let Some(existing) = st.providers.get(p) {
            fail(
                &format!("provider prefix {p} already owned by {existing}"),
                error::CONFLICT_PROVIDER,
            );
            return Err(());
        }
    }
    for p in &provide {
        st.providers.insert(p.to_string(), name.clone());
    }
    let connected_at = chrono::Utc::now().to_rfc3339();
    st.processes.insert(
        name.clone(),
        Process {
            name: name.clone(),
            subscribe,
            provide,
            actions,
            connected_at,
            tx: tx.clone(),
        },
    );
    send_line(tx, resp_line(&req.id, Some(json!({"ok": true})), None));
    Ok(name)
}

fn dispatch(state: &Arc<Mutex<State>>, sender: &str, msg: Message) {
    match msg {
        Message::Request(req) => handle_request(state, sender, req),
        Message::Notification(n) => handle_notification(state, sender, n),
        Message::Response(resp) => handle_response(state, resp),
    }
}

fn handle_request(state: &Arc<Mutex<State>>, sender: &str, mut req: Request) {
    if req.params.source != sender {
        req.params.source = sender.to_string();
    }

    let mut st = state.lock().unwrap();

    match req.method.as_str() {
        method::SYS_REGISTER => {
            let err = RpcError {
                code: error::DUPLICATE_NAME,
                message: "already registered".to_string(),
                data: None,
            };
            reply_to(&st, sender, &req.id, None, Some(err));
        }
        method::SYS_PING => {
            let target = req.params.data.get("target").and_then(|v| v.as_str()).unwrap_or("");
            if target.is_empty() || target == "bus" {
                let uptime = st.started_at.elapsed().as_secs();
                reply_to(
                    &st,
                    sender,
                    &req.id,
                    Some(json!({"pong": true, "uptime_s": uptime})),
                    None,
                );
            } else if let Some(p) = st.processes.get(target) {
                let line = serde_json::to_string(&req).unwrap_or_default();
                send_line(&p.tx, line);
                st.pending.insert(
                    req.id.clone(),
                    PendingReq {
                        requester: sender.to_string(),
                        deadline: Instant::now() + PENDING_TIMEOUT,
                    },
                );
            } else {
                let err = RpcError {
                    code: error::METHOD_NOT_FOUND,
                    message: format!("no such process: {target}"),
                    data: None,
                };
                reply_to(&st, sender, &req.id, None, Some(err));
            }
        }
        method::SYS_REGISTRY_LIST => {
            let processes: Vec<Value> = st
                .processes
                .values()
                .map(|p| {
                    json!({
                        "name": p.name,
                        "subscribe": p.subscribe,
                        "provide": p.provide,
                        "connected_at": p.connected_at,
                    })
                })
                .collect();
            reply_to(&st, sender, &req.id, Some(json!({"processes": processes})), None);
        }
        method::SYS_REGISTRY_ACTIONS => {
            let actions: Vec<Value> = st
                .processes
                .values()
                .flat_map(|p| p.actions.iter())
                .map(|a| serde_json::to_value(a).unwrap_or(Value::Null))
                .collect();
            reply_to(&st, sender, &req.id, Some(json!({"actions": actions})), None);
        }
        method::ENV_GET => {
            let key = req.params.data.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let value = st.env_kv.get(key).cloned().unwrap_or(Value::Null);
            reply_to(&st, sender, &req.id, Some(json!({"value": value})), None);
        }
        method::ENV_SET => {
            let key = req.params.data.get("key").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let value = req.params.data.get("value").cloned().unwrap_or(Value::Null);
            let history = req.params.data.get("history").and_then(|v| v.as_bool()).unwrap_or(false);
            if key.is_empty() {
                let err = RpcError {
                    code: error::INVALID_PARAMS,
                    message: "key is required".to_string(),
                    data: None,
                };
                reply_to(&st, sender, &req.id, None, Some(err));
                return;
            }
            let now = chrono::Utc::now().to_rfc3339();
            let value_str = serde_json::to_string(&value).unwrap_or_default();
            st.env_kv.insert(key.clone(), value.clone());
            let _ = st.db.execute(
                "INSERT INTO env_kv(key, value, updated_at) VALUES(?1, ?2, ?3) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                rusqlite::params![key, value_str, now],
            );
            if history {
                let _ = st.db.execute(
                    "INSERT INTO env_hist(ts, key, value) VALUES(?1, ?2, ?3)",
                    rusqlite::params![now, key, value_str],
                );
            }
            reply_to(&st, sender, &req.id, Some(json!({"ok": true})), None);
            let line = notification_line(
                ea_proto::event::ENV_CHANGED,
                "bus",
                &now,
                json!({"key": key, "value": value}),
            );
            broadcast_event(&st, sender, ea_proto::event::ENV_CHANGED, &line);
        }
        method::ENV_SNAPSHOT => {
            let prefix = req.params.data.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
            let values: serde_json::Map<String, Value> = st
                .env_kv
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            reply_to(&st, sender, &req.id, Some(json!({"values": Value::Object(values)})), None);
        }
        method::ENV_HISTORY => {
            let key = req.params.data.get("key").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let limit =
                req.params.data.get("limit").and_then(|v| v.as_u64()).unwrap_or(100).min(1000) as i64;
            let mut items: Vec<Value> = Vec::new();
            {
                let stmt = st
                    .db
                    .prepare("SELECT ts, value FROM env_hist WHERE key = ?1 ORDER BY id DESC LIMIT ?2");
                if let Ok(mut stmt) = stmt {
                    let rows = stmt.query_map(rusqlite::params![key, limit], |row| {
                        let ts: String = row.get(0)?;
                        let value: String = row.get(1)?;
                        Ok((ts, value))
                    });
                    if let Ok(rows) = rows {
                        for r in rows.flatten() {
                            let value =
                                serde_json::from_str::<Value>(&r.1).unwrap_or(Value::String(r.1));
                            items.push(json!({"ts": r.0, "value": value}));
                        }
                    }
                }
            }
            reply_to(&st, sender, &req.id, Some(json!({"items": items})), None);
        }
        _ => {
            // 路由到提供者（唯一）
            let provider = st
                .providers
                .iter()
                .find(|(prefix, _)| ea_proto::prefix_matches(prefix, &req.method))
                .map(|(_, name)| name.clone());
            match provider {
                None => {
                    let err = RpcError {
                        code: error::METHOD_NOT_FOUND,
                        message: format!("no provider for {}", req.method),
                        data: None,
                    };
                    reply_to(&st, sender, &req.id, None, Some(err));
                }
                Some(provider_name) => {
                    if let Some(p) = st.processes.get(&provider_name) {
                        let line = serde_json::to_string(&req).unwrap_or_default();
                        send_line(&p.tx, line);
                        st.pending.insert(
                            req.id.clone(),
                            PendingReq {
                                requester: sender.to_string(),
                                deadline: Instant::now() + PENDING_TIMEOUT,
                            },
                        );
                    } else {
                        let err = RpcError {
                            code: error::METHOD_NOT_FOUND,
                            message: format!("provider {provider_name} gone"),
                            data: None,
                        };
                        reply_to(&st, sender, &req.id, None, Some(err));
                    }
                }
            }
        }
    }
}

fn handle_notification(state: &Arc<Mutex<State>>, sender: &str, mut n: Notification) {
    if n.params.source != sender {
        n.params.source = sender.to_string();
    }
    let method_name = n.method.clone();
    let st = state.lock().unwrap();
    if method_name.starts_with("event.") && method_name != ea_proto::event::TEMPERATURE_UPDATED {
        let now = chrono::Utc::now().to_rfc3339();
        let _ = st.db.execute(
            "INSERT INTO event_log(ts, method, params) VALUES(?1, ?2, ?3)",
            rusqlite::params![now, method_name, serde_json::to_string(&n.params).unwrap_or_default()],
        );
    }
    let line = serde_json::to_string(&n).unwrap_or_default();
    broadcast_event(&st, sender, &method_name, &line);
}

fn handle_response(state: &Arc<Mutex<State>>, resp: Response) {
    let mut st = state.lock().unwrap();
    if let Some(pending) = st.pending.remove(&resp.id) {
        if let Some(p) = st.processes.get(&pending.requester) {
            let line = serde_json::to_string(&resp).unwrap_or_default();
            send_line(&p.tx, line);
        }
    }
}

fn unregister(state: &Arc<Mutex<State>>, name: &str) {
    let mut st = state.lock().unwrap();
    if let Some(p) = st.processes.remove(name) {
        for prefix in p.provide {
            st.providers.remove(&prefix);
        }
    }
    let ids: Vec<String> = st
        .pending
        .iter()
        .filter(|(_, pr)| pr.requester == name)
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        st.pending.remove(&id);
    }
}

fn sweep_pending(state: &Arc<Mutex<State>>) {
    let mut st = state.lock().unwrap();
    let now = Instant::now();
    let expired: Vec<String> = st
        .pending
        .iter()
        .filter(|(_, pr)| pr.deadline <= now)
        .map(|(id, _)| id.clone())
        .collect();
    for id in expired {
        if let Some(pr) = st.pending.remove(&id) {
            let err = RpcError {
                code: error::TIMEOUT,
                message: "provider did not respond in time".to_string(),
                data: None,
            };
            reply_to(&st, &pr.requester, &id, None, Some(err));
        }
    }
}

fn broadcast_event(st: &State, sender: &str, method: &str, line: &str) {
    for p in st.processes.values() {
        if p.name == sender {
            continue;
        }
        if p.subscribe.iter().any(|pat| ea_proto::prefix_matches(pat, method)) {
            send_line(&p.tx, line.to_string());
        }
    }
}

fn reply_to(st: &State, name: &str, id: &str, result: Option<Value>, error: Option<RpcError>) {
    if let Some(p) = st.processes.get(name) {
        send_line(&p.tx, resp_line(id, result, error));
    }
}

fn resp_line(id: &str, result: Option<Value>, error: Option<RpcError>) -> String {
    serde_json::to_string(&Message::Response(Response {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id: id.to_string(),
        result,
        error,
    }))
    .unwrap_or_default()
}

fn notification_line(method: &str, source: &str, ts: &str, data: Value) -> String {
    serde_json::to_string(&Message::Notification(Notification {
        jsonrpc: JSONRPC_VERSION.to_string(),
        method: method.to_string(),
        params: Envelope { source: source.to_string(), ts: ts.to_string(), data },
    }))
    .unwrap_or_default()
}

fn send_line(tx: &mpsc::Sender<String>, line: String) {
    match tx.try_send(line.clone()) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            let tx2 = tx.clone();
            tokio::spawn(async move {
                let _ = tx2.send(line).await;
            });
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {}
    }
}
