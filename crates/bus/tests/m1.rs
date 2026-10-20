//! M1 验收：最小闭环（bus + hw-temp + heartbeat + web）。
//!
//! - `set_ambient` 固定 -5 ℃ 后 12 s 内出现 low 告警，固定 30 ℃ 后出现 cleared
//! - 加热器可启停、温度可读
//! - 杀 hw-temp 后 15 s 内出现 `event.process.down`
//! - web 的 `GET /` 返回单页前端
//!
//! 依赖 `cargo build --workspace` 先构建出 hw-temp/heartbeat/web 二进制
//!（scripts/smoke.sh 已按此顺序执行）。

use std::io::{Read, Write};
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
    let db = std::env::temp_dir().join(format!("ea_m1_{}_{}.db", std::process::id(), port));
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

async fn wait_for_event<F>(
    client: &Client,
    timeout: Duration,
    mut pred: F,
) -> Option<(String, Value)>
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

#[tokio::test]
async fn m1_alarm_heater_process_down() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let _bus = spawn_bus(port);
    wait_ready(&addr);
    let _hw = spawn("hw-temp", &["--bus", addr.as_str()]);
    let _hb = spawn("heartbeat", &["--bus", addr.as_str()]);

    let client = Client::connect(&addr, "tester", &["event.*"], &[], &[])
        .await
        .expect("tester register");

    // 等 hw-temp 注册完成（消除固定 sleep 的竞态）
    wait_registered(&client, "hw-temp").await;

    // 低温告警：固定冷环境 -5 ℃（加热器默认关，室温向环境散热下降）
    client
        .call(action::TEMPERATURE_SET_AMBIENT, json!({"value": -5.0, "hold": true}))
        .await
        .expect("set_ambient");
    let low = wait_for_event(&client, Duration::from_secs(12), |m, d| {
        m == event::TEMPERATURE_ALARM && d.get("level").and_then(|v| v.as_str()) == Some("low")
    })
    .await;
    assert!(low.is_some(), "expected low temperature alarm within 12s");

    // 开加热器（唯一热源）→ low 解除 → 温度继续升到 high 告警
    let r = client
        .call(action::HEATER_START, json!({}))
        .await
        .expect("heater start");
    assert_eq!(r["heater_on"], true);

    let cleared = wait_for_event(&client, Duration::from_secs(12), |m, d| {
        m == event::TEMPERATURE_ALARM_CLEARED && d.get("level").and_then(|v| v.as_str()) == Some("low")
    })
    .await;
    assert!(cleared.is_some(), "expected low alarm cleared within 12s");

    let high = wait_for_event(&client, Duration::from_secs(20), |m, d| {
        m == event::TEMPERATURE_ALARM && d.get("level").and_then(|v| v.as_str()) == Some("high")
    })
    .await;
    assert!(high.is_some(), "expected high alarm after heater on");

    // 关加热器 → high 解除
    let r = client
        .call(action::HEATER_STOP, json!({}))
        .await
        .expect("heater stop");
    assert_eq!(r["heater_on"], false);
    let high_cleared = wait_for_event(&client, Duration::from_secs(20), |m, d| {
        m == event::TEMPERATURE_ALARM_CLEARED && d.get("level").and_then(|v| v.as_str()) == Some("high")
    })
    .await;
    assert!(high_cleared.is_some(), "expected high alarm cleared after heater stop");

    // 温度读取：关加热器后 heater_on=false
    let r = client
        .call(action::TEMPERATURE_READ, json!({}))
        .await
        .expect("temperature read");
    assert_eq!(r["heater_on"], false);

    // 杀 hw-temp -> 15 s 内 process.down
    drop(_hw);
    let down = wait_for_event(&client, Duration::from_secs(15), |m, d| {
        m == event::PROCESS_DOWN && d.get("name").and_then(|v| v.as_str()) == Some("hw-temp")
    })
    .await;
    assert!(down.is_some(), "expected process.down for hw-temp within 15s");
}

#[tokio::test]
async fn m1_web_serves_page() {
    let port = free_port();
    let web_port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let _bus = spawn_bus(port);
    wait_ready(&addr);
    let _hw = spawn("hw-temp", &["--bus", addr.as_str()]);
    let web_listen = format!("0.0.0.0:{web_port}");
    let _web = spawn(
        "web",
        &["--bus", addr.as_str(), "--listen", web_listen.as_str()],
    );
    wait_ready(&format!("127.0.0.1:{web_port}"));

    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{web_port}")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut buf = String::new();
    let _ = stream.read_to_string(&mut buf);
    assert!(
        buf.contains("200"),
        "expected 200 response, got: {}",
        buf.lines().next().unwrap_or("")
    );
    assert!(buf.contains("text/html"));
    assert!(
        buf.contains("事件驱动智能体"),
        "index.html should contain the title"
    );
}
