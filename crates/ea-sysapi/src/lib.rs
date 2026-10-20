//! `ea-sysapi`：原生进程使用的总线客户端（异步）。
//!
//! 目标 `aarch64-unknown-linux-gnu`（及开发机本机目标），基于 tokio。
//! 与 Wasm 侧的 `ea-wasmapi` 共享 `ea-proto` 的消息类型，接口形状一致。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ea_proto::{
    method, ActionSpec, Envelope, JSONRPC_VERSION, Message, Notification, Request, Response, RpcError,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("bus returned error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    #[error("connection closed")]
    Closed,
    #[error("registration failed: {0}")]
    Register(String),
}

/// 入站消息：事件（通知）或发给本进程的请求（操作）。
#[derive(Debug)]
pub enum Incoming {
    Event { method: String, params: Envelope },
    Request { id: String, method: String, params: Envelope },
}

/// 环境历史记录项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryItem {
    pub ts: String,
    pub value: Value,
}

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Response>>>>;

pub struct Client {
    name: String,
    tx: mpsc::Sender<String>,
    rx: tokio::sync::Mutex<mpsc::Receiver<Incoming>>,
    pending: PendingMap,
    counter: AtomicU64,
    timeout: Duration,
}

impl Client {
    /// 连接总线并注册（默认超时 5 s）。
    pub async fn connect(
        addr: &str,
        name: &str,
        subscribe: &[&str],
        provide: &[&str],
        actions: &[ActionSpec],
    ) -> Result<Self, Error> {
        Self::connect_with_timeout(addr, name, subscribe, provide, actions, Duration::from_secs(5))
            .await
    }

    pub async fn connect_with_timeout(
        addr: &str,
        name: &str,
        subscribe: &[&str],
        provide: &[&str],
        actions: &[ActionSpec],
        timeout: Duration,
    ) -> Result<Self, Error> {
        let stream = TcpStream::connect(addr).await?;
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

        let (incoming_tx, rx) = mpsc::channel::<Incoming>(1024);
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_reader = pending.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let msg: Message = match serde_json::from_str(line.trim_end()) {
                            Ok(m) => m,
                            Err(_) => continue,
                        };
                        match msg {
                            Message::Response(resp) => {
                                if let Some(sender) = pending_reader.lock().await.remove(&resp.id) {
                                    let _ = sender.send(resp);
                                }
                            }
                            Message::Notification(n) => {
                                let _ = incoming_tx
                                    .send(Incoming::Event { method: n.method, params: n.params })
                                    .await;
                            }
                            Message::Request(r) => {
                                let _ = incoming_tx
                                    .send(Incoming::Request {
                                        id: r.id,
                                        method: r.method,
                                        params: r.params,
                                    })
                                    .await;
                            }
                        }
                    }
                }
            }
        });

        let client = Client {
            name: name.to_string(),
            tx,
            rx: tokio::sync::Mutex::new(rx),
            pending,
            counter: AtomicU64::new(0),
            timeout,
        };

        let reg = json!({
            "name": name,
            "subscribe": subscribe,
            "provide": provide,
            "actions": actions,
        });
        match client.call_timeout(method::SYS_REGISTER, reg, timeout).await {
            Ok(v) if v.get("ok").and_then(|o| o.as_bool()) == Some(true) => Ok(client),
            Ok(v) => Err(Error::Register(format!("unexpected result: {v}"))),
            Err(Error::Rpc { code, message }) => Err(Error::Register(format!("{code}: {message}"))),
            Err(e) => Err(e),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// 调用操作并等待响应（使用默认超时）。
    pub async fn call(&self, method: &str, data: Value) -> Result<Value, Error> {
        self.call_timeout(method, data, self.timeout).await
    }

    /// 调用操作并等待响应（指定超时）。
    pub async fn call_timeout(
        &self,
        method: &str,
        data: Value,
        timeout: Duration,
    ) -> Result<Value, Error> {
        let resp = self.request(method, data, timeout).await?;
        match resp.error {
            Some(err) => Err(Error::Rpc { code: err.code, message: err.message }),
            None => Ok(resp.result.unwrap_or(Value::Null)),
        }
    }

    /// 发送事件（通知）。
    pub async fn emit(&self, method: &str, data: Value) -> Result<(), Error> {
        let n = Message::Notification(Notification {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.to_string(),
            params: self.envelope(data),
        });
        self.send(&serde_json::to_string(&n)?).await
    }

    /// 阻塞读取下一条入站消息（事件或请求）。
    pub async fn next(&self) -> Option<Incoming> {
        self.rx.lock().await.recv().await
    }

    /// 回复一个请求。
    pub async fn reply(&self, id: &str, result: Value) -> Result<(), Error> {
        let r = Message::Response(Response {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.to_string(),
            result: Some(result),
            error: None,
        });
        self.send(&serde_json::to_string(&r)?).await
    }

    pub async fn reply_error(&self, id: &str, code: i64, message: &str) -> Result<(), Error> {
        let r = Message::Response(Response {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.to_string(),
            result: None,
            error: Some(RpcError { code, message: message.to_string(), data: None }),
        });
        self.send(&serde_json::to_string(&r)?).await
    }

    // —— 环境读写封装 ——

    pub async fn env_get(&self, key: &str) -> Result<Option<Value>, Error> {
        let v = self.call(method::ENV_GET, json!({"key": key})).await?;
        Ok(v.get("value").cloned().filter(|x| !x.is_null()))
    }

    pub async fn env_set(&self, key: &str, value: Value, history: bool) -> Result<(), Error> {
        let v = self
            .call(method::ENV_SET, json!({"key": key, "value": value, "history": history}))
            .await?;
        if v.get("ok").and_then(|o| o.as_bool()) == Some(true) {
            Ok(())
        } else {
            Err(Error::Register(format!("env_set failed: {v}")))
        }
    }

    pub async fn env_snapshot(&self, prefix: &str) -> Result<HashMap<String, Value>, Error> {
        let v = self.call(method::ENV_SNAPSHOT, json!({"prefix": prefix})).await?;
        Ok(v.get("values")
            .and_then(|x| serde_json::from_value(x.clone()).ok())
            .unwrap_or_default())
    }

    pub async fn env_history(&self, key: &str, limit: usize) -> Result<Vec<HistoryItem>, Error> {
        let v = self.call(method::ENV_HISTORY, json!({"key": key, "limit": limit})).await?;
        Ok(v.get("items")
            .and_then(|x| serde_json::from_value(x.clone()).ok())
            .unwrap_or_default())
    }

    // —— 内部 ——

    async fn request(&self, method: &str, data: Value, timeout: Duration) -> Result<Response, Error> {
        let id = ea_proto::make_id(&self.name, self.counter.fetch_add(1, Ordering::Relaxed));
        let req = Message::Request(Request {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.clone(),
            method: method.to_string(),
            params: self.envelope(data),
        });
        let (otx, orx) = oneshot::channel::<Response>();
        self.pending.lock().await.insert(id.clone(), otx);
        self.send(&serde_json::to_string(&req)?).await?;
        match tokio::time::timeout(timeout, orx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => Err(Error::Closed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(Error::Timeout(timeout))
            }
        }
    }

    fn envelope(&self, data: Value) -> Envelope {
        Envelope {
            source: self.name.clone(),
            ts: chrono::Utc::now().to_rfc3339(),
            data,
        }
    }

    async fn send(&self, line: &str) -> Result<(), Error> {
        self.tx.send(line.to_string()).await.map_err(|_| Error::Closed)
    }
}

/// 从命令行 `--bus <addr>` 解析总线地址，默认 `127.0.0.1:7700`。
pub fn parse_bus_arg() -> String {
    let mut bus = "127.0.0.1:7700".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--bus" {
            if let Some(v) = args.next() {
                bus = v;
            }
        }
    }
    bus
}
