//! `ea-wasmapi`：Wasm 来宾使用的总线客户端（同步、单线程）。
//!
//! 目标 `wasm32-wasip1`，网络走 WasmEdge 的 WASI socket 扩展（`wasmedge_wasi_socket`，
//! 因为 `std::net` 在 wasip1 上是桩实现）。与原生侧 `ea-sysapi` 共享 `ea-proto` 的
//! 消息类型、接口形状一致；不做线程、TLS、文件系统。

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

use ea_proto::{
    method, ActionSpec, Envelope, JSONRPC_VERSION, Message, Notification, Request, Response, RpcError,
};
use serde_json::{json, Value};
use wasmedge_wasi_socket::TcpStream;

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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HistoryItem {
    pub ts: String,
    pub value: Value,
}

pub struct Client {
    name: String,
    stream: TcpStream,
    inbox: VecDeque<Incoming>,
    counter: u64,
    timeout: Duration,
}

impl Client {
    /// 连接总线（默认超时 5 s），不注册。
    pub fn connect(addr: &str) -> Result<Self, Error> {
        let stream = TcpStream::connect(addr)?;
        let _ = stream.set_nonblocking(true);
        Ok(Client {
            name: String::new(),
            stream,
            inbox: VecDeque::new(),
            counter: 0,
            timeout: Duration::from_secs(5),
        })
    }

    /// 注册到总线（发送 `sys.register` 并等待应答）。
    pub fn register(
        &mut self,
        name: &str,
        subscribe: &[&str],
        provide: &[&str],
        actions: &[ActionSpec],
    ) -> Result<(), Error> {
        self.name = name.to_string();
        let reg = json!({
            "name": name,
            "subscribe": subscribe,
            "provide": provide,
            "actions": actions,
        });
        let timeout = self.timeout;
        match self.call_timeout(method::SYS_REGISTER, reg, timeout) {
            Ok(v) if v.get("ok").and_then(|o| o.as_bool()) == Some(true) => Ok(()),
            Ok(v) => Err(Error::Register(format!("unexpected result: {v}"))),
            Err(Error::Rpc { code, message }) => Err(Error::Register(format!("{code}: {message}"))),
            Err(e) => Err(e),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// 调用操作并等待响应（默认超时）。
    pub fn call(&mut self, method: &str, data: Value) -> Result<Value, Error> {
        let timeout = self.timeout;
        self.call_timeout(method, data, timeout)
    }

    /// 调用操作并等待响应（指定超时）。等待期间收到的事件缓存到内部队列，`next()` 后续返回。
    pub fn call_timeout(&mut self, method: &str, data: Value, timeout: Duration) -> Result<Value, Error> {
        let id = ea_proto::make_id(&self.name, self.counter);
        self.counter += 1;
        let req = Message::Request(Request {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.clone(),
            method: method.to_string(),
            params: self.envelope(data),
        });
        self.write_line(&serde_json::to_string(&req)?)?;

        let deadline = Instant::now() + timeout;
        loop {
            match self.read_line(Some(deadline)) {
                Ok(Some(line)) => {
                    let msg: Message = serde_json::from_str(line.trim_end())?;
                    match msg {
                        Message::Response(resp) => {
                            if resp.id == id {
                                return match resp.error {
                                    Some(e) => Err(Error::Rpc { code: e.code, message: e.message }),
                                    None => Ok(resp.result.unwrap_or(Value::Null)),
                                };
                            }
                            // 过期响应（正常不该出现）：忽略
                        }
                        Message::Notification(n) => {
                            self.inbox.push_back(Incoming::Event { method: n.method, params: n.params });
                        }
                        Message::Request(r) => {
                            self.inbox.push_back(Incoming::Request { id: r.id, method: r.method, params: r.params });
                        }
                    }
                }
                Ok(None) => return Err(Error::Closed),
                Err(e) if e.kind() == ErrorKind::TimedOut => return Err(Error::Timeout(timeout)),
                Err(e) => return Err(Error::Io(e)),
            }
        }
    }

    /// 发送事件（通知）。
    pub fn emit(&mut self, method: &str, data: Value) -> Result<(), Error> {
        let n = Message::Notification(Notification {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.to_string(),
            params: self.envelope(data),
        });
        self.write_line(&serde_json::to_string(&n)?)
    }

    /// 阻塞读取下一条入站消息（事件或请求）。
    pub fn next(&mut self) -> Option<Incoming> {
        if let Some(m) = self.inbox.pop_front() {
            return Some(m);
        }
        loop {
            match self.read_line(None) {
                Ok(Some(line)) => {
                    let msg: Message = match serde_json::from_str(line.trim_end()) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    match msg {
                        Message::Notification(n) => {
                            return Some(Incoming::Event { method: n.method, params: n.params });
                        }
                        Message::Request(r) => {
                            return Some(Incoming::Request { id: r.id, method: r.method, params: r.params });
                        }
                        Message::Response(_) => continue, // 丢弃
                    }
                }
                Ok(None) | Err(_) => return None,
            }
        }
    }

    /// 阻塞读取下一条入站消息，超时返回 `None`（供非阻塞节拍轮询，如 task-thermostat）。
    pub fn next_timeout(&mut self, timeout: Duration) -> Option<Incoming> {
        if let Some(m) = self.inbox.pop_front() {
            return Some(m);
        }
        let deadline = Instant::now() + timeout;
        loop {
            match self.read_line(Some(deadline)) {
                Ok(Some(line)) => {
                    let msg: Message = match serde_json::from_str(line.trim_end()) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    match msg {
                        Message::Notification(n) => {
                            return Some(Incoming::Event { method: n.method, params: n.params });
                        }
                        Message::Request(r) => {
                            return Some(Incoming::Request { id: r.id, method: r.method, params: r.params });
                        }
                        Message::Response(_) => continue,
                    }
                }
                Ok(None) => return None,
                Err(e) if e.kind() == ErrorKind::TimedOut => return None,
                Err(_) => return None,
            }
        }
    }

    /// 回复一个请求。
    pub fn reply(&mut self, id: &str, result: Value) -> Result<(), Error> {
        let r = Message::Response(Response {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.to_string(),
            result: Some(result),
            error: None,
        });
        self.write_line(&serde_json::to_string(&r)?)
    }

    pub fn reply_error(&mut self, id: &str, code: i64, message: &str) -> Result<(), Error> {
        let r = Message::Response(Response {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.to_string(),
            result: None,
            error: Some(RpcError { code, message: message.to_string(), data: None }),
        });
        self.write_line(&serde_json::to_string(&r)?)
    }

    // —— 环境读写封装 ——

    pub fn env_get(&mut self, key: &str) -> Result<Option<Value>, Error> {
        let v = self.call(method::ENV_GET, json!({"key": key}))?;
        Ok(v.get("value").cloned().filter(|x| !x.is_null()))
    }

    pub fn env_set(&mut self, key: &str, value: Value, history: bool) -> Result<(), Error> {
        let v = self.call(method::ENV_SET, json!({"key": key, "value": value, "history": history}))?;
        if v.get("ok").and_then(|o| o.as_bool()) == Some(true) {
            Ok(())
        } else {
            Err(Error::Register(format!("env_set failed: {v}")))
        }
    }

    pub fn env_snapshot(&mut self, prefix: &str) -> Result<std::collections::HashMap<String, Value>, Error> {
        let v = self.call(method::ENV_SNAPSHOT, json!({"prefix": prefix}))?;
        Ok(v.get("values")
            .and_then(|x| serde_json::from_value(x.clone()).ok())
            .unwrap_or_default())
    }

    pub fn env_history(&mut self, key: &str, limit: usize) -> Result<Vec<HistoryItem>, Error> {
        let v = self.call(method::ENV_HISTORY, json!({"key": key, "limit": limit}))?;
        Ok(v.get("items")
            .and_then(|x| serde_json::from_value(x.clone()).ok())
            .unwrap_or_default())
    }

    // —— 内部 ——

    fn envelope(&self, data: Value) -> Envelope {
        Envelope {
            source: self.name.clone(),
            ts: now_ts(),
            data,
        }
    }

    fn write_line(&mut self, line: &str) -> Result<(), Error> {
        self.stream.write_all(line.as_bytes())?;
        self.stream.write_all(b"\n")?;
        self.stream.flush()?;
        Ok(())
    }

    /// 读取一行（到 `\n`）。`deadline` 为 `None` 时阻塞到有数据/EOF；
    /// 为 `Some` 时超时返回 `ErrorKind::TimedOut`。
    fn read_line(&mut self, deadline: Option<Instant>) -> std::io::Result<Option<String>> {
        let mut buf: Vec<u8> = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match self.stream.read(&mut byte) {
                Ok(0) => return Ok(None), // EOF
                Ok(_) => {
                    if byte[0] == b'\n' {
                        return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
                    }
                    buf.push(byte[0]);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if let Some(d) = deadline {
                        if Instant::now() >= d {
                            return Err(std::io::Error::new(ErrorKind::TimedOut, "deadline exceeded"));
                        }
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

fn now_ts() -> String {
    // wasm32-wasip1 下以 unix 秒作为时间戳兜底；ts 仅作观测用途，总线不校验格式。
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
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
