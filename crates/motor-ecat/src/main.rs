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
                    // 到位标志：让任务进程能靠快照轮询，不必依赖事件时序
                    //（事件可能在本进程订阅之前就发过了）。
                    "reached": s.reached.load(Ordering::Relaxed),
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

// —— SOEM 后端（真实 EtherCAT，仅 Linux）——

/// MOONS STF06 默认 PDO 布局（见 CLAUDE.md「MOONS STF06」节）：
///
/// ```text
/// RxPDO 13B: [0-1]0x6040 controlword | [2]0x6060 op_mode | [3-6]0x607A target_pos
///            | [7-8]0x60B8 | [9-12]0x60FE:01
/// TxPDO 35B: [0-1]0x603F error | [2-3]0x6041 statusword | [4]0x6061 mode
///            | [5-8]0x6064 actual_pos | [9-12]0x606C actual_vel | ...
/// ```
#[cfg(target_os = "linux")]
mod soem {
    use super::*;
    use std::ffi::CString;

    const OUT_MIN: i32 = 13;
    const IN_MIN: i32 = 13;
    /// CiA402 op_mode：1 = Profile Position (PP)。
    const OP_MODE_PP: u8 = 1;
    /// 新目标沿需要保持 ≥2 个 pump 周期（4 ms × 3 ≈ 12 ms）。
    const ARM_CYCLES: u8 = 3;

    #[repr(C)]
    struct EaSlave {
        outputs: *mut u8,
        inputs: *mut u8,
        output_bytes: i32,
        input_bytes: i32,
        vendor_id: u32,
        product_code: u32,
        al_state: u16,
    }

    extern "C" {
        fn ea_ecat_init(ifname: *const std::os::raw::c_char) -> i32;
        fn ea_ecat_to_op(slave: i32) -> i32;
        fn ea_ecat_shutdown();
        fn ea_ecat_slave(idx: i32, out: *mut EaSlave) -> i32;
        fn ea_ecat_pdo_cycle() -> i32;
    }

    struct Slave {
        outputs: *mut u8,
        inputs: *mut u8,
    }

    struct Axis {
        enabled: bool,
        /// 0=Shutdown(0x06) 1=SwitchOn(0x07) 2=EnableOp(0x0F)，逐周期推进。
        enable_step: u8,
        target_raw: i32,
        arm_cycles: u8,
        home_offset: i32,
        last_raw: i32,
        velocity: i32,
    }

    impl Axis {
        fn new() -> Self {
            Axis {
                enabled: false,
                enable_step: 0,
                target_raw: 0,
                arm_cycles: 0,
                home_offset: 0,
                last_raw: 0,
                velocity: 20_000,
            }
        }
    }

    pub struct SoemBackend {
        slaves: Vec<Slave>,
        axes: [Axis; AXES],
    }

    // SAFETY: 后端只在单个 RT 线程内使用；裸指针来自 SOEM 的 IOmap，
    // 生命周期由 ea_ecat_shutdown()（Drop）保证。
    unsafe impl Send for SoemBackend {}

    impl SoemBackend {
        pub fn new(ifname: &str) -> Result<Self, String> {
            let c = CString::new(ifname).map_err(|e| format!("接口名含 NUL：{e}"))?;
            let n = unsafe { ea_ecat_init(c.as_ptr()) };
            if n <= 0 {
                return Err(format!("ea_ecat_init({ifname}) 失败，错误码 {n}"));
            }
            if (n as usize) < AXES {
                unsafe { ea_ecat_shutdown() };
                return Err(format!("总线上只有 {n} 个从站，需要 {AXES} 个"));
            }

            let mut slaves = Vec::with_capacity(AXES);
            for i in 0..AXES {
                let mut s = EaSlave {
                    outputs: std::ptr::null_mut(),
                    inputs: std::ptr::null_mut(),
                    output_bytes: 0,
                    input_bytes: 0,
                    vendor_id: 0,
                    product_code: 0,
                    al_state: 0,
                };
                if unsafe { ea_ecat_slave(i as i32, &mut s) } != 0 {
                    unsafe { ea_ecat_shutdown() };
                    return Err(format!("读取从站 {i} 信息失败"));
                }
                if s.output_bytes < OUT_MIN || s.input_bytes < IN_MIN {
                    unsafe { ea_ecat_shutdown() };
                    return Err(format!(
                        "从站 {i} 的 PDO 太小（RxPDO {} B / TxPDO {} B，至少各 {OUT_MIN} B）——\
                         用 --probe 核对 PDO 映射",
                        s.output_bytes, s.input_bytes
                    ));
                }
                info!(
                    "[soem] slave {i}: vendor=0x{:x} product=0x{:x} Rx={}B Tx={}B",
                    s.vendor_id, s.product_code, s.output_bytes, s.input_bytes
                );
                slaves.push(Slave { outputs: s.outputs, inputs: s.inputs });
            }

            // ESM：先全部进 OP，再靠 PDO 使能（不能反过来）。
            for i in 1..=AXES {
                let rc = unsafe { ea_ecat_to_op(i as i32) };
                if rc != 0 {
                    unsafe { ea_ecat_shutdown() };
                    return Err(format!("从站 {i} 无法进入 OP，错误码 {rc}"));
                }
            }

            Ok(SoemBackend { slaves, axes: std::array::from_fn(|_| Axis::new()) })
        }

        fn each_axis(&mut self, axis: usize, mut f: impl FnMut(&mut Axis)) {
            if axis == AXIS_ALL {
                for a in &mut self.axes {
                    f(a);
                }
            } else if axis < AXES {
                f(&mut self.axes[axis]);
            }
        }
    }

    impl Drop for SoemBackend {
        fn drop(&mut self) {
            unsafe { ea_ecat_shutdown() };
        }
    }

    /// 扫描并打印从站信息；**不进入 OP、不使能伺服**。
    pub fn probe(ifname: &str) -> Result<(), Box<dyn std::error::Error>> {
        let c = CString::new(ifname)?;
        let n = unsafe { ea_ecat_init(c.as_ptr()) };
        if n <= 0 {
            return Err(format!("ea_ecat_init({ifname}) 失败，错误码 {n}").into());
        }
        println!("接口 {ifname}：发现 {n} 个从站");
        for i in 0..n {
            let mut s = EaSlave {
                outputs: std::ptr::null_mut(),
                inputs: std::ptr::null_mut(),
                output_bytes: 0,
                input_bytes: 0,
                vendor_id: 0,
                product_code: 0,
                al_state: 0,
            };
            if unsafe { ea_ecat_slave(i, &mut s) } != 0 {
                println!("  slave {i}: 读取失败");
                continue;
            }
            println!(
                "  slave {i}: vendor=0x{:x} product=0x{:x} RxPDO={}B TxPDO={}B AL=0x{:x}",
                s.vendor_id, s.product_code, s.output_bytes, s.input_bytes, s.al_state
            );
            if s.output_bytes < OUT_MIN || s.input_bytes < IN_MIN {
                println!(
                    "    ⚠️  PDO 尺寸小于预期（Rx≥{OUT_MIN} B / Tx≥{IN_MIN} B）——核对 PDO 映射"
                );
            }
        }
        unsafe { ea_ecat_shutdown() };
        println!("探测完成：未进 OP、未使能伺服");
        Ok(())
    }

    impl Backend for SoemBackend {
        fn apply(&mut self, cmd: Cmd) {
            let value = cmd.value;
            match cmd.op {
                OP_ENABLE => self.each_axis(cmd.axis, |a| {
                    if !a.enabled {
                        a.enabled = true;
                        a.enable_step = 0;
                    }
                }),
                OP_STOP => self.each_axis(cmd.axis, |a| {
                    a.enabled = false;
                    a.enable_step = 0;
                    a.arm_cycles = 0;
                }),
                OP_MOVE_ABS => self.each_axis(cmd.axis, |a| {
                    a.enabled = true;
                    // 目标按各轴自己的 home 偏移换算，Axis_ALL 时也正确。
                    a.target_raw = value.saturating_add(a.home_offset);
                    a.arm_cycles = ARM_CYCLES;
                }),
                OP_MOVE_REL => self.each_axis(cmd.axis, |a| {
                    a.enabled = true;
                    a.target_raw = a.last_raw.saturating_add(value);
                    a.arm_cycles = ARM_CYCLES;
                }),
                OP_HOME => self.each_axis(cmd.axis, |a| {
                    // 软件置零：不驱动电机，只把当前位置定义为 0。
                    a.home_offset = a.last_raw;
                    a.target_raw = a.last_raw;
                    a.arm_cycles = 0;
                }),
                OP_SET_VELOCITY => {
                    // 注意：MOONS 默认 RxPDO 没有 0x60FF，PP 模式的速度由 0x6081(SDO) 决定。
                    // 这里只记录数值，接真实硬件时不生效（需 PRE_OP 写 SDO 或重映射 PDO）。
                    self.each_axis(cmd.axis, |a| a.velocity = value);
                }
                _ => {}
            }
        }

        fn cycle(&mut self) -> CycleOut {
            let mut out = CycleOut { wkc: 0, ..Default::default() };

            // 1) 写 RxPDO（必须先写再发）。
            for i in 0..AXES {
                let p = self.slaves[i].outputs;
                let ax = &mut self.axes[i];

                let mut cw: u16 = if ax.enabled {
                    ax.enable_step = (ax.enable_step + 1).min(2);
                    match ax.enable_step {
                        0 => 0x0006, // Shutdown
                        1 => 0x0007, // Switch on
                        _ => 0x000F, // Enable operation
                    }
                } else {
                    0x0000
                };

                // 新目标沿：bit4=新目标，bit5=立即切换，bit6=0 绝对定位。
                if ax.enabled && ax.arm_cycles > 0 {
                    cw |= 0x0030;
                    ax.arm_cycles -= 1;
                }

                unsafe {
                    put_u16(p, 0, cw);
                    *p.add(2) = OP_MODE_PP;
                    put_i32(p, 3, ax.target_raw);
                    put_u16(p, 7, 0); // touch_probe
                    put_i32(p, 9, 0); // 0x60FE:01
                }
            }

            // 2) 一次 PDO 收发。
            let wkc = unsafe { ea_ecat_pdo_cycle() };
            out.wkc = if wkc < 0 { 0 } else { wkc as u32 };

            // 3) 读 TxPDO（收发之后才有本周期数据）。
            for i in 0..AXES {
                let p = self.slaves[i].inputs;
                let ax = &mut self.axes[i];
                let (err, sw, raw, vel) = unsafe {
                    (get_u16(p, 0), get_u16(p, 2), get_i32(p, 5), get_i32(p, 9))
                };
                ax.last_raw = raw;
                out.error[i] = err;
                out.statusword[i] = sw;
                out.position[i] = raw.wrapping_sub(ax.home_offset);
                out.velocity[i] = vel;
                // CiA402：0x27 = Operation enabled
                out.enabled[i] = ax.enabled && (sw & 0x0027) == 0x0027;
                out.reached[i] = ax.enabled && (sw & 0x0400) != 0;
            }

            out
        }
    }

    unsafe fn put_u16(p: *mut u8, off: usize, v: u16) {
        std::ptr::copy_nonoverlapping(v.to_le_bytes().as_ptr(), p.add(off), 2);
    }

    unsafe fn put_i32(p: *mut u8, off: usize, v: i32) {
        std::ptr::copy_nonoverlapping(v.to_le_bytes().as_ptr(), p.add(off), 4);
    }

    unsafe fn get_u16(p: *const u8, off: usize) -> u16 {
        let mut b = [0u8; 2];
        std::ptr::copy_nonoverlapping(p.add(off), b.as_mut_ptr(), 2);
        u16::from_le_bytes(b)
    }

    unsafe fn get_i32(p: *const u8, off: usize) -> i32 {
        let mut b = [0u8; 4];
        std::ptr::copy_nonoverlapping(p.add(off), b.as_mut_ptr(), 4);
        i32::from_le_bytes(b)
    }
}

// —— 实时循环 ——

fn rt_loop(shared: Arc<Shared>, mut backend: Box<dyn Backend>) {
    rt_setup(&shared);
    info!("[rt] 4 ms PDO 循环启动");

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
            libc::CPU_SET(RT_CPU, &mut set);
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

// —— 命令行 ——

fn has_flag(name: &str) -> bool {
    std::env::args().skip(1).any(|a| a == name)
}

fn flag_value(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == name {
            return args.next();
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn build_backend(
    ifname: Option<&str>,
    force_mock: bool,
) -> Result<Box<dyn Backend>, Box<dyn std::error::Error>> {
    match ifname {
        Some(name) if !force_mock => {
            info!("后端 = SOEM EtherCAT，接口 {name}");
            Ok(Box::new(soem::SoemBackend::new(name)?))
        }
        _ => {
            warn!("后端 = mock（软件仿真，**不驱动真实电机**）；真实硬件请加 --ifname <网口>");
            Ok(Box::new(MockBackend::new()))
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn build_backend(
    ifname: Option<&str>,
    _force_mock: bool,
) -> Result<Box<dyn Backend>, Box<dyn std::error::Error>> {
    if ifname.is_some() {
        return Err("SOEM 后端仅支持 Linux 目标；本机是开发机，请用 --mock".into());
    }
    warn!("后端 = mock（软件仿真，**不驱动真实电机**）");
    Ok(Box::new(MockBackend::new()))
}

/// `--probe`：只做扫描与信息打印，**不进 OP、不使能伺服**（首次接硬件的安全入口）。
#[cfg(target_os = "linux")]
fn probe(ifname: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let Some(name) = ifname else {
        return Err("--probe 需要 --ifname <网口>（如 end1）".into());
    };
    soem::probe(name)
}

#[cfg(not(target_os = "linux"))]
fn probe(_ifname: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    Err("--probe 仅支持 Linux 目标（本机是开发机）".into())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let bus_addr = ea_sysapi::parse_bus_arg();
    let ifname = flag_value("--ifname");

    if has_flag("--probe") {
        return probe(ifname.as_deref());
    }

    let backend = build_backend(ifname.as_deref(), has_flag("--mock"))?;

    let shared = Arc::new(Shared::new());

    // 先起 RT 线程，再做任何可能阻塞的事情。
    let rt_shared = shared.clone();
    std::thread::Builder::new()
        .name("pdo-rt".to_string())
        .spawn(move || rt_loop(rt_shared, backend))?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(rpc_loop(shared.clone(), bus_addr))?;

    shared.shutdown.store(true, Ordering::Release);
    Ok(())
}
