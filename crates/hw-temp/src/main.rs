//! `hw-temp`：温度模拟器（M1）。
//!
//! 每 1 s 采样一次：加热器是唯一热源（开→升温、关→降温），环境只作冷源；
//! 按 2 ℃ 滞回触发高/低告警，通过总线提供加热器/温度读取/环境温度操作，
//! 并发布温度事件与环境快照。

use std::time::{Duration, Instant};

use ea_proto::{action, event, method, ActionSpec};
use ea_sysapi::{Client, Incoming};
use serde_json::json;
use tracing::info;

// α=0.3（τ≈3.3 s）：保证 set_ambient 固定冷环境后 10 s 内触达 low 告警阈值。
const ALPHA: f64 = 0.3;
// 加热器是唯一热源：环境只作冷源（见 step()），温度上升只能靠加热器。
// P=25 ℃/s 为演示校准——足够在冷环境下把温度抬过 high 告警阈值（60 ℃）。
const HEATER_POWER: f64 = 25.0;
const T_MIN: f64 = -10.0;
const T_MAX: f64 = 80.0;
const ALARM_LOW: f64 = 0.0;
const ALARM_HIGH: f64 = 60.0;
const HYSTERESIS: f64 = 2.0;
const AMBIENT_MIN: f64 = -8.0;
// 环境温度上限（冷源）：不再取 68 ℃，否则环境会直接加热房间（造成「温度涨但加热器关」）。
const AMBIENT_MAX: f64 = 20.0;
const AMBIENT_PERIOD_S: f64 = 1200.0; // 20 min

#[derive(Clone, Copy, PartialEq, Eq)]
enum AlarmState {
    Normal,
    Low,
    High,
}

impl AlarmState {
    fn as_str(self) -> &'static str {
        match self {
            AlarmState::Normal => "normal",
            AlarmState::Low => "low",
            AlarmState::High => "high",
        }
    }
}

struct Sim {
    temperature: f64,
    heater_on: bool,
    alarm: AlarmState,
    ambient_hold: Option<f64>,
    started: Instant,
}

impl Sim {
    fn new() -> Self {
        Sim {
            temperature: 25.0,
            heater_on: false,
            alarm: AlarmState::Normal,
            ambient_hold: None,
            started: Instant::now(),
        }
    }

    fn ambient(&self) -> f64 {
        if let Some(v) = self.ambient_hold {
            return v;
        }
        ambient_at(self.started.elapsed().as_secs_f64())
    }

    fn step(&mut self) {
        let amb = self.ambient();
        // 环境只降温（室温高于环境时向环境散热），绝不被动加热房间；
        // 温度上升只能来自加热器。
        let cooling = ALPHA * (self.temperature - amb).max(0.0);
        let heating = if self.heater_on { HEATER_POWER } else { 0.0 };
        let t = self.temperature + heating - cooling;
        self.temperature = t.clamp(T_MIN, T_MAX);
        self.alarm = match self.alarm {
            AlarmState::Normal => {
                if self.temperature < ALARM_LOW {
                    AlarmState::Low
                } else if self.temperature > ALARM_HIGH {
                    AlarmState::High
                } else {
                    AlarmState::Normal
                }
            }
            AlarmState::Low => {
                if self.temperature >= ALARM_LOW + HYSTERESIS {
                    AlarmState::Normal
                } else {
                    AlarmState::Low
                }
            }
            AlarmState::High => {
                if self.temperature <= ALARM_HIGH - HYSTERESIS {
                    AlarmState::Normal
                } else {
                    AlarmState::High
                }
            }
        };
    }
}

fn ambient_at(t: f64) -> f64 {
    let center = (AMBIENT_MIN + AMBIENT_MAX) / 2.0;
    let amp = (AMBIENT_MAX - AMBIENT_MIN) / 2.0;
    center + amp * (std::f64::consts::TAU * t / AMBIENT_PERIOD_S).sin()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let addr = ea_sysapi::parse_bus_arg();
    let provide = [
        action::HEATER_START,
        action::HEATER_STOP,
        action::TEMPERATURE_READ,
        action::TEMPERATURE_SET_AMBIENT,
    ];
    let actions = [
        ActionSpec {
            method: action::HEATER_START.to_string(),
            description: "启动加热器".to_string(),
            params_schema: json!({"type": "object", "properties": {}}),
        },
        ActionSpec {
            method: action::HEATER_STOP.to_string(),
            description: "停止加热器".to_string(),
            params_schema: json!({"type": "object", "properties": {}}),
        },
        ActionSpec {
            method: action::TEMPERATURE_READ.to_string(),
            description: "读取当前温度".to_string(),
            params_schema: json!({"type": "object", "properties": {}}),
        },
        ActionSpec {
            method: action::TEMPERATURE_SET_AMBIENT.to_string(),
            description: "设定环境温度（hold=true 固定，false 恢复正弦）".to_string(),
            params_schema: json!({
                "type": "object",
                "properties": {"value": {"type": "number"}, "hold": {"type": "boolean"}}
            }),
        },
    ];

    let client = Client::connect(&addr, "hw-temp", &[], &provide, &actions).await?;
    info!("hw-temp connected to {addr}");

    let mut sim = Sim::new();
    // 采样 1 s 一拍（告警判定与 env 更新靠它），但 `event.temperature.updated`
    // **按 `EA_STATUS_REPORT_MS` 节流**（默认 3 分钟）——这条事件会进 Agent 视野，
    // 1 Hz 上报只会淹没对话。网页要实时温度就走 `action.temperature.read`。
    let report_every = status_report_ticks();
    info!("温度事件上报每 {report_every} 个采样周期（EA_STATUS_REPORT_MS）");
    let mut tick_count: u64 = 0;
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                tick_count = tick_count.wrapping_add(1);
                // 首拍先报一次（网页/测试不用等满一个周期），之后按周期节流。
                let report = tick_count == 1 || tick_count % report_every == 0;
                sample(&client, &mut sim, report).await;
            }
            msg = client.next() => {
                match msg {
                    Some(Incoming::Request { id, method, params }) => {
                        handle_request(&client, &mut sim, &id, &method, params).await;
                    }
                    Some(Incoming::Event { .. }) => {}
                    None => break,
                }
            }
        }
    }
    Ok(())
}

/// 上报周期换算成「1 s 采样拍数」。`EA_STATUS_REPORT_MS` 默认 180000（3 分钟）。
fn status_report_ticks() -> u64 {
    let ms = std::env::var("EA_STATUS_REPORT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(180_000);
    (ms / 1000).max(1)
}

async fn sample(client: &Client, sim: &mut Sim, report: bool) {
    let prev_alarm = sim.alarm;
    sim.step();
    let value = sim.temperature;
    let heater_on = sim.heater_on;
    let state = sim.alarm.as_str();

    if report {
        client
            .emit(
                event::TEMPERATURE_UPDATED,
                json!({"value": value, "heater_on": heater_on, "state": state}),
            )
            .await
            .ok();
    }

    let _ = client.env_set("temperature.current", json!(value), true).await;
    let _ = client.env_set("temperature.state", json!(state), false).await;
    let _ = client.env_set("heater.on", json!(heater_on), false).await;

    if sim.alarm != prev_alarm {
        match sim.alarm {
            AlarmState::Low => {
                let _ = client.env_set("alarm.active", json!(true), false).await;
                client
                    .emit(
                        event::TEMPERATURE_ALARM,
                        json!({"level": "low", "value": value, "threshold": ALARM_LOW}),
                    )
                    .await
                    .ok();
            }
            AlarmState::High => {
                let _ = client.env_set("alarm.active", json!(true), false).await;
                client
                    .emit(
                        event::TEMPERATURE_ALARM,
                        json!({"level": "high", "value": value, "threshold": ALARM_HIGH}),
                    )
                    .await
                    .ok();
            }
            AlarmState::Normal => {
                let level = match prev_alarm {
                    AlarmState::Low => "low",
                    _ => "high",
                };
                let _ = client.env_set("alarm.active", json!(false), false).await;
                client
                    .emit(
                        event::TEMPERATURE_ALARM_CLEARED,
                        json!({"level": level, "value": value}),
                    )
                    .await
                    .ok();
            }
        }
    }
}

async fn handle_request(
    client: &Client,
    sim: &mut Sim,
    id: &str,
    m: &str,
    params: ea_proto::Envelope,
) {
    match m {
        method::SYS_PING => {
            let uptime = sim.started.elapsed().as_secs();
            let _ = client.reply(id, json!({"pong": true, "uptime_s": uptime})).await;
        }
        action::HEATER_START => {
            sim.heater_on = true;
            let _ = client.env_set("heater.on", json!(true), false).await;
            let _ = client.reply(id, json!({"heater_on": true})).await;
        }
        action::HEATER_STOP => {
            sim.heater_on = false;
            let _ = client.env_set("heater.on", json!(false), false).await;
            let _ = client.reply(id, json!({"heater_on": false})).await;
        }
        action::TEMPERATURE_READ => {
            let _ = client
                .reply(
                    id,
                    json!({
                        "value": sim.temperature,
                        "state": sim.alarm.as_str(),
                        "heater_on": sim.heater_on,
                        "ts": chrono::Utc::now().to_rfc3339(),
                    }),
                )
                .await;
        }
        action::TEMPERATURE_SET_AMBIENT => {
            let value = params.data.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let hold = params.data.get("hold").and_then(|v| v.as_bool()).unwrap_or(false);
            sim.ambient_hold = if hold { Some(value) } else { None };
            let ambient = sim.ambient();
            let _ = client.reply(id, json!({"ambient": ambient, "hold": hold})).await;
        }
        _ => {
            let _ = client
                .reply_error(id, ea_proto::error::METHOD_NOT_FOUND, "unknown method")
                .await;
        }
    }
}
