//! `motor-ecat`：实时运动控制进程（M5）。
//!
//! **线程模型（本进程的核心约束）**：RT 线程与 RPC 线程彻底分离。
//!
//! ```text
//!   bus ──JSON-RPC/TCP──> RPC 线程（tokio，普通优先级）
//!                          · 收 action.motor.*，只投递命令后立即应答
//!                          · 按 10 Hz 读快照发 event.motor.status / reached / alarm
//!                                │  命令环（无锁 SPSC，AtomicU64 槽）
//!                                │  状态快照（原子字段）
//!                          RT 线程（SCHED_FIFO 80 + 绑核 + 定向 mlock）
//!                          · 4 ms 周期 PDO，只碰 IOmap
//!                          · 不做分配 / 加锁 / 日志 / 总线 I/O
//! ```
//!
//! RT 线程永远不等 RPC：命令是投递式的（`action.motor.move_abs` 立即返回
//! `{"accepted":true}`），运动完成由 `event.motor.reached` 通知。
//!
//! 后端当前为 `Mock`（无硬件也能跑通协议与线程模型）；SOEM 后端接入点见
//! `Backend` trait。

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ea_proto::{action, event, method, ActionSpec};
use ea_sysapi::{Client, Incoming};
use serde_json::{json, Value};
use tracing::{info, warn};

// —— 拓扑常量 ——

/// 轴上数（2 台 MOONS STF06 串联）。
const AXES: usize = 2;
/// PDO 周期（µs）。4 ms 对齐 LinuxWasm 已验证基线。
const CYCLE_US: u64 = 4_000;
/// 状态事件频率：10 Hz（**不是** 250 Hz —— 否则会把总线与 runtime 打爆）。
const STATUS_PERIOD: Duration = Duration::from_millis(100);
/// 命令环容量（定长，无需分配）。
const CMD_RING: usize = 64;
/// 每个周期最多处理多少条命令，防止命令洪泛饿死 PDO 循环。
const CMD_DRAIN_MAX: usize = 16;
/// 期望工作计数：2 个从站 + 1。
const EXPECTED_WKC: u32 = (AXES + 1) as u32;
/// RT 线程优先级与绑核（对齐 LinuxWasm 默认值）。
#[cfg(target_os = "linux")]
const RT_PRIO: i32 = 80;
#[cfg(target_os = "linux")]
const RT_CPU: usize = 3;

// —— 命令编码：op(8) | axis(8) | value(i32) ——

const OP_ENABLE: u64 = 1;
const OP_STOP: u64 = 2;
const OP_MOVE_ABS: u64 = 3;
const OP_MOVE_REL: u64 = 4;
const OP_HOME: u64 = 5;
const OP_SET_VELOCITY: u64 = 6;
/// axis = 0xFF 表示全部轴。
const AXIS_ALL: usize = 0xFF;

#[derive(Clone, Copy, Debug)]
struct Cmd {
    op: u64,
    axis: usize,
    value: i32,
}

fn encode(op: u64, axis: usize, value: i32) -> u64 {
    op | ((axis as u64) << 8) | (((value as u32) as u64) << 16)
}

fn decode(raw: u64) -> Cmd {
    Cmd {
        op: raw & 0xFF,
        axis: ((raw >> 8) & 0xFF) as usize,
        value: (raw >> 16) as u32 as i32,
    }
}

// —— 无锁 SPSC 命令环（RPC → RT）——

struct CmdRing {
    slots: [AtomicU64; CMD_RING],
    head: AtomicUsize,
    tail: AtomicUsize,
}

impl CmdRing {
    fn new() -> Self {
        CmdRing {
            slots: std::array::from_fn(|_| AtomicU64::new(0)),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// 生产者（RPC 线程）。环满返回 false，调用方应答「繁忙」。
    fn push(&self, v: u64) -> bool {
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= CMD_RING {
            return false;
        }
        self.slots[head % CMD_RING].store(v, Ordering::Release);
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// 消费者（RT 线程）。
    fn pop(&self) -> Option<u64> {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let v = self.slots[tail % CMD_RING].load(Ordering::Acquire);
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(v)
    }
}

// —— 状态快照（RT → RPC，原子字段，无锁）——

struct AxisStatus {
    position: AtomicI32,
    velocity: AtomicI32,
    statusword: AtomicU16,
    error: AtomicU16,
    enabled: AtomicBool,
    reached: AtomicBool,
}

impl AxisStatus {
    fn new() -> Self {
        AxisStatus {
            position: AtomicI32::new(0),
            velocity: AtomicI32::new(0),
            statusword: AtomicU16::new(0x0250),
            error: AtomicU16::new(0),
            enabled: AtomicBool::new(false),
            reached: AtomicBool::new(false),
        }
    }
}

struct Shared {
    axes: [AxisStatus; AXES],
    wkc: AtomicU32,
    cycles: AtomicU64,
    cmds: CmdRing,
    /// 急停落点：RPC 直接置位，RT 在下一个周期最优先处理（不排队）。
    estop: AtomicBool,
    shutdown: AtomicBool,
}

impl Shared {
    fn new() -> Self {
        Shared {
            axes: std::array::from_fn(|_| AxisStatus::new()),
            wkc: AtomicU32::new(0),
            cycles: AtomicU64::new(0),
            cmds: CmdRing::new(),
            estop: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
        }
    }

    fn publish(&self, out: &CycleOut) {
        for a in 0..AXES {
            self.axes[a].position.store(out.position[a], Ordering::Relaxed);
            self.axes[a].velocity.store(out.velocity[a], Ordering::Relaxed);
            self.axes[a].statusword.store(out.statusword[a], Ordering::Relaxed);
            self.axes[a].error.store(out.error[a], Ordering::Relaxed);
            self.axes[a].enabled.store(out.enabled[a], Ordering::Relaxed);
            self.axes[a].reached.store(out.reached[a], Ordering::Relaxed);
        }
        self.wkc.store(out.wkc, Ordering::Relaxed);
    }

    fn snapshot(&self) -> Value {
        let axes: Vec<Value> = (0..AXES)
            .map(|a| {
                let s = &self.axes[a];
                json!({
                    "axis": a,
                    "position": s.position.load(Ordering::Relaxed),
                    "velocity": s.velocity.load(Ordering::Relaxed),
                    "statusword": s.statusword.load(Ordering::Relaxed),
                    "error": s.error.load(Ordering::Relaxed),
                    "enabled": s.enabled.load(Ordering::Relaxed),
                })
            })
            .collect();
        json!({
            "axes": axes,
            "wkc": self.wkc.load(Ordering::Relaxed),
            "cycles": self.cycles.load(Ordering::Relaxed),
        })
    }
}

// —— 后端抽象 ——

#[derive(Clone, Copy, Default)]
struct CycleOut {
    position: [i32; AXES],
    velocity: [i32; AXES],
    statusword: [u16; AXES],
    error: [u16; AXES],
    enabled: [bool; AXES],
    reached: [bool; AXES],
    wkc: u32,
}

trait Backend: Send {
    fn apply(&mut self, cmd: Cmd);
    fn cycle(&mut self) -> CycleOut;
}

/// 软件仿真后端：无硬件时验证线程模型与协议。
///
/// 语义对齐开环步进：位置是驱动器自生成的微步计数（电机不动也会涨）。
struct MockBackend {
    position: [i64; AXES],
    target: [i64; AXES],
    velocity: [i64; AXES],
    enabled: [bool; AXES],
}

impl MockBackend {
    fn new() -> Self {
        MockBackend {
            position: [0; AXES],
            target: [0; AXES],
            velocity: [20_000; AXES], // 20000 µstep/s = 1 rev/s（MOONS 微步/圈）
            enabled: [false; AXES],
        }
    }

    fn each_axis(&mut self, axis: usize, mut f: impl FnMut(&mut Self, usize)) {
        if axis == AXIS_ALL {
            for a in 0..AXES {
                f(self, a);
            }
        } else if axis < AXES {
            f(self, axis);
        }
    }
}

impl Backend for MockBackend {
    fn apply(&mut self, cmd: Cmd) {
        let value = cmd.value as i64;
        match cmd.op {
            OP_ENABLE => self.each_axis(cmd.axis, |s, a| {
                s.enabled[a] = true;
                s.target[a] = s.position[a];
            }),
            OP_STOP => self.each_axis(cmd.axis, |s, a| {
                s.enabled[a] = false;
                s.target[a] = s.position[a];
            }),
            OP_MOVE_ABS => self.each_axis(cmd.axis, |s, a| {
                s.enabled[a] = true;
                s.target[a] = value;
            }),
            OP_MOVE_REL => self.each_axis(cmd.axis, |s, a| {
                s.enabled[a] = true;
                s.target[a] = s.position[a] + value;
            }),
            OP_HOME => self.each_axis(cmd.axis, |s, a| {
                // 开环无原点传感器：软件置零，不做机械回零。
                s.position[a] = 0;
                s.target[a] = 0;
            }),
            OP_SET_VELOCITY => self.each_axis(cmd.axis, |s, a| {
                if value > 0 {
                    s.velocity[a] = value;
                }
            }),
            _ => {}
        }
    }

    fn cycle(&mut self) -> CycleOut {
        let mut out = CycleOut {
            wkc: EXPECTED_WKC,
            ..Default::default()
        };
        for a in 0..AXES {
            if self.enabled[a] {
                let delta = self.target[a] - self.position[a];
                let max_step = (self.velocity[a] * CYCLE_US as i64 / 1_000_000).max(1);
                let mv = delta.clamp(-max_step, max_step);
                self.position[a] += mv;
            }
            let at_target = self.position[a] == self.target[a];
            out.position[a] = self.position[a] as i32;
            out.velocity[a] = if self.enabled[a] && !at_target {
                self.velocity[a] as i32
            } else {
                0
            };
            // CiA402：0x0250 Switch on disabled / 0x0027 Operation enabled
            //         / bit10 = Target reached
            out.statusword[a] = if self.enabled[a] {
                0x0027 | if at_target { 0x0400 } else { 0 }
            } else {
                0x0250
            };
            out.enabled[a] = self.enabled[a];
            out.reached[a] = self.enabled[a] && at_target;
        }
        out
    }
}

// —— 实时循环 ——

fn rt_loop(shared: Arc<Shared>, mut backend: Box<dyn Backend>) {
    rt_setup(&shared);
    info!("[rt] 4 ms PDO 循环启动（backend={}）", backend_name());

    let mut deadline = clock::Deadline::now();
    let mut n: u64 = 0;
    while !shared.shutdown.load(Ordering::Relaxed) {
        deadline.advance(CYCLE_US);

        // 1) 急停最优先：不排队、不等前面的命令。
        if shared.estop.swap(false, Ordering::AcqRel) {
            backend.apply(Cmd { op: OP_STOP, axis: AXIS_ALL, value: 0 });
        }

        // 2) 排空命令（有上限，防命令洪泛饿死 PDO）。
        for _ in 0..CMD_DRAIN_MAX {
            let Some(raw) = shared.cmds.pop() else { break };
            backend.apply(decode(raw));
        }

        // 3) 一个 PDO 周期。
        let out = backend.cycle();

        // 4) 发布快照（原子写，无锁、无分配）。
        shared.publish(&out);
        n = n.wrapping_add(1);
        shared.cycles.store(n, Ordering::Relaxed);

        // 5) 绝对时间睡眠（CLOCK_MONOTONIC + TIMER_ABSTIME，不累积漂移）。
        deadline.sleep();
    }
    info!("[rt] 循环退出，共 {n} 个周期");
}

fn backend_name() -> &'static str {
    "mock"
}

#[cfg(target_os = "linux")]
fn rt_setup(shared: &Shared) {
    use std::mem::size_of;
    unsafe {
        // 允许用 EA_RT=0 关掉实时（排查抖动/权限问题时的开关）。
        let enabled = std::env::var("EA_RT").map(|v| v != "0").unwrap_or(true);

        if enabled {
            let mut param: libc::sched_param = std::mem::zeroed();
            param.sched_priority = RT_PRIO;
            if libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) != 0 {
                warn!(
                    "[rt] sched_setscheduler(SCHED_FIFO, {RT_PRIO}) 失败（需 root 或 CAP_SYS_NICE），降级为普通调度"
                );
            } else {
                info!("[rt] SCHED_FIFO prio={RT_PRIO}");
            }

            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_ZERO(&mut set);
            libc::CPU_SET(RT_CPU as libc::c_int, &mut set);
            if libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) != 0 {
                warn!("[rt] sched_setaffinity(CPU{RT_CPU}) 失败");
            } else {
                info!("[rt] 绑定 CPU{RT_CPU}");
            }

            // 定向 mlock —— **绝不 mlockall**：锁住整个进程会破坏 AF_PACKET
            // raw socket（周期 wkc 塌到 1、TxPDO 全零、伺服 Err 80）。
            let p = shared as *const Shared as *const libc::c_void;
            if libc::mlock(p, size_of::<Shared>()) != 0 {
                warn!("[rt] mlock(Shared) 失败（需 CAP_IPC_LOCK 或足够 RLIMIT_MEMLOCK）");
            } else {
                info!("[rt] 定向 mlock Shared（{} B）", size_of::<Shared>());
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn rt_setup(_shared: &Shared) {
    warn!("[rt] 非 Linux 目标：跳过 SCHED_FIFO / 绑核 / mlock（仅用于开发验证）");
}

/// 绝对时间调度：Linux 用 CLOCK_MONOTONIC + TIMER_ABSTIME；其它平台退回 Instant。
#[cfg(target_os = "linux")]
mod clock {
    pub struct Deadline(libc::timespec);

    impl Deadline {
        pub fn now() -> Self {
            let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
            Deadline(ts)
        }

        pub fn advance(&mut self, us: u64) {
            self.0.tv_sec += (us / 1_000_000) as libc::time_t;
            self.0.tv_nsec += ((us % 1_000_000) * 1000) as libc::c_long;
            if self.0.tv_nsec >= 1_000_000_000 {
                self.0.tv_nsec -= 1_000_000_000;
                self.0.tv_sec += 1;
            }
        }

        pub fn sleep(&self) {
            // 被信号打断返回 EINTR：循环重试，语义上仍是「睡到绝对时刻」。
            while unsafe {
                libc::clock_nanosleep(
                    libc::CLOCK_MONOTONIC,
                    libc::TIMER_ABSTIME,
                    &self.0,
                    std::ptr::null_mut(),
                )
            } == libc::EINTR
            {}
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod clock {
    use std::time::{Duration, Instant};

    pub struct Deadline(Instant);

    impl Deadline {
        pub fn now() -> Self {
            Deadline(Instant::now())
        }

        pub fn advance(&mut self, us: u64) {
            self.0 += Duration::from_micros(us);
        }

        pub fn sleep(&self) {
            let now = Instant::now();
            if self.0 > now {
                std::thread::sleep(self.0 - now);
            }
        }
    }
}

// —— RPC 线程 ——

/// 路由登记（`provide`）——总线按它把请求转给本进程；少了它请求会报 no provider。
const PROVIDES: &[&str] = &[
    action::MOTOR_ENABLE,
    action::MOTOR_STOP,
    action::MOTOR_MOVE_ABS,
    action::MOTOR_MOVE_REL,
    action::MOTOR_HOME,
    action::MOTOR_SET_VELOCITY,
    action::MOTOR_READ,
];

fn actions() -> Vec<ActionSpec> {
    vec![
        ActionSpec {
            method: action::MOTOR_ENABLE.to_string(),
            description: "使能电机轴（axis 省略=全部）".to_string(),
            params_schema: json!({"type":"object","properties":{"axis":{"type":"integer"}}}),
        },
        ActionSpec {
            method: action::MOTOR_STOP.to_string(),
            description: "急停：置零并断使能（axis 省略=全部），立即生效".to_string(),
            params_schema: json!({"type":"object","properties":{"axis":{"type":"integer"}}}),
        },
        ActionSpec {
            method: action::MOTOR_MOVE_ABS.to_string(),
            description: "绝对定位（投递即返回，完成发 event.motor.reached）".to_string(),
            params_schema: json!({
                "type":"object",
                "properties":{"axis":{"type":"integer"},"position":{"type":"integer"}},
                "required":["axis","position"]
            }),
        },
        ActionSpec {
            method: action::MOTOR_MOVE_REL.to_string(),
            description: "相对定位（投递即返回）".to_string(),
            params_schema: json!({
                "type":"object",
                "properties":{"axis":{"type":"integer"},"delta":{"type":"integer"}},
                "required":["axis","delta"]
            }),
        },
        ActionSpec {
            method: action::MOTOR_HOME.to_string(),
            description: "软件置零（开环步进无原点传感器）".to_string(),
            params_schema: json!({"type":"object","properties":{"axis":{"type":"integer"}}}),
        },
        ActionSpec {
            method: action::MOTOR_SET_VELOCITY.to_string(),
            description: "设置速度（counts/s，20000=1 圈/s）".to_string(),
            params_schema: json!({
                "type":"object",
                "properties":{"axis":{"type":"integer"},"velocity":{"type":"integer"}},
                "required":["axis","velocity"]
            }),
        },
        ActionSpec {
            method: action::MOTOR_READ.to_string(),
            description: "读取当前轴状态快照".to_string(),
            params_schema: json!({"type":"object","properties":{}}),
        },
    ]
}

fn parse_axis(data: &Value) -> Result<usize, String> {
    match data.get("axis") {
        None | Some(Value::Null) => Ok(AXIS_ALL),
        Some(v) => match v.as_u64() {
            Some(a) if (a as usize) < AXES => Ok(a as usize),
            _ => Err(format!("axis 必须是 0..{AXES} 的整数")),
        },
    }
}

fn parse_i32(data: &Value, key: &str) -> Result<i32, String> {
    data.get(key)
        .and_then(|v| v.as_i64())
        .map(|v| v.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
        .ok_or_else(|| format!("缺少整数参数 {key}"))
}

async fn rpc_loop(shared: Arc<Shared>, addr: String) -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::connect(&addr, "motor-ecat", &[], PROVIDES, &actions()).await?;
    info!("[rpc] 已连接总线 {addr}");

    let mut tick = tokio::time::interval(STATUS_PERIOD);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut last_reached = [false; AXES];
    let mut last_alarm = false;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                // 10 Hz 状态（在 ignore 列表里，不触发推理）
                let _ = client.emit(event::MOTOR_STATUS, shared.snapshot()).await;

                // 到位沿（只在 false→true 时发一次）
                for a in 0..AXES {
                    let reached = shared.axes[a].reached.load(Ordering::Relaxed);
                    if reached && !last_reached[a] {
                        let _ = client.emit(event::MOTOR_REACHED, json!({
                            "axis": a,
                            "position": shared.axes[a].position.load(Ordering::Relaxed),
                        })).await;
                    }
                    last_reached[a] = reached;
                }

                // 告警沿：伺服错误码非零，或 EtherCAT 工作计数异常
                let wkc = shared.wkc.load(Ordering::Relaxed);
                let err_axis = (0..AXES).find(|&a| shared.axes[a].error.load(Ordering::Relaxed) != 0);
                let alarm = wkc != EXPECTED_WKC || err_axis.is_some();
                if alarm && !last_alarm {
                    let payload = match err_axis {
                        Some(a) => json!({
                            "kind": "servo",
                            "axis": a,
                            "error": shared.axes[a].error.load(Ordering::Relaxed),
                        }),
                        None => json!({"kind": "wkc", "wkc": wkc, "expected": EXPECTED_WKC}),
                    };
                    let _ = client.emit(event::MOTOR_ALARM, payload).await;
                }
                last_alarm = alarm;
            }

            incoming = client.next() => {
                match incoming {
                    None => break,
                    Some(Incoming::Request { id, method: m, params }) => {
                        let data = &params.data;
                        match m.as_str() {
                            method::SYS_PING => {
                                let _ = client.reply(&id, json!({"pong": true})).await;
                            }
                            action::MOTOR_READ => {
                                let _ = client.reply(&id, shared.snapshot()).await;
                            }
                            action::MOTOR_STOP => {
                                // 急停：直接置位落点，RT 下一周期最优先执行。
                                shared.estop.store(true, Ordering::Release);
                                let _ = client.reply(&id, json!({"accepted": true, "estop": true})).await;
                            }
                            _ => {
                                let (op, value) = match m.as_str() {
                                    action::MOTOR_ENABLE => (OP_ENABLE, 0),
                                    action::MOTOR_MOVE_ABS => match parse_i32(data, "position") {
                                        Ok(v) => (OP_MOVE_ABS, v),
                                        Err(e) => {
                                            let _ = client.reply_error(&id, ea_proto::error::INVALID_PARAMS, &e).await;
                                            continue;
                                        }
                                    },
                                    action::MOTOR_MOVE_REL => match parse_i32(data, "delta") {
                                        Ok(v) => (OP_MOVE_REL, v),
                                        Err(e) => {
                                            let _ = client.reply_error(&id, ea_proto::error::INVALID_PARAMS, &e).await;
                                            continue;
                                        }
                                    },
                                    action::MOTOR_HOME => (OP_HOME, 0),
                                    action::MOTOR_SET_VELOCITY => match parse_i32(data, "velocity") {
                                        Ok(v) => (OP_SET_VELOCITY, v),
                                        Err(e) => {
                                            let _ = client.reply_error(&id, ea_proto::error::INVALID_PARAMS, &e).await;
                                            continue;
                                        }
                                    },
                                    _ => {
                                        let _ = client
                                            .reply_error(&id, ea_proto::error::METHOD_NOT_FOUND, "unknown method")
                                            .await;
                                        continue;
                                    }
                                };

                                let axis = match parse_axis(data) {
                                    Ok(a) => a,
                                    Err(e) => {
                                        let _ = client.reply_error(&id, ea_proto::error::INVALID_PARAMS, &e).await;
                                        continue;
                                    }
                                };

                                if shared.cmds.push(encode(op, axis, value)) {
                                    // 投递即返回：绝不等运动完成（完成靠 event.motor.reached）。
                                    let _ = client.reply(&id, json!({"accepted": true})).await;
                                } else {
                                    let _ = client
                                        .reply_error(&id, ea_proto::error::INTERNAL_ERROR, "command ring full")
                                        .await;
                                }
                            }
                        }
                    }
                    Some(Incoming::Event { .. }) => {}
                }
            }
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let bus_addr = ea_sysapi::parse_bus_arg();

    // 后端尚未接入真实 EtherCAT：显式警告，避免误以为在驱动硬件。
    warn!("后端 = mock（软件仿真，**不驱动真实电机**）；SOEM 后端接入点见 Backend trait");

    let shared = Arc::new(Shared::new());

    // 先起 RT 线程，再做任何可能阻塞的事情。
    let rt_shared = shared.clone();
    std::thread::Builder::new()
        .name("pdo-rt".to_string())
        .spawn(move || rt_loop(rt_shared, Box::new(MockBackend::new())))?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(rpc_loop(shared.clone(), bus_addr))?;

    shared.shutdown.store(true, Ordering::Release);
    Ok(())
}
