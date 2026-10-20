//! M0 验收：协议与总线。
//!
//! - 两个测试客户端经总线互发事件/操作
//! - 无提供者返回 -32601
//! - 重启 bus 后 `env.get` 读回重启前写入的值

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use ea_proto::{error, ActionSpec};
use ea_sysapi::Client;
use serde_json::json;

struct Bus {
    child: Child,
    addr: String,
    db: std::path::PathBuf,
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.db);
        let _ = std::fs::remove_file(self.db.with_extension("db-wal"));
        let _ = std::fs::remove_file(self.db.with_extension("db-shm"));
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spawn_bus_at(port: u16, db: &std::path::Path) -> Bus {
    let bin = env!("CARGO_BIN_EXE_bus");
    let child = Command::new(bin)
        .arg("--listen")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--db")
        .arg(db)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bus");
    let addr = format!("127.0.0.1:{port}");
    let mut ready = false;
    for _ in 0..500 {
        if std::net::TcpStream::connect(&addr).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(ready, "bus did not start listening on {addr}");
    Bus { child, addr, db: db.to_path_buf() }
}

fn spawn_bus() -> Bus {
    let port = free_port();
    let db = std::env::temp_dir().join(format!("ea_m0_{}_{}.db", std::process::id(), port));
    let _ = std::fs::remove_file(&db);
    spawn_bus_at(port, &db)
}

#[tokio::test]
async fn m0_event_and_action_roundtrip() {
    let bus = spawn_bus();

    let a = Client::connect(
        &bus.addr,
        "a",
        &["event.*"],
        &["action.echo"],
        &[ActionSpec {
            method: "action.echo".to_string(),
            description: "回显操作".to_string(),
            params_schema: json!({"type": "object"}),
        }],
    )
    .await
    .expect("a register");

    let b = Client::connect(&bus.addr, "b", &["event.*"], &[], &[])
        .await
        .expect("b register");

    // 1) 事件广播：b 发事件，a 收到，source 被总线覆写为 "b"
    b.emit("event.test.hello", json!({"x": 1})).await.unwrap();
    let evt = tokio::time::timeout(Duration::from_secs(2), a.next())
        .await
        .expect("a should receive event")
        .expect("event present");
    match evt {
        ea_sysapi::Incoming::Event { method, params } => {
            assert_eq!(method, "event.test.hello");
            assert_eq!(params.source, "b");
            assert_eq!(params.data["x"], 1);
        }
        other => panic!("expected event, got {:?}", other),
    }

    // 2) 操作路由：b 调用 action.echo，a 收到请求并回复
    let (_, call_result) = tokio::join!(
        async {
            let req = tokio::time::timeout(Duration::from_secs(2), a.next())
                .await
                .expect("a should receive request")
                .expect("request present");
            let (id, n) = match req {
                ea_sysapi::Incoming::Request { id, method, params } => {
                    assert_eq!(method, "action.echo");
                    assert_eq!(params.source, "b");
                    (id, params.data["n"].clone())
                }
                other => panic!("expected request, got {:?}", other),
            };
            a.reply(&id, json!({"echoed": n})).await.unwrap();
        },
        b.call("action.echo", json!({"n": 42})),
    );
    let result = call_result.expect("call ok");
    assert_eq!(result["echoed"], 42);

    // 3) 无提供者：-32601
    match b.call("action.nope", json!({})).await {
        Err(ea_sysapi::Error::Rpc { code, .. }) => assert_eq!(code, error::METHOD_NOT_FOUND),
        other => panic!("expected -32601, got {:?}", other),
    }
}

#[tokio::test]
async fn m0_env_persistence_across_restart() {
    let port = free_port();
    let db = std::env::temp_dir()
        .join(format!("ea_m0_restart_{}_{}.db", std::process::id(), port));
    let _ = std::fs::remove_file(&db);

    let mut bus = spawn_bus_at(port, &db);
    {
        let a = Client::connect(&bus.addr, "a", &[], &[], &[]).await.expect("a register");
        a.env_set("greeting", json!("hello"), true).await.unwrap();
        assert_eq!(a.env_get("greeting").await.unwrap().unwrap(), json!("hello"));
        let hist = a.env_history("greeting", 10).await.unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].value, json!("hello"));
    }

    // 重启（同一端口与 db）
    bus.child.kill().expect("kill");
    bus.child.wait().expect("wait");
    let bus2 = spawn_bus_at(port, &db);
    {
        let a = Client::connect(&bus2.addr, "a", &[], &[], &[]).await.expect("a register");
        assert_eq!(a.env_get("greeting").await.unwrap().unwrap(), json!("hello"));
    }
}
