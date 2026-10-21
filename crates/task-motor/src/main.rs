//! `task-motor`：电机演示任务（Wasm 访客，M5）。
//!
//! 提供 `action.task.motor_demo`：**顺时针 n 圈 → 逆时针 m 圈 → 停机**，速度可配。
//! 这是「Agent 驱动真实硬件」的最小演示——`motor-ecat` 负责 4 ms 周期的实时
//! PDO 循环，本进程只是编排动作（读起点 → 使能 → 设速度 → 两次绝对定位 → 停机），
//! 全程只经统一 RPC，不碰硬件。
//!
//! 两个设计点：
//! - **立即应答**：动作秒级持续，先回 `{"started":true}` 再做，避免总线请求超时；
//!   完成由 `event.task.stopped` 通知。
//! - **等待期间不哑掉**：轮询用 `next_timeout` 的 100 ms 节拍，顺带处理 `sys.ping`，
//!   否则心跳会把这个正忙着的进程判成软挂。

use std::time::{Duration, Instant};

use ea_proto::{action, error, event, method, ActionSpec};
use ea_wasmapi::{Client, Incoming};
use serde_json::{json, Value};

/// MOONS STF06 微步/圈（见 CLAUDE.md「MOONS STF06-ECX-mini」）。
/// 换算：`velocity = 20000` 即 1 圈/s（60 rpm）。
const USTEPS_PER_REV: f64 = 20_000.0;
/// 到位判定的位置容差（µsteps = 1/100 圈）。开环无编码器，`0x6064` 是驱动器
/// 自己生成的指令计数，收敛后一般精确等于目标；留余量吸收量化/取整误差。
const POS_TOLERANCE: i64 = 200;
/// 轮询节拍：既是等待睡眠，也是处理入站请求（sys.ping）的机会。
const TICK: Duration = Duration::from_millis(100);
/// 连续多少个节拍稳定在容差内就算到位（驱动器没给 bit10 时的兜底）。
const STABLE_TICKS: u32 = 3;

fn main() {
    let addr = ea_wasmapi::parse_bus_arg();
    let mut client = match Client::connect(&addr) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[task-motor] connect {addr} 失败: {e:?}");
            std::process::exit(1);
        }
    };

    let actions = [ActionSpec {
        method: action::TASK_MOTOR_DEMO.to_string(),
        description: "电机演示：顺时针 n 圈 → 逆时针 m 圈 → 停机".to_string(),
        params_schema: json!({
            "type": "object",
            "properties": {
                "turns_cw":  {"type": "number",  "description": "顺时针圈数（默认 2）"},
                "turns_ccw": {"type": "number",  "description": "逆时针圈数（默认 2）"},
                "speed":     {"type": "integer", "description": "速度 counts/s，20000 = 1 圈/s（默认 20000）"},
                "axis":      {"type": "integer", "description": "轴号（默认 0）"}
            }
        }),
    }];
    if let Err(e) = client.register("task-motor", &[], &[action::TASK_MOTOR_DEMO], &actions) {
        eprintln!("[task-motor] register 失败: {e:?}");
        std::process::exit(1);
    }
    eprintln!("[task-motor] connected to {addr}");

    loop {
        match client.next() {
            None => break,
            Some(Incoming::Request { id, method: m, params }) => match m.as_str() {
                action::TASK_MOTOR_DEMO => match DemoParams::parse(&params.data) {
                    Err(e) => {
                        let _ = client.reply_error(&id, error::INVALID_PARAMS, &e);
                    }
                    Ok(p) => {
                        // 先应答（任务要跑数秒），再干活；完成用事件通知。
                        let _ = client.reply(
                            &id,
                            json!({
                                "started": true,
                                "task": "motor_demo",
                                "axis": p.axis,
                                "turns_cw": p.turns_cw,
                                "turns_ccw": p.turns_ccw,
                                "speed": p.speed,
                            }),
                        );
                        run_demo(&mut client, &p);
                    }
                },
                method::SYS_PING => {
                    let _ = client.reply(&id, json!({"pong": true}));
                }
                _ => {
                    let _ = client.reply_error(&id, error::METHOD_NOT_FOUND, "unknown method");
                }
            },
            Some(Incoming::Event { .. }) => {}
        }
    }
}

/// 演示参数。全部可选；缺省 = 2 圈顺、2 圈逆、1 圈/s、轴 0。
struct DemoParams {
    axis: u32,
    turns_cw: f64,
    turns_ccw: f64,
    speed: i64,
}

impl DemoParams {
    fn parse(d: &Value) -> Result<Self, String> {
        let axis = d.get("axis").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let turns_cw = num(d, "turns_cw")?.unwrap_or(2.0);
        let turns_ccw = num(d, "turns_ccw")?.unwrap_or(2.0);
        let speed = num(d, "speed")?.unwrap_or(USTEPS_PER_REV).round() as i64;

        if turns_cw < 0.0 || turns_ccw < 0.0 {
            return Err("turns_cw / turns_ccw 必须 ≥ 0".to_string());
        }
        if speed <= 0 {
            return Err("speed 必须 > 0（counts/s，20000 = 1 圈/s）".to_string());
        }
        Ok(DemoParams { axis, turns_cw, turns_ccw, speed })
    }
}

fn num(d: &Value, key: &str) -> Result<Option<f64>, String> {
    match d.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| format!("{key} 必须是数字")),
    }
}

/// 跑完整段演示，**任何分支都以停机收尾**。
fn run_demo(client: &mut Client, p: &DemoParams) {
    let _ = client.env_set("task.motor_demo.running", json!(true), false);
    let _ = client.emit(
        event::TASK_STARTED,
        json!({
            "task": "motor_demo",
            "axis": p.axis,
            "turns_cw": p.turns_cw,
            "turns_ccw": p.turns_ccw,
            "speed": p.speed,
        }),
    );
    eprintln!(
        "[task-motor] motor_demo start: axis={} cw={} ccw={} speed={}",
        p.axis, p.turns_cw, p.turns_ccw, p.speed
    );

    let reason = match demo_sequence(client, p) {
        Ok(()) => "done",
        Err(e) => {
            eprintln!("[task-motor] motor_demo 中止: {e}");
            "error"
        }
    };

    // 收尾必须停机：即使上面出错，也不能把轴留在使能+运动的状态。
    // motor.stop 走 motor-ecat 的急停独立路径，不排队。
    let _ = client.call(action::MOTOR_STOP, json!({"axis": p.axis}));
    let _ = client.env_set("task.motor_demo.running", json!(false), false);
    let _ = client.emit(event::TASK_STOPPED, json!({"task": "motor_demo", "reason": reason}));
    eprintln!("[task-motor] motor_demo stop: reason={reason}");
}

fn demo_sequence(client: &mut Client, p: &DemoParams) -> Result<(), String> {
    let start = read_axis(client, p.axis)?.position;

    client
        .call(action::MOTOR_ENABLE, json!({"axis": p.axis}))
        .map_err(|e| format!("enable 失败: {e:?}"))?;
    client
        .call(action::MOTOR_SET_VELOCITY, json!({"axis": p.axis, "velocity": p.speed}))
        .map_err(|e| format!("set_velocity 失败: {e:?}"))?;

    // 顺时针：+ 方向 = 圈数 × 微步/圈（MOONS 上 + 为顺时针，见 CLAUDE.md）。
    let cw_target = start + (p.turns_cw * USTEPS_PER_REV).round() as i64;
    move_and_wait(client, p, start, cw_target)?;

    // 逆时针：从顺针终点往回走 m 圈。
    let ccw_target = cw_target - (p.turns_ccw * USTEPS_PER_REV).round() as i64;
    move_and_wait(client, p, cw_target, ccw_target)?;

    Ok(())
}

/// 绝对定位并等到达。等待期间照常应答 `sys.ping`（否则心跳判软挂）。
fn move_and_wait(client: &mut Client, p: &DemoParams, from: i64, target: i64) -> Result<(), String> {
    if target > i32::MAX as i64 || target < i32::MIN as i64 {
        return Err(format!("目标位置 {target} 超出 int32 范围"));
    }
    client
        .call(action::MOTOR_MOVE_ABS, json!({"axis": p.axis, "position": target}))
        .map_err(|e| format!("move_abs({target}) 失败: {e:?}"))?;
    eprintln!("[task-motor] move_abs: {from} -> {target}");

    // 保守超时：按距离/速度估时 ×2，再留 5 s 余量。
    let travel = (target - from).unsigned_abs() as f64 / p.speed as f64;
    let timeout = Duration::from_secs_f64(travel * 2.0 + 5.0);
    let deadline = Instant::now() + timeout;
    let mut stable = 0u32;

    loop {
        // 一个节拍：先处理入站请求（非阻塞语义，无请求则睡满 100 ms）。
        match client.next_timeout(TICK) {
            Some(Incoming::Request { id, method: m, .. }) if m == method::SYS_PING => {
                let _ = client.reply(&id, json!({"pong": true}));
            }
            Some(Incoming::Request { id, .. }) => {
                let _ = client.reply_error(&id, error::METHOD_NOT_FOUND, "task busy: motor_demo running");
            }
            _ => {}
        }

        let ax = read_axis(client, p.axis)?;
        if ax.error != 0 {
            return Err(format!("轴 {} 报警 error=0x{:04X}", p.axis, ax.error));
        }

        let at_target = (ax.position - target).abs() <= POS_TOLERANCE;
        if at_target {
            // statusword bit10 = Target reached；驱动器没给就用「连续稳定」兜底。
            if ax.reached {
                return Ok(());
            }
            stable += 1;
            if stable >= STABLE_TICKS {
                return Ok(());
            }
        } else {
            stable = 0;
        }

        if Instant::now() > deadline {
            return Err(format!(
                "等到达超时（{timeout:?}）：目标 {target}，当前 {}",
                ax.position
            ));
        }
    }
}

struct AxisSnapshot {
    position: i64,
    error: i64,
    reached: bool,
}

/// 读单轴快照（`action.motor.read` 返回全部轴，这里取一个）。
fn read_axis(client: &mut Client, axis: u32) -> Result<AxisSnapshot, String> {
    let snap = client
        .call(action::MOTOR_READ, json!({}))
        .map_err(|e| format!("motor.read 失败: {e:?}"))?;
    let axes = snap
        .get("axes")
        .and_then(|a| a.as_array())
        .ok_or("motor.read 返回缺少 axes")?;
    let ax = axes
        .get(axis as usize)
        .ok_or_else(|| format!("轴 {axis} 不存在（总线上 {} 轴）", axes.len()))?;
    Ok(AxisSnapshot {
        position: ax.get("position").and_then(|v| v.as_i64()).unwrap_or(0),
        error: ax.get("error").and_then(|v| v.as_i64()).unwrap_or(0),
        reached: ax.get("reached").and_then(|v| v.as_bool()).unwrap_or(false),
    })
}
