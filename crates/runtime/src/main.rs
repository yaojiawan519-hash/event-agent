//! `runtime`：事件运行时（Wasm，M2 → M3）。
//!
//! 单线程同步事件循环：连接总线 → 注册订阅 `event.*` → 逐条 `next()` 决策。
//! 决策顺序：过滤 ignore → 规则匹配（命中则执行 `then`，`explain:true` 再让 LLM 补一句说明）
//! → 无规则命中则进入 LLM 推理（工具 = 已登记的 `action.*` + 虚拟 `task.start`）。
//!
//! 规则默认硬编码；`--rules <path>` 可加载 `rules.json` 覆盖（M4 挂载 config 目录时用）。

use std::collections::VecDeque;
use std::time::Duration;

use ea_proto::{action, event, method, ActionSpec};
use ea_wasmapi::{Client, Incoming};
use serde_json::{json, Value};

const DEFAULT_IGNORE: &[&str] = &[
    event::TEMPERATURE_UPDATED,
    event::HEARTBEAT_TICK,
    event::ENV_CHANGED,
];
const EVENT_WINDOW: usize = 20;
const MAX_ROUNDS: u32 = 5;
/// LLM 推理调用超时（对齐 llm-gw 的上游 60 s 超时；真实大模型偶发 >5 s，用默认 5 s 会误判为不可用）。
const LLM_TIMEOUT: Duration = Duration::from_secs(60);

struct Rule {
    id: String,
    method: String,
    level: Option<String>,
    then: Vec<String>,
    explain: bool,
}

impl Rule {
    fn matches(&self, m: &str, data: &Value) -> bool {
        if self.method != m {
            return false;
        }
        match &self.level {
            Some(lvl) => data.get("level").and_then(|v| v.as_str()) == Some(lvl.as_str()),
            None => true,
        }
    }
}

struct Runtime {
    client: Client,
    ignore: Vec<String>,
    rules: Vec<Rule>,
    actions: Vec<ActionSpec>,
    history: VecDeque<(String, Value)>,
    max_rounds: u32,
}

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

    let (ignore, rules) = load_rules();
    eprintln!("[runtime] connected to {addr}, {} rules", rules.len());

    let mut rt = Runtime {
        client,
        ignore,
        rules,
        actions: Vec::new(),
        history: VecDeque::new(),
        max_rounds: MAX_ROUNDS,
    };

    loop {
        match rt.client.next() {
            None => break,
            Some(Incoming::Event { method, params }) => dispatch(&mut rt, &method, &params.data),
            Some(Incoming::Request { id, .. }) => {
                let _ = rt.client.reply_error(
                    &id,
                    ea_proto::error::METHOD_NOT_FOUND,
                    "runtime provides no actions",
                );
            }
        }
    }
}

fn dispatch(rt: &mut Runtime, m: &str, data: &Value) {
    // ignore 列表：只更新本地缓存（env.snapshot 按需读），不进入决策。
    if rt.ignore.iter().any(|x| x == m) {
        return;
    }
    rt.history.push_back((m.to_string(), data.clone()));
    if rt.history.len() > EVENT_WINDOW {
        rt.history.pop_front();
    }

    // 规则匹配（第一条命中即止）。
    for i in 0..rt.rules.len() {
        if !rt.rules[i].matches(m, data) {
            continue;
        }
        let rule_id = rt.rules[i].id.clone();
        let explain = rt.rules[i].explain;
        let then = rt.rules[i].then.clone();
        let mut executed = Vec::new();
        for a in &then {
            match rt.client.call(a, json!({})) {
                Ok(_) => executed.push(a.clone()),
                Err(e) => eprintln!("[runtime] rule {rule_id} action {a} 失败: {e:?}"),
            }
        }
        emit_decision(rt, m, "rule", Some(&rule_id), &executed);
        if explain {
            explain_rule(rt, m, &rule_id, data, &executed);
        }
        return;
    }

    // 无规则命中 → LLM 推理。
    llm_decide(rt, m, data);
}

fn llm_decide(rt: &mut Runtime, m: &str, data: &Value) {
    refresh_actions(rt);
    let session = data.get("session").and_then(|v| v.as_str()).unwrap_or("").to_string();

    let mut messages = vec![json!({"role": "system", "content": system_prompt(rt)})];
    if let Ok(snap) = rt.client.env_snapshot("") {
        if !snap.is_empty() {
            let s = serde_json::to_string(&snap).unwrap_or_default();
            messages.push(json!({"role": "system", "content": format!("当前环境快照: {s}")}));
        }
    }
    for (em, ed) in rt.history.iter().rev() {
        messages.push(json!({
            "role": "user",
            "content": format!("事件 {em}: {}", serde_json::to_string(ed).unwrap_or_default()),
        }));
    }
    if m == event::CHAT_MESSAGE {
        let text = data.get("text").and_then(|v| v.as_str()).unwrap_or("");
        messages.push(json!({"role": "user", "content": text}));
    } else {
        messages.push(json!({
            "role": "user",
            "content": format!("触发事件 {m}: {}", serde_json::to_string(data).unwrap_or_default()),
        }));
    }

    let tools = build_tools(rt);
    let mut executed: Vec<String> = Vec::new();
    let mut final_text = String::new();

    for _ in 0..rt.max_rounds {
        let resp = match rt.client.call_timeout(method::SYS_LLM_CHAT, json!({"messages": messages, "tools": tools}), LLM_TIMEOUT) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[runtime] llm_chat 失败: {e:?}");
                final_text = "（大模型不可用）".to_string();
                break;
            }
        };
        let content = resp.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let tool_calls = resp.get("tool_calls").cloned().unwrap_or_else(|| json!([]));
        let n_calls = tool_calls.as_array().map(|a| a.len()).unwrap_or(0);
        if n_calls == 0 {
            final_text = content;
            break;
        }
        // 追加 assistant 消息（含 tool_calls），再逐个执行工具并追加 tool 结果。
        // llm-gw 归一化后的 tool_calls 是 {id,name,arguments(Value)}，回填给上游须还原为
        // OpenAI 原生结构 {id,type:"function",function:{name,arguments(字符串)}}，
        // 否则 DeepSeek 报 `missing field 'type'`。
        let echo_calls: Vec<Value> = tool_calls
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|tc| {
                        let args = tc.get("arguments").cloned().unwrap_or_else(|| json!({}));
                        let args_str = match args {
                            Value::String(s) => s,
                            v => serde_json::to_string(&v).unwrap_or_else(|_| "{}".to_string()),
                        };
                        json!({
                            "id": tc.get("id").cloned().unwrap_or(Value::Null),
                            "type": "function",
                            "function": {
                                "name": tc.get("name").cloned().unwrap_or(Value::Null),
                                "arguments": args_str,
                            }
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        messages.push(json!({"role": "assistant", "content": content, "tool_calls": echo_calls}));
        if let Some(arr) = tool_calls.as_array() {
            for tc in arr {
                let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let tc_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let args = tc.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let (real, allowed) = resolve_action(rt, &name);
                let result = if allowed {
                    match rt.client.call(&real, args.clone()) {
                        Ok(v) => {
                            executed.push(real);
                            serde_json::to_string(&v).unwrap_or_default()
                        }
                        Err(e) => format!("error: {e:?}"),
                    }
                } else {
                    format!("拒绝：未登记的方法 {name}")
                };
                messages.push(json!({"role": "tool", "tool_call_id": tc_id, "content": result}));
            }
        }
    }

    emit_decision(rt, m, "llm", None, &executed);
    if !final_text.is_empty() {
        let _ = rt.client.call(action::CHAT_REPLY, json!({"session": session, "text": final_text}));
    }
}

fn explain_rule(rt: &mut Runtime, event_method: &str, rule_id: &str, data: &Value, executed: &[String]) {
    let session = data.get("session").and_then(|v| v.as_str()).unwrap_or("");
    let user = format!(
        "事件 {event_method} 触发规则 {rule_id}，执行操作 {}。请用一句话向用户解释。",
        executed.join(", ")
    );
    let req = json!({
        "messages": [
            {"role": "system", "content": "你用一个中文短句向用户解释系统刚才的自动操作。"},
            {"role": "user", "content": user}
        ],
        "tools": []
    });
    if let Ok(resp) = rt.client.call_timeout(method::SYS_LLM_CHAT, req, LLM_TIMEOUT) {
        if let Some(text) = resp.get("content").and_then(|v| v.as_str()) {
            let _ = rt.client.call(action::CHAT_REPLY, json!({"session": session, "text": text}));
        }
    }
}

fn refresh_actions(rt: &mut Runtime) {
    if let Ok(v) = rt.client.call(method::SYS_REGISTRY_ACTIONS, json!({})) {
        if let Some(arr) = v.get("actions").and_then(|a| a.as_array()) {
            rt.actions = arr
                .iter()
                .filter_map(|a| serde_json::from_value::<ActionSpec>(a.clone()).ok())
                .collect();
        }
    }
}

fn system_prompt(rt: &Runtime) -> String {
    let mut s = String::from("你是事件驱动智能体，根据事件决定对温度环境执行的操作。可用操作：\n");
    for a in &rt.actions {
        if a.method.starts_with("action.") && a.method != action::CHAT_REPLY {
            s.push_str(&format!("- {}：{}\n", tool_name(&a.method), a.description));
        }
    }
    s.push_str("- task_start：启动异步任务（如 hold_temp 恒温）\n");
    s.push_str("收到事件后用 tool_calls 调用相应操作；完成后用简短中文说明你做了什么。");
    s
}

fn build_tools(rt: &Runtime) -> Value {
    let mut tools = Vec::new();
    for a in &rt.actions {
        if a.method.starts_with("action.") && a.method != action::CHAT_REPLY {
            tools.push(json!({
                "type": "function",
                "function": {"name": tool_name(&a.method), "description": a.description, "parameters": a.params_schema}
            }));
        }
    }
    tools.push(json!({
        "type": "function",
        "function": {
            "name": tool_name("task.start"),
            "description": "启动异步任务（如 hold_temp 恒温）",
            "parameters": json!({"type":"object","properties":{"task":{"type":"string"},"target":{"type":"number"},"hysteresis":{"type":"number"}}})
        }
    }));
    json!(tools)
}

/// LLM 工具名 = 总线方法名的「点 → 下划线」形式（DeepSeek 等 OpenAI 兼容端点要求工具名匹配 `^[a-zA-Z0-9_-]+$`，不能含 `.`）。
fn tool_name(method: &str) -> String {
    method.replace('.', "_")
}

fn resolve_action(rt: &Runtime, name: &str) -> (String, bool) {
    // name 是 sanitized 工具名，反查真实总线方法名。
    if name == tool_name("task.start") {
        return (action::TASK_START.to_string(), true);
    }
    if let Some(a) = rt.actions.iter().find(|a| tool_name(&a.method) == name) {
        return (a.method.clone(), true);
    }
    (name.to_string(), false)
}

fn emit_decision(rt: &mut Runtime, event_method: &str, mode: &str, rule: Option<&str>, actions: &[String]) {
    let mut d = json!({"event_method": event_method, "mode": mode, "actions": actions});
    if let Some(r) = rule {
        d["rule"] = json!(r);
    }
    let _ = rt.client.emit(event::RUNTIME_DECISION, d);
}

// —— 规则加载 ——

fn default_rules() -> Vec<Rule> {
    vec![
        Rule {
            id: "low-temp-heat".into(),
            method: event::TEMPERATURE_ALARM.into(),
            level: Some("low".into()),
            then: vec![action::HEATER_START.into()],
            explain: true,
        },
        Rule {
            id: "high-temp-cool".into(),
            method: event::TEMPERATURE_ALARM.into(),
            level: Some("high".into()),
            then: vec![action::HEATER_STOP.into()],
            explain: true,
        },
        Rule {
            id: "cleared".into(),
            method: event::TEMPERATURE_ALARM_CLEARED.into(),
            level: None,
            then: vec![],
            explain: true,
        },
    ]
}

fn load_rules() -> (Vec<String>, Vec<Rule>) {
    let default_ignore: Vec<String> = DEFAULT_IGNORE.iter().map(|s| s.to_string()).collect();
    let Some(path) = parse_rules_arg() else {
        return (default_ignore, default_rules());
    };
    let parsed = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok());
    match parsed {
        Some(v) => {
            let ignore = v
                .get("ignore")
                .and_then(|i| i.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                .unwrap_or(default_ignore);
            let rules = v
                .get("rules")
                .and_then(|r| r.as_array())
                .map(|a| a.iter().filter_map(parse_rule).collect())
                .unwrap_or_else(default_rules);
            eprintln!("[runtime] loaded rules from {path}: {} rules", rules.len());
            (ignore, rules)
        }
        None => {
            eprintln!("[runtime] failed to load rules from {path}, using defaults");
            (default_ignore, default_rules())
        }
    }
}

fn parse_rule(v: &Value) -> Option<Rule> {
    let id = v.get("id")?.as_str()?.to_string();
    let when = v.get("when")?;
    let method = when.get("method")?.as_str()?.to_string();
    // 兼容 §8.6 示例的 "data.level"（字面点号键）与嵌套 {"data":{"level":...}} 两种写法。
    let level = when
        .get("data.level")
        .and_then(|x| x.as_str())
        .or_else(|| when.get("data").and_then(|d| d.get("level")).and_then(|x| x.as_str()))
        .map(|s| s.to_string());
    let then = v
        .get("then")
        .and_then(|t| t.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| t.get("method").and_then(|m| m.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let explain = v.get("explain").and_then(|x| x.as_bool()).unwrap_or(false);
    Some(Rule { id, method, level, then, explain })
}

fn parse_rules_arg() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--rules" {
            return args.next();
        }
    }
    None
}
