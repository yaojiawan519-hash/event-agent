//! `task-thermostat`：任务进程（Wasm，M3）。
//!
//! 提供 `action.task.start`（hold_temp 恒温）与 `action.task.stop`。收到 start 后进入
//! bang-bang 控制循环：每 1 s 读温度，低于 `target - hysteresis` 开加热器、高于
//! `target + hysteresis` 关加热器；循环期间用 `next_timeout` 非阻塞轮询 stop 请求。
//!
//! 这是原文「循环读取总线数据、按算法控制温度」的最小版本；换成 PID 只改一个函数。

use std::time::Duration;

use ea_proto::{action, event, method, ActionSpec};
use ea_wasmapi::{Client, Incoming};
use serde_json::{json, Value};

fn main() {
    let addr = ea_wasmapi::parse_bus_arg();
    let mut client = match Client::connect(&addr) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[task-thermostat] connect {addr} 失败: {e:?}");
            std::process::exit(1);
        }
    };
    let actions = [
        ActionSpec {
            method: action::TASK_START.to_string(),
            description: "启动恒温任务（hold_temp）".to_string(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string"},
                    "target": {"type": "number"},
                    "hysteresis": {"type": "number"}
                }
            }),
        },
        ActionSpec {
            method: action::TASK_STOP.to_string(),
            description: "停止恒温任务".to_string(),
            params_schema: json!({"type": "object", "properties": {"task": {"type": "string"}}}),
        },
    ];
    if let Err(e) = client.register(
        "task-thermostat",
        &[],
        &[action::TASK_START, action::TASK_STOP],
        &actions,
    ) {
        eprintln!("[task-thermostat] register 失败: {e:?}");
        std::process::exit(1);
    }
    eprintln!("[task-thermostat] connected to {addr}");

    loop {
        match client.next() {
            None => break,
            Some(Incoming::Request { id, method: m, params }) => match m.as_str() {
                action::TASK_START => {
                    let _ = client.reply(&id, json!({"started": true}));
                    run_hold_temp(&mut client, &params.data);
                }
                action::TASK_STOP => {
                    let _ = client.reply(&id, json!({"stopped": false, "running": false}));
                }
                method::SYS_PING => {
                    let _ = client.reply(&id, json!({"pong": true}));
                }
                _ => {
                    let _ = client.reply_error(&id, ea_proto::error::METHOD_NOT_FOUND, "unknown method");
                }
            },
            Some(Incoming::Event { .. }) => {}
        }
    }
}

/// 恒温控制循环：节拍 1 s，读温度后 bang-bang 控制加热器；收到 stop 或外部关停则退出。
fn run_hold_temp(client: &mut Client, data: &Value) {
    let task = data
        .get("task")
        .and_then(|v| v.as_str())
        .unwrap_or("hold_temp")
        .to_string();
    let target = data.get("target").and_then(|v| v.as_f64()).unwrap_or(25.0);
    let hysteresis = data
        .get("hysteresis")
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0);

    let _ = client.env_set("task.hold_temp.enabled", json!(true), false);
    let _ = client.env_set("task.hold_temp.target", json!(target), false);
    let _ = client.emit(event::TASK_STARTED, json!({"task": task}));
    eprintln!("[task-thermostat] hold_temp start: target={target} hyst={hysteresis}");

    let mut reason = "stop";
    loop {
        // 非阻塞节拍：最多等 1 s，期间收到的 stop 请求立即处理。
        let mut stop_requested = false;
        match client.next_timeout(Duration::from_secs(1)) {
            Some(Incoming::Request { id, method: m, .. }) if m == action::TASK_STOP => {
                let _ = client.reply(&id, json!({"stopped": true, "running": false}));
                stop_requested = true;
            }
            Some(Incoming::Request { id, .. }) => {
                let _ = client.reply_error(&id, ea_proto::error::METHOD_NOT_FOUND, "task busy");
            }
            Some(Incoming::Event { .. }) => {}
            None => {}
        }
        if stop_requested {
            break;
        }

        // 外部关停（env 被写 false）。
        if let Ok(Some(enabled)) = client.env_get("task.hold_temp.enabled") {
            if enabled == json!(false) {
                reason = "env_disabled";
                break;
            }
        }

        // 控制步：读温度 → bang-bang。
        if let Ok(v) = client.call(action::TEMPERATURE_READ, json!({})) {
            if let Some(t) = v.get("value").and_then(|x| x.as_f64()) {
                if t < target - hysteresis {
                    let _ = client.call(action::HEATER_START, json!({}));
                } else if t > target + hysteresis {
                    let _ = client.call(action::HEATER_STOP, json!({}));
                }
            }
        }
    }

    let _ = client.env_set("task.hold_temp.enabled", json!(false), false);
    let _ = client.emit(event::TASK_STOPPED, json!({"task": task, "reason": reason}));
    eprintln!("[task-thermostat] hold_temp stop: reason={reason}");
}
