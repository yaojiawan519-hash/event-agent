//! M2 验收：运行时（Wasm）+ 规则。
//!
//! 用 `wasmedge` 运行 `runtime.wasm`，验证规则触发：
//! - low 告警后 2 s 内 `heater.on` 变 `true`（rule low-temp-heat → action.heater.start）
//! - high 告警后 `heater.on` 变 `false`（rule high-temp-cool → action.heater.stop）
//! - `event.runtime.decision` 的 `mode` 为 `rule`
//!
//! 依赖：`rustup run beta cargo build --target wasm32-wasip1 --release -p runtime`
//! 产出 `runtime.wasm`（scripts/smoke.sh 已按此顺序），以及 PATH 上的 `wasmedge`。

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use ea_proto::{action, event, method};
use ea_sysapi::Client;
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

fn runtime_wasm() -> std::path::PathBuf {
    let p = workspace_root()
        .join("crates/runtime/target/wasm32-wasip1/release/runtime.wasm");
    assert!(
        p.exists(),
        "runtime.wasm not found; run `rustup run beta cargo build --target wasm32-wasip1 --release -p runtime` (see scripts/smoke.sh)"
    );
    p
}

fn spawn(name: &str, args: &[&str]) -> ChildGuard {
    let child = Command::new(bin(name))
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn process");
    ChildGuard(child)
}

fn spawn_bus(port: u16) -> ChildGuard {
    let db = std::env::temp_dir().join(format!("ea_m2_{}_{}.db", std::process::id(), port));
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

fn spawn_runtime(port: u16) -> ChildGuard {
    let child = Command::new("wasmedge")
        .arg(runtime_wasm())
        .arg("--bus")
        .arg(format!("127.0.0.1:{port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn wasmedge runtime (is `wasmedge` installed?)");
    ChildGuard(child)
}

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
        if let Some(ea_sysapi::Incoming::Event { method, params }) = evt {
            if pred(&method, &params.data) {
                return Some((method, params.data));
            }
        }
    }
    None
}

async fn wait_heater_on(client: &Client, expect: bool, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
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
            "heater.on not {expect} within 2s of {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn m2_rules_low_heat_high_cool() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let _bus = spawn_bus(port);
    wait_ready(&addr);
    let _hw = spawn("hw-temp", &["--bus", addr.as_str()]);
    let _rt = spawn_runtime(port);

    let client = Client::connect(&addr, "tester", &["event.*"], &[], &[])
        .await
        .expect("tester register");

    wait_registered(&client, "hw-temp").await;
    wait_registered(&client, "runtime").await;

    // 冷环境触发 low 告警
    client
        .call(action::TEMPERATURE_SET_AMBIENT, json!({"value": -5.0, "hold": true}))
        .await
        .expect("set_ambient");

    let low = wait_for_event(&client, Duration::from_secs(15), |m, d| {
        m == event::TEMPERATURE_ALARM && d.get("level").and_then(|v| v.as_str()) == Some("low")
    })
    .await;
    assert!(low.is_some(), "expected low alarm");

    // low 告警后 2 s 内 heater.on 变 true
    wait_heater_on(&client, true, "low alarm").await;

    // 决策记录：mode=rule、rule=low-temp-heat、动作含 action.heater.start
    let d1 = wait_for_event(&client, Duration::from_secs(5), |m, d| {
        m == event::RUNTIME_DECISION
            && d.get("mode").and_then(|v| v.as_str()) == Some("rule")
            && d.get("rule").and_then(|v| v.as_str()) == Some("low-temp-heat")
    })
    .await;
    assert!(d1.is_some(), "expected low-temp-heat decision");
    let has_start = d1
        .as_ref()
        .and_then(|(_, d)| d.get("actions").and_then(|a| a.as_array()))
        .map(|a| a.iter().any(|x| x.as_str() == Some(action::HEATER_START)))
        .unwrap_or(false);
    assert!(has_start, "low-temp-heat decision should include action.heater.start");

    // 加热到 high 告警 → runtime 关加热器
    let high = wait_for_event(&client, Duration::from_secs(20), |m, d| {
        m == event::TEMPERATURE_ALARM && d.get("level").and_then(|v| v.as_str()) == Some("high")
    })
    .await;
    assert!(high.is_some(), "expected high alarm after heating");

    wait_heater_on(&client, false, "high alarm").await;

    let d2 = wait_for_event(&client, Duration::from_secs(5), |m, d| {
        m == event::RUNTIME_DECISION
            && d.get("mode").and_then(|v| v.as_str()) == Some("rule")
            && d.get("rule").and_then(|v| v.as_str()) == Some("high-temp-cool")
    })
    .await;
    assert!(d2.is_some(), "expected high-temp-cool decision");
    let has_stop = d2
        .as_ref()
        .and_then(|(_, d)| d.get("actions").and_then(|a| a.as_array()))
        .map(|a| a.iter().any(|x| x.as_str() == Some(action::HEATER_STOP)))
        .unwrap_or(false);
    assert!(has_stop, "high-temp-cool decision should include action.heater.stop");
}
