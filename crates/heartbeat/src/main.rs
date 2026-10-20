//! `heartbeat`：心跳进程（M1）。
//!
//! 每 2 s 拉一次注册表：掉线的进程发 `event.process.down`，新/恢复的进程发
//! `event.process.up`；对每个在册进程发 `sys.ping` 做软挂检测（连续 3 次失败判死）。
//! 每 5 s 发一次 `event.heartbeat.tick`。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ea_proto::{event, method};
use ea_sysapi::{Client, Incoming};
use serde_json::json;
use tracing::info;

const ROUND: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_secs(5);
const PING_TIMEOUT: Duration = Duration::from_secs(1);
const DOWN_AFTER: u32 = 3;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let addr = ea_sysapi::parse_bus_arg();
    let client = Client::connect(&addr, "heartbeat", &[], &[], &[]).await?;
    info!("heartbeat connected to {addr}");
    let started = Instant::now();

    let mut up: HashSet<String> = HashSet::new();
    let mut failures: HashMap<String, u32> = HashMap::new();
    let mut initialized = false;

    let mut round = tokio::time::interval(ROUND);
    let mut tick = tokio::time::interval(TICK);
    round.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = round.tick() => {
                round_once(&client, &mut up, &mut failures, &mut initialized).await;
            }
            _ = tick.tick() => {
                let mut alive: Vec<String> = up.iter().cloned().collect();
                alive.sort();
                client
                    .emit(
                        event::HEARTBEAT_TICK,
                        json!({"ts": chrono::Utc::now().to_rfc3339(), "alive": alive}),
                    )
                    .await
                    .ok();
            }
            msg = client.next() => {
                match msg {
                    Some(Incoming::Request { id, method: m, .. }) if m == method::SYS_PING => {
                        let uptime = started.elapsed().as_secs();
                        let _ = client.reply(&id, json!({"pong": true, "uptime_s": uptime})).await;
                    }
                    Some(Incoming::Request { .. }) => {}
                    Some(Incoming::Event { .. }) => {}
                    None => break,
                }
            }
        }
    }
    Ok(())
}

async fn round_once(
    client: &Client,
    up: &mut HashSet<String>,
    failures: &mut HashMap<String, u32>,
    initialized: &mut bool,
) {
    let v = match client.call(method::SYS_REGISTRY_LIST, json!({})).await {
        Ok(v) => v,
        Err(_) => return,
    };
    let current: HashSet<String> = v
        .get("processes")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.get("name").and_then(|n| n.as_str()).map(String::from))
                .filter(|n| n != "heartbeat")
                .collect()
        })
        .unwrap_or_default();

    if !*initialized {
        *up = current.clone();
        for n in &current {
            failures.insert(n.clone(), 0);
        }
        *initialized = true;
        return;
    }

    // 1) 掉线：之前在册、现在消失的进程
    let vanished: Vec<String> = up.iter().filter(|n| !current.contains(*n)).cloned().collect();
    for n in vanished {
        info!("process down: {n}");
        client.emit(event::PROCESS_DOWN, json!({"name": n})).await.ok();
        up.remove(&n);
        failures.remove(&n);
    }

    // 2) 软挂检测 + 恢复
    for n in &current {
        let ok = client
            .call_timeout(method::SYS_PING, json!({"target": n}), PING_TIMEOUT)
            .await
            .is_ok();
        if ok {
            if !up.contains(n) {
                info!("process up: {n}");
                client.emit(event::PROCESS_UP, json!({"name": n})).await.ok();
            }
            up.insert(n.clone());
            failures.insert(n.clone(), 0);
        } else {
            let f = failures.entry(n.clone()).or_insert(0);
            *f += 1;
            if *f >= DOWN_AFTER && up.contains(n) {
                info!("process down (no pong): {n}");
                client.emit(event::PROCESS_DOWN, json!({"name": n})).await.ok();
                up.remove(n);
            }
        }
    }
}
