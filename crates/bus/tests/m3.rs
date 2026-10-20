//! M3 验收：LLM 网关 + 任务进程 + 运行时 LLM 推理。
//!
//! 依赖：`wasmedge`（跑 runtime.wasm / task-thermostat.wasm），以及已编译的
//! `llm-gw`（原生，`cargo build --workspace`）与 `hw-temp`。LLM 端点用一个进程内
//! mock（OpenAI 兼容 `/chat/completions`）替代，`llm-gw` 经 `--config` 指向它。
//!
//! 验收（docs/DESIGN.md §12）：
//! - `m3_chat_hold_temp`：chat「把温度保持在 25 度」→ 30 s 内 `action.chat.reply` +
//!   `task.hold_temp.enabled=true` + 温度进入 25±2 ℃。
//! - `m3_empty_rules_llm_fallback`：规则为空时，low 告警仍经 LLM 触发 `heater.start`。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ea_proto::{action, event, method};
use ea_sysapi::{Client, Incoming};
use serde_json::{json, Value};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn bin(name: &str) -> std::path::PathBuf {
    if name == "bus" {
        return env!("CARGO_BIN_EXE_bus").into();
    }
    for profile in ["debug", "release"] {
        let p = workspace_root().join("target").join(profile).join(name);
        if p.exists() {
            return p;
        }
    }
    panic!("binary {name} not found; run `cargo build --workspace` first (see scripts/smoke.sh)");
}

fn wasm(name: &str) -> std::path::PathBuf {
    let p = workspace_root().join(format!(
        "crates/{name}/target/wasm32-wasip1/release/{name}.wasm"
    ));
    assert!(
        p.exists(),
        "{name}.wasm not found; run `rustup run beta cargo build --target wasm32-wasip1 --release --manifest-path crates/{name}/Cargo.toml` (see scripts/smoke.sh)"
    );
    p
}

static TMP_SEQ: AtomicUsize = AtomicUsize::new(0);

fn temp_dir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "ea_m3_{}_{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_temp(name: &str, content: &str) -> std::path::PathBuf {
    let p = temp_dir().join(name);
    std::fs::write(&p, content).unwrap();
    p
}

fn spawn_bus(port: u16) -> ChildGuard {
    let db = std::env::temp_dir().join(format!("ea_m3_{}_{}.db", std::process::id(), port));
    let _ = std::fs::remove_file(&db);
    let child = Command::new(env!("CARGO_BIN_EXE_bus"))
        .arg("--listen")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--db")
        .arg(&db)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bus");
    ChildGuard(child)
}

fn spawn_native(name: &str, args: &[&str]) -> ChildGuard {
    let child = Command::new(bin(name))
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn native process");
    ChildGuard(child)
}

fn spawn_runtime(port: u16, rules: Option<&Path>) -> ChildGuard {
    let mut cmd = Command::new("wasmedge");
    if let Some(r) = rules {
        if let Some(parent) = r.parent() {
            cmd.arg("--dir").arg(parent);
        }
    }
    cmd.arg(wasm("runtime"));
    if let Some(r) = rules {
        cmd.arg("--rules").arg(r);
    }
    cmd.arg("--bus").arg(format!("127.0.0.1:{port}"));
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    ChildGuard(cmd.spawn().expect("spawn wasmedge runtime"))
}

fn spawn_task_thermostat(port: u16) -> ChildGuard {
    let child = Command::new("wasmedge")
        .arg(wasm("task-thermostat"))
        .arg("--bus")
        .arg(format!("127.0.0.1:{port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn wasmedge task-thermostat");
    ChildGuard(child)
}

fn spawn_llm_gw(port: u16, config: &Path) -> ChildGuard {
    let child = Command::new(bin("llm-gw"))
        .arg("--bus")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--config")
        .arg(config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn llm-gw");
    ChildGuard(child)
}

// —— mock OpenAI 兼容端点 ——

/// 依最后一条消息决定响应：`tool` 角色回最终内容；含「保持/恒温」回 `task.start`；
/// 含 `low` 回 `action.heater.start`；否则回空 content（不触发动作/回复）。
fn mock_response(body: &Value) -> Value {
    let last = body
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|a| a.last())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let role = last.get("role").and_then(|r| r.as_str()).unwrap_or("");
    let content = last.get("content").and_then(|c| c.as_str()).unwrap_or("");

    if role == "tool" {
        return json!({"choices":[{"message":{"role":"assistant","content":"已完成"}}],"usage":{"total_tokens":10}});
    }
    if content.contains("保持") || content.contains("恒温") {
        return json!({
            "choices":[{"message":{"role":"assistant","content":null,
                "tool_calls":[{"id":"call_1","type":"function",
                    "function":{"name":"task_start","arguments":"{\"task\":\"hold_temp\",\"target\":25.0,\"hysteresis\":1.0}"}}]}}],
            "usage":{"total_tokens":42}
        });
    }
    if content.contains("low") {
        return json!({
            "choices":[{"message":{"role":"assistant","content":null,
                "tool_calls":[{"id":"call_1","type":"function",
                    "function":{"name":"action_heater_start","arguments":"{}"}}]}}],
            "usage":{"total_tokens":42}
        });
    }
    json!({"choices":[{"message":{"role":"assistant","content":""}}],"usage":{"total_tokens":10}})
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Value> {
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        // 头部名大小写不敏感（hyper/reqwest 发小写 `content-length`）。
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().ok()?;
            }
        }
    }
    if content_length == 0 {
        return None;
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn spawn_mock() -> (u16, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let c = c.clone();
            // 每连接一线程：慢连接/复用不阻塞后续请求。
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                let mut reader = BufReader::new(stream);
                let Some(body) = read_request(&mut reader) else {
                    return;
                };
                let resp = mock_response(&body);
                c.fetch_add(1, Ordering::SeqCst);
                let body_str = resp.to_string();
                let out = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body_str.len(),
                    body_str
                );
                let _ = reader.get_mut().write_all(out.as_bytes());
            });
        }
    });
    (port, counter)
}

// —— 同步/异步测试辅助 ——

fn wait_ready(addr: &str) {
    for _ in 0..500 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("process did not start listening on {addr}");
}

async fn wait_registered(client: &Client, name: &str) {
    for _ in 0..100 {
        if let Ok(v) = client.call(method::SYS_REGISTRY_LIST, json!({})).await {
            let found = v
                .get("processes")
                .and_then(|p| p.as_array())
                .map(|arr| arr.iter().any(|p| p.get("name").and_then(|n| n.as_str()) == Some(name)))
                .unwrap_or(false);
            if found {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("process {name} did not register in time");
}

async fn wait_for_event<F>(client: &Client, timeout: Duration, mut pred: F) -> Option<(String, Value)>
where
    F: FnMut(&str, &Value) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let evt = tokio::time::timeout(Duration::from_millis(200), client.next())
            .await
            .ok()
            .flatten();
        if let Some(Incoming::Event { method, params }) = evt {
            if pred(&method, &params.data) {
                return Some((method, params.data));
            }
        }
    }
    None
}

/// 等待 `action.chat.reply` 请求（回复 `ok` 并返回正文）。
async fn wait_chat_reply(client: &Client, timeout: Duration) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let evt = tokio::time::timeout(Duration::from_millis(200), client.next())
            .await
            .ok()
            .flatten();
        if let Some(Incoming::Request { id, method, params }) = evt {
            if method == action::CHAT_REPLY {
                let text = params
                    .data
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let _ = client.reply(&id, json!({"ok": true})).await;
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
    None
}

async fn wait_heater_on(client: &Client, expect: bool, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let r = client
            .call(action::TEMPERATURE_READ, json!({}))
            .await
            .expect("temperature read");
        if r.get("heater_on").and_then(|v| v.as_bool()) == Some(expect) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "heater.on not {expect} within 5s of {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_temp_in_range(client: &Client, lo: f64, hi: f64, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let r = client
            .call(action::TEMPERATURE_READ, json!({}))
            .await
            .expect("temperature read");
        if let Some(v) = r.get("value").and_then(|x| x.as_f64()) {
            if v >= lo && v <= hi {
                return;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "temperature not in [{lo},{hi}] within 60s of {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_env_true(client: &Client, key: &str, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(r) = client.call(method::ENV_GET, json!({"key": key})).await {
            if r.get("value").and_then(|v| v.as_bool()) == Some(true) {
                return;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "env {key} not true within 10s of {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_mock_calls(counter: &Arc<AtomicUsize>, target: usize, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if counter.load(Ordering::SeqCst) >= target {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

// —— 测试 ——

#[tokio::test]
async fn m3_chat_hold_temp() {
    let (mock_port, mock_calls) = spawn_mock();
    let cfg = write_temp(
        "config.json",
        &format!(r#"{{"llm":{{"api_url":"http://127.0.0.1:{mock_port}/v1","api_key":"test","model":"mock","timeout_s":10}}}}"#),
    );

    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let _bus = spawn_bus(port);
    wait_ready(&addr);
    let _hw = spawn_native("hw-temp", &["--bus", addr.as_str()]);
    let _task = spawn_task_thermostat(port);
    let _llm = spawn_llm_gw(port, &cfg);
    let _rt = spawn_runtime(port, None);

    // tester 提供 chat.reply，用于捕获运行时经 LLM 发回的回复。
    let client = Client::connect(&addr, "tester", &["event.*"], &[action::CHAT_REPLY], &[])
        .await
        .expect("tester register");

    wait_registered(&client, "hw-temp").await;
    wait_registered(&client, "task-thermostat").await;
    wait_registered(&client, "llm-gw").await;
    wait_registered(&client, "runtime").await;

    // 先把环境调到 15 ℃（不触发告警）让温度偏离 25，再发 chat。
    client
        .call(action::TEMPERATURE_SET_AMBIENT, json!({"value": 15.0, "hold": true}))
        .await
        .expect("set_ambient");
    // 等温度跌到 24 以下（确保 hold_temp 确有动作可做）。
    let drift = std::time::Instant::now();
    loop {
        let r = client
            .call(action::TEMPERATURE_READ, json!({}))
            .await
            .expect("temperature read");
        if r.get("value").and_then(|v| v.as_f64()).map(|v| v < 24.0) == Some(true) {
            break;
        }
        assert!(
            std::time::Instant::now() - drift < Duration::from_secs(10),
            "temperature did not drift below 24"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    client
        .emit(event::CHAT_MESSAGE, json!({"session": "s1", "text": "请把房间温度保持在25度"}))
        .await
        .expect("emit chat");

    // LLM 决策记录含 task.start（decision 先于 chat.reply 发出，故先捕获）。
    let d = wait_for_event(&client, Duration::from_secs(10), |m, d| {
        m == event::RUNTIME_DECISION
            && d.get("mode").and_then(|v| v.as_str()) == Some("llm")
            && d.get("actions")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().any(|x| x.as_str() == Some(action::TASK_START)))
                .unwrap_or(false)
    })
    .await;
    assert!(d.is_some(), "expected llm decision with action.task.start");

    // 30 s 内收到 chat.reply（回复路径走通）。
    let reply = wait_chat_reply(&client, Duration::from_secs(30)).await;
    assert!(reply.is_some(), "expected chat.reply within 30s");

    // 任务已启动：env 置 true。
    wait_env_true(&client, "task.hold_temp.enabled", "chat hold_temp").await;

    // 温度收敛到 25±2（验收上限 5 min，测试取 60 s）。
    wait_temp_in_range(&client, 23.0, 27.0, "hold_temp").await;

    assert!(
        wait_mock_calls(&mock_calls, 2, Duration::from_secs(3)).await,
        "mock LLM should be called (tool + final), got {}",
        mock_calls.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn m3_empty_rules_llm_fallback() {
    let (mock_port, _mock_calls) = spawn_mock();
    let cfg = write_temp(
        "config.json",
        &format!(r#"{{"llm":{{"api_url":"http://127.0.0.1:{mock_port}/v1","api_key":"test","model":"mock","timeout_s":10}}}}"#),
    );
    // 空规则：无 low 告警规则，low 告警只能靠 LLM 推理。
    let rules = write_temp(
        "rules.json",
        r#"{"ignore":["event.temperature.updated","event.heartbeat.tick","event.env.changed"],"rules":[]}"#,
    );

    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let _bus = spawn_bus(port);
    wait_ready(&addr);
    let _hw = spawn_native("hw-temp", &["--bus", addr.as_str()]);
    let _llm = spawn_llm_gw(port, &cfg);
    let _rt = spawn_runtime(port, Some(&rules));

    let client = Client::connect(&addr, "tester", &["event.*"], &[], &[])
        .await
        .expect("tester register");

    wait_registered(&client, "hw-temp").await;
    wait_registered(&client, "llm-gw").await;
    wait_registered(&client, "runtime").await;

    client
        .call(action::TEMPERATURE_SET_AMBIENT, json!({"value": -5.0, "hold": true}))
        .await
        .expect("set_ambient");

    let low = wait_for_event(&client, Duration::from_secs(15), |m, d| {
        m == event::TEMPERATURE_ALARM && d.get("level").and_then(|v| v.as_str()) == Some("low")
    })
    .await;
    assert!(low.is_some(), "expected low alarm");

    // 无规则命中 → LLM 决策开加热器。
    let d = wait_for_event(&client, Duration::from_secs(10), |m, d| {
        m == event::RUNTIME_DECISION
            && d.get("mode").and_then(|v| v.as_str()) == Some("llm")
            && d.get("actions")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().any(|x| x.as_str() == Some(action::HEATER_START)))
                .unwrap_or(false)
    })
    .await;
    assert!(d.is_some(), "expected llm decision with action.heater.start");

    wait_heater_on(&client, true, "low alarm via llm").await;
}
