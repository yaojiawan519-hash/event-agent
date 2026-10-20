//! `ea-proto`：事件驱动 Agent 系统的协议唯一真源。
//!
//! 定义 JSON-RPC 2.0 消息类型、方法名常量、错误码与路由规则。
//! 本 crate 不进行任何 I/O，同时可用于原生目标与 wasm32-wasip1。

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";

/// 方法名常量。任何进程不得手写方法字符串（见仓库 CLAUDE.md）。
pub mod method {
    // —— 总线与系统服务 ——
    pub const SYS_REGISTER: &str = "sys.register";
    pub const SYS_PING: &str = "sys.ping";
    pub const SYS_REGISTRY_LIST: &str = "sys.registry.list";
    pub const SYS_REGISTRY_ACTIONS: &str = "sys.registry.actions";
    pub const SYS_LLM_CHAT: &str = "sys.llm.chat";

    // —— 环境读写 ——
    pub const ENV_GET: &str = "env.get";
    pub const ENV_SET: &str = "env.set";
    pub const ENV_SNAPSHOT: &str = "env.snapshot";
    pub const ENV_HISTORY: &str = "env.history";
}

/// 事件方法名（event.*）。
pub mod event {
    pub const TEMPERATURE_UPDATED: &str = "event.temperature.updated";
    pub const TEMPERATURE_ALARM: &str = "event.temperature.alarm";
    pub const TEMPERATURE_ALARM_CLEARED: &str = "event.temperature.alarm_cleared";
    pub const CHAT_MESSAGE: &str = "event.chat.message";
    pub const HEARTBEAT_TICK: &str = "event.heartbeat.tick";
    pub const PROCESS_DOWN: &str = "event.process.down";
    pub const PROCESS_UP: &str = "event.process.up";
    pub const ENV_CHANGED: &str = "event.env.changed";
    pub const TASK_STARTED: &str = "event.task.started";
    pub const TASK_STOPPED: &str = "event.task.stopped";
    pub const RUNTIME_DECISION: &str = "event.runtime.decision";
}

/// 操作方法名（action.*）。
pub mod action {
    pub const HEATER_START: &str = "action.heater.start";
    pub const HEATER_STOP: &str = "action.heater.stop";
    pub const TEMPERATURE_READ: &str = "action.temperature.read";
    pub const TEMPERATURE_SET_AMBIENT: &str = "action.temperature.set_ambient";
    pub const CHAT_REPLY: &str = "action.chat.reply";
    pub const TASK_START: &str = "action.task.start";
    pub const TASK_STOP: &str = "action.task.stop";
}

/// JSON-RPC 2.0 错误码。
pub mod error {
    // 标准错误码
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;

    // 本系统自定义错误码
    pub const DUPLICATE_NAME: i64 = -32000;
    pub const CONFLICT_PROVIDER: i64 = -32001;
    pub const TIMEOUT: i64 = -32002;
    pub const NOT_REGISTERED: i64 = -32003;
    pub const LLM_UNAVAILABLE: i64 = -32010;
}

/// `params` 统一信封。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Envelope {
    pub source: String,
    pub ts: String,
    pub data: Value,
}

impl Envelope {
    pub fn new(source: &str, ts: &str, data: Value) -> Self {
        Self { source: source.to_string(), ts: ts.to_string(), data }
    }
}

/// JSON-RPC 请求（操作）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub jsonrpc: String,
    pub id: String,
    pub method: String,
    pub params: Envelope,
}

/// JSON-RPC 通知（事件）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Envelope,
}

/// JSON-RPC 响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub jsonrpc: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// 一条总线消息：请求 / 通知 / 响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Message {
    Request(Request),
    Notification(Notification),
    Response(Response),
}

impl Message {
    pub fn id(&self) -> Option<&str> {
        match self {
            Message::Request(r) => Some(&r.id),
            Message::Response(r) => Some(&r.id),
            Message::Notification(_) => None,
        }
    }

    pub fn method(&self) -> Option<&str> {
        match self {
            Message::Request(r) => Some(&r.method),
            Message::Notification(n) => Some(&n.method),
            Message::Response(_) => None,
        }
    }
}

/// 操作说明，供 LLM 构造工具列表。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionSpec {
    pub method: String,
    pub description: String,
    pub params_schema: Value,
}

/// 前缀匹配：`event.*` 匹配 `event.temperature.alarm`；`action.echo` 精确匹配。
pub fn prefix_matches(pattern: &str, method: &str) -> bool {
    if let Some(base) = pattern.strip_suffix('*') {
        method.starts_with(base)
    } else {
        pattern == method
    }
}

/// 生成 `id`：`<进程名>-<单调递增整数>`。
pub fn make_id(name: &str, counter: u64) -> String {
    format!("{name}-{counter}")
}
