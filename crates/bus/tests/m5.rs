//! M5 验收：实时运动控制进程（motor-ecat，mock 后端）。
//!
//! 验证「统一 RPC + RT/RPC 线程分离」的对外契约：
//! - 进程注册并提供 `action.motor.*`
//! - 动作是投递式的：立即返回 `{"accepted":true}`，**不等运动完成**
//! - 运动完成通过 `event.motor.reached` 通知
//! - `action.motor.read` 能读到 RT 线程发布的快照
//! - `action.motor.stop` 走急停路径（直接断使能）
//! - 参数非法返回 INVALID_PARAMS
//!
//! 依赖：`cargo build --workspace`（smoke.sh 已按此顺序）。

use std::process::{Child, Command, Stdio};
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

fn spawn_bus(port: u16) -> ChildGuard {
    let db = std::env::temp_dir().join(format!("ea_m5_{}_{}.db", std::process::id(), port));
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

fn spawn_motor(port: u16) -> ChildGuard {
    let child = Command::new(bin("motor-ecat"))
        .arg("--bus")
        .arg(format!("127.0.0.1:{port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn motor-ecat");
    ChildGuard(child)
}

fn wait_ready(addr: &str) {
    for _ in 0..500 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("bus did not start listening on {addr}");
}

async fn wait_for_event<F>(client: &Client, timeout: Duration, mut pred: F) -> Option<Value>
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
                return Some(params.data);
            }
        }
    }
    None
}

async fn connect_observer(port: u16) -> Client {
    for _ in 0..100 {
        match Client::connect(
            &format!("127.0.0.1:{port}"),
            "m5-observer",
            &["event.motor.*"],
            &[],
            &[],
        )
        .await
        {
            Ok(c) => return c,
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    panic!("observer could not connect to bus");
}

#[tokio::test]
async fn m5_motor_rpc_contract() {
    let port = free_port();
    let _bus = spawn_bus(port);
    let addr = format!("127.0.0.1:{port}");
    wait_ready(&addr);

    // 观察者先连上，才能收到 motor-ecat 后续发出的事件。
    let observer = connect_observer(port).await;

    let _motor = spawn_motor(port);

    // —— 1) 注册与动作清单 ——
    let mut actions = Vec::new();
    for _ in 0..100 {
        if let Ok(v) = observer.call(method::SYS_REGISTRY_ACTIONS, json!({})).await {
            if let Some(arr) = v.get("actions").and_then(|a| a.as_array()) {
                actions = arr
                    .iter()
                    .filter_map(|a| a.get("method").and_then(|m| m.as_str()).map(String::from))
                    .collect();
                if actions.iter().any(|m| m == action::MOTOR_MOVE_ABS) {
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for expected in [
        action::MOTOR_ENABLE,
        action::MOTOR_STOP,
        action::MOTOR_MOVE_ABS,
        action::MOTOR_MOVE_REL,
        action::MOTOR_HOME,
        action::MOTOR_READ,
    ] {
        assert!(
            actions.iter().any(|m| m == expected),
            "motor-ecat 未登记 {expected}（实际：{actions:?}）"
        );
    }

    // —— 2) 投递式语义：move_abs 立即返回，不等运动完成 ——
    let t0 = std::time::Instant::now();
    let ack = observer
        .call(action::MOTOR_MOVE_ABS, json!({"axis": 0, "position": 20000}))
        .await
        .expect("move_abs 应被接受");
    assert_eq!(ack.get("accepted").and_then(|v| v.as_bool()), Some(true));
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "move_abs 必须投递即返回（实际耗时 {:?}）—— 不能阻塞等运动完成",
        t0.elapsed()
    );

    // —— 3) 到位通过 event.motor.reached 通知（20000 counts @ 20000 counts/s ≈ 1 s）——
    let reached = wait_for_event(&observer, Duration::from_secs(5), |m, d| {
        m == event::MOTOR_REACHED && d.get("axis").and_then(|a| a.as_u64()) == Some(0)
    })
    .await
    .expect("5s 内未收到 event.motor.reached");
    assert_eq!(
        reached.get("position").and_then(|v| v.as_i64()),
        Some(20000),
        "到位位置应为 20000"
    );

    // —— 4) 快照可读 ——
    let snap = observer
        .call(action::MOTOR_READ, json!({}))
        .await
        .expect("motor.read 应成功");
    let axes = snap.get("axes").and_then(|a| a.as_array()).expect("axes 数组");
    assert_eq!(axes.len(), 2, "应有 2 个轴");
    assert_eq!(
        axes[0].get("position").and_then(|v| v.as_i64()),
        Some(20000),
        "读回位置应为 20000"
    );
    assert_eq!(
        snap.get("wkc").and_then(|v| v.as_u64()),
        Some(3),
        "工作计数应为 3（2 从站 + 1）"
    );

    // —— 5) 急停：断使能 ——
    let ack = observer
        .call(action::MOTOR_STOP, json!({}))
        .await
        .expect("motor.stop 应成功");
    assert_eq!(ack.get("estop").and_then(|v| v.as_bool()), Some(true));

    let mut stopped = false;
    for _ in 0..40 {
        let snap = observer.call(action::MOTOR_READ, json!({})).await.unwrap();
        let enabled = snap
            .get("axes")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
            .and_then(|a| a.get("enabled"))
            .and_then(|v| v.as_bool());
        if enabled == Some(false) {
            stopped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(stopped, "急停后 2s 内轴应断使能");

    // —— 6) 参数校验 ——
    let err = observer
        .call(action::MOTOR_MOVE_ABS, json!({"axis": 0}))
        .await
        .expect_err("缺少 position 应报参数错误");
    match err {
        ea_sysapi::Error::Rpc { code, .. } => assert_eq!(
            code,
            ea_proto::error::INVALID_PARAMS,
            "应返回 INVALID_PARAMS"
        ),
        other => panic!("期望 Rpc 错误，实际 {other:?}"),
    }
}
