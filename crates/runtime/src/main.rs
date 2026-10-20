//! `runtime`：事件运行时（Wasm，M2）。
//!
//! 单线程同步事件循环：连接总线 → 注册订阅 `event.*` → 逐条 `next()` 并按规则匹配执行。
//! M2 只做规则匹配（不接 LLM），命中后发 `event.runtime.decision {mode:"rule"}`。

use ea_proto::{action, event};
use ea_wasmapi::{Client, Incoming};
use serde_json::{json, Value};

/// 默认不进入决策的事件（1 Hz 噪声，避免反复触发规则）。
const IGNORE: &[&str] = &[
    event::TEMPERATURE_UPDATED,
    event::HEARTBEAT_TICK,
    event::ENV_CHANGED,
];

struct Rule {
    id: &'static str,
    method: &'static str,
    level: Option<&'static str>,
    then: &'static [&'static str],
}

/// 默认规则（对应 §8.6 的 rules.json；M4 改为挂载 config 目录读取）。
const RULES: &[Rule] = &[
    Rule {
        id: "low-temp-heat",
        method: event::TEMPERATURE_ALARM,
        level: Some("low"),
        then: &[action::HEATER_START],
    },
    Rule {
        id: "high-temp-cool",
        method: event::TEMPERATURE_ALARM,
        level: Some("high"),
        then: &[action::HEATER_STOP],
    },
    Rule {
        id: "cleared",
        method: event::TEMPERATURE_ALARM_CLEARED,
        level: None,
        then: &[],
    },
];

fn main() {
    let addr = ea_wasmapi::parse_bus_arg();
    let mut client = match Client::connect(&addr) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[runtime] connect {addr} 失败: {e:?}");
            std::process::exit(1);
        }
    };
    if let Err(e) = client.register("runtime", &["event.*"], &[], &[]) {
        eprintln!("[runtime] register 失败: {e:?}");
        std::process::exit(1);
    }
    eprintln!("[runtime] connected to {addr}");

    loop {
        match client.next() {
            None => break,
            Some(Incoming::Event { method, params }) => dispatch(&mut client, &method, &params.data),
            Some(Incoming::Request { id, .. }) => {
                let _ = client.reply_error(
                    &id,
                    ea_proto::error::METHOD_NOT_FOUND,
                    "runtime provides no actions in M2",
                );
            }
        }
    }
}

fn dispatch(client: &mut Client, m: &str, data: &Value) {
    if IGNORE.contains(&m) {
        return;
    }
    for r in RULES {
        if r.method != m {
            continue;
        }
        if let Some(lvl) = r.level {
            if data.get("level").and_then(|v| v.as_str()) != Some(lvl) {
                continue;
            }
        }
        let mut executed: Vec<String> = Vec::new();
        for &a in r.then {
            match client.call(a, json!({})) {
                Ok(_) => executed.push(a.to_string()),
                Err(e) => eprintln!("[runtime] rule {} action {} 失败: {e:?}", r.id, a),
            }
        }
        let _ = client.emit(
            event::RUNTIME_DECISION,
            json!({"event_method": m, "mode": "rule", "rule": r.id, "actions": executed}),
        );
        return;
    }
    // 无规则命中：M2 不接 LLM，忽略（M3 在此进入 LLM 推理）。
}
