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

/// 编译期最大轴数（数组上界）。**实际轴数运行时由总线从站数决定**（`Shared::n_axes`）：
/// 写死 2 轴而总线上只有 1 台驱动器时，期望工作计数对不上，会每 100 ms 刷一条假
/// `event.motor.alarm`——而 alarm 是进 LLM 决策的。
const MAX_AXES: usize = 2;
/// PDO 周期（µs）。4 ms 对齐 LinuxWasm 已验证基线。
const CYCLE_US: u64 = 4_000;
/// 状态事件频率：10 Hz（**不是** 250 Hz —— 否则会把总线与 runtime 打爆）。
const STATUS_PERIOD: Duration = Duration::from_millis(100);
/// 命令环容量（定长，无需分配）。
const CMD_RING: usize = 64;
/// 每个周期最多处理多少条命令，防止命令洪泛饿死 PDO 循环。
const CMD_DRAIN_MAX: usize = 16;
/// CiA402 使能序列每档保持的周期数（10 × 4 ms = 40 ms）。
#[cfg(target_os = "linux")]
const ENABLE_DWELL: u8 = 10;
/// mock 后端仿真的工作计数（2 轴固定 3；m5.rs 按此断言）。
const MOCK_WKC: u32 = 3;
/// PP 模式默认轮廓速度：20000 counts/s = 1 圈/s（MOONS 20000 µstep/圈）。
/// 仅当驱动器里的 0x6081 为 0 时才写入（见 soem::ensure_profile）。
const DEFAULT_SPEED: u32 = 20_000;
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
    /// 实际轴数（启动时确定，此后只读）。
    n_axes: usize,
    axes: [AxisStatus; MAX_AXES],
    wkc: AtomicU32,
    /// 期望工作计数：**首个健康周期自标定**（0 = 尚未标定）。
    /// 不写死公式——1 个 MOONS 从站实测 wkc=3（SOEM 的 LRW 计数是
    /// `2*OutputsWKC + InputsWKC`），按「从站数+1」猜会一直误报 alarm。
    expect_wkc: AtomicU32,
    cycles: AtomicU64,
    cmds: CmdRing,
    /// 急停落点：RPC 直接置位，RT 在下一个周期最优先处理（不排队）。
    estop: AtomicBool,
    shutdown: AtomicBool,
}

impl Shared {
    fn new(n_axes: usize) -> Self {
        Shared {
            n_axes,
            axes: std::array::from_fn(|_| AxisStatus::new()),
            wkc: AtomicU32::new(0),
            expect_wkc: AtomicU32::new(0),
            cycles: AtomicU64::new(0),
            cmds: CmdRing::new(),
            estop: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
        }
    }

    fn publish(&self, out: &CycleOut) {
        for a in 0..self.n_axes.min(MAX_AXES) {
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
        let axes: Vec<Value> = (0..self.n_axes.min(MAX_AXES))
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
    position: [i32; MAX_AXES],
    velocity: [i32; MAX_AXES],
    statusword: [u16; MAX_AXES],
    error: [u16; MAX_AXES],
    enabled: [bool; MAX_AXES],
    reached: [bool; MAX_AXES],
    wkc: u32,
}

trait Backend: Send {
    fn apply(&mut self, cmd: Cmd);
    fn cycle(&mut self) -> CycleOut;
    /// 每周期收尾的「低频工作」（如把待写的 SDO 真正发出去）。默认无操作。
    ///
    /// 单独开这个钩子是为了把 SOEM 的 SDO 写入限制在**命令刚下来的那一拍**：
    /// SDO 走邮箱、耗时 1–10 ms，会让该周期超时一次（4 ms → 十几 ms），
    /// 但参数设置本来就极少发生，比在每个周期里轮询代价小得多。
    fn service(&mut self) {}
    /// 本后端实际驱动的轴数（决定期望 wkc、快照与 axis 参数上界）。
    fn n_axes(&self) -> usize;
}

/// 软件仿真后端：无硬件时验证线程模型与协议。
///
/// 语义对齐开环步进：位置是驱动器自生成的微步计数（电机不动也会涨）。
struct MockBackend {
    position: [i64; MAX_AXES],
    target: [i64; MAX_AXES],
    velocity: [i64; MAX_AXES],
    enabled: [bool; MAX_AXES],
}

impl MockBackend {
    fn new() -> Self {
        MockBackend {
            position: [0; MAX_AXES],
            target: [0; MAX_AXES],
            velocity: [20_000; MAX_AXES], // 20000 µstep/s = 1 rev/s（MOONS 微步/圈）
            enabled: [false; MAX_AXES],
        }
    }

    fn each_axis(&mut self, axis: usize, mut f: impl FnMut(&mut Self, usize)) {
        if axis == AXIS_ALL {
            for a in 0..MAX_AXES {
                f(self, a);
            }
        } else if axis < MAX_AXES {
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
            wkc: MOCK_WKC,
            ..Default::default()
        };
        for a in 0..MAX_AXES {
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

    /// mock 始终仿真满轴（测试用它验证 2 轴契约）。
    fn n_axes(&self) -> usize {
        MAX_AXES
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
        fn ea_ecat_sdo_read(slave: i32, index: u16, sub: u8, buf: *mut u8, size: *mut i32) -> i32;
        fn ea_ecat_sdo_write(slave: i32, index: u16, sub: u8, buf: *const u8, size: i32) -> i32;
    }

    /// 读一个 SDO 对象，按 size 解成 u32（兼容 1/2/4 字节的对象）。
    fn sdo_read(slave: i32, index: u16, sub: u8, size: i32) -> Option<u32> {
        let mut buf = [0u8; 4];
        let mut sz = size;
        let rc = unsafe { ea_ecat_sdo_read(slave, index, sub, buf.as_mut_ptr(), &mut sz) };
        if rc != 0 || sz <= 0 || sz > 4 {
            return None;
        }
        let mut v: u32 = 0;
        for (i, b) in buf[..sz as usize].iter().enumerate() {
            v |= (*b as u32) << (8 * i);
        }
        Some(v)
    }

    /// 写一个 32 位 SDO 对象。返回是否成功。
    fn sdo_write_u32(slave: i32, index: u16, sub: u8, value: u32) -> bool {
        let buf = value.to_le_bytes();
        unsafe { ea_ecat_sdo_write(slave, index, sub, buf.as_ptr(), 4) == 0 }
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
        /// TxPDO 读回的 0x603F 错误码（0x7500 = 通讯故障，主站消失时锁存）。
        last_error: u16,
        /// 故障复位脉冲剩余周期：>0 时 controlword 发 0x0080（bit7 上升沿）。
        reset_cycles: u8,
        /// 当前使能档已保持的周期数（见 ENABLE_DWELL）。
        step_cycles: u8,
        /// 待写入 0x6081（轮廓速度）的值：RxPDO 里没有这个对象，只能走 SDO。
        pending_velocity: Option<i32>,
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
                last_error: 0,
                reset_cycles: 0,
                step_cycles: 0,
                pending_velocity: None,
            }
        }
    }

    pub struct SoemBackend {
        slaves: Vec<Slave>,
        axes: [Axis; MAX_AXES],
        n: usize,
    }

    // SAFETY: 后端只在单个 RT 线程内使用；裸指针来自 SOEM 的 IOmap，
    // 生命周期由 ea_ecat_shutdown()（Drop）保证。
    unsafe impl Send for SoemBackend {}

    /// 进 OP 前检查 PP 轮廓参数：0x6081 为 0 时驱动器「瞬间到点」，
    /// 表现为 pos 直接跳到目标、vel 恒为 0（真机踩过）。这里只补零值，
    /// 不覆盖用户在驱动器里调好的参数。
    fn ensure_profile(idx: usize, slave: i32, default_speed: u32) {
        for (index, name, fallback) in [
            (0x6081u16, "0x6081 profile_velocity", default_speed),
            (0x6083u16, "0x6083 profile_acceleration", default_speed * 20),
            (0x6084u16, "0x6084 profile_deceleration", default_speed * 20),
        ] {
            match sdo_read(slave, index, 0, 4) {
                None => warn!("[soem] slave {idx} {name} 读失败（对象可能不存在）"),
                Some(0) => {
                    if sdo_write_u32(slave, index, 0, fallback) {
                        info!("[soem] slave {idx} {name} 原为 0，写默认值 {fallback}");
                    } else {
                        warn!("[soem] slave {idx} {name} 为 0 且写入失败——运动可能瞬间到点");
                    }
                }
                Some(v) => info!("[soem] slave {idx} {name} = {v}（保留驱动器设定）"),
            }
        }
    }

    impl SoemBackend {
        pub fn new(ifname: &str, default_speed: u32) -> Result<Self, String> {
            let c = CString::new(ifname).map_err(|e| format!("接口名含 NUL：{e}"))?;
            let n = unsafe { ea_ecat_init(c.as_ptr()) };
            if n <= 0 {
                return Err(format!("ea_ecat_init({ifname}) 失败，错误码 {n}"));
            }
            let n = n as usize;
            if n == 0 {
                unsafe { ea_ecat_shutdown() };
                return Err("总线上没有从站".to_string());
            }
            if n > MAX_AXES {
                unsafe { ea_ecat_shutdown() };
                return Err(format!(
                    "总线上有 {n} 个从站，超过编译期上界 MAX_AXES={MAX_AXES}"
                ));
            }
            if n < MAX_AXES {
                warn!("[soem] 总线上只有 {n} 个从站（MAX_AXES={MAX_AXES}），按 {n} 轴运行");
            }

            let mut slaves = Vec::with_capacity(n);
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

            // 进 OP 之前（此时仍在 PRE_OP）通过 SDO 检查/补齐轮廓参数。
            for i in 1..=n as i32 {
                ensure_profile((i - 1) as usize, i, default_speed);
            }

            // ESM：先全部进 OP，再靠 PDO 使能（不能反过来）。
            for i in 1..=n {
                let rc = unsafe { ea_ecat_to_op(i as i32) };
                if rc != 0 {
                    unsafe { ea_ecat_shutdown() };
                    return Err(format!("从站 {i} 无法进入 OP，错误码 {rc}"));
                }
            }

            Ok(SoemBackend { slaves, axes: std::array::from_fn(|_| Axis::new()), n })
        }

        fn each_axis(&mut self, axis: usize, mut f: impl FnMut(&mut Axis)) {
            if axis == AXIS_ALL {
                for a in &mut self.axes {
                    f(a);
                }
            } else if axis < self.n {
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

    /// `--sdo`：在 PRE_OP 下把对象字典的关键项打出来（**不进 OP、不使能**）。
    ///
    /// 用来核对三件事：①实际 PDO 映射是不是文档写的 13 B / 35 B；
    /// ②PP 轮廓参数（0x6081/83/84）是不是 0；③驱动器的模式与故障码。
    pub fn sdo_dump(ifname: &str) -> Result<(), Box<dyn std::error::Error>> {
        let c = CString::new(ifname)?;
        let n = unsafe { ea_ecat_init(c.as_ptr()) };
        if n <= 0 {
            return Err(format!("ea_ecat_init({ifname}) 失败，错误码 {n}").into());
        }

        for i in 1..=n {
            println!("\n== slave {} ==", i - 1);
            for (index, sub, size, name) in [
                (0x603F_u16, 0u8, 2i32, "error_code"),
                (0x6060, 0, 1, "modes_of_operation"),
                (0x6061, 0, 1, "modes_of_operation_display"),
                (0x6040, 0, 2, "controlword"),
                (0x6041, 0, 2, "statusword"),
                (0x607A, 0, 4, "target_position"),
                (0x6064, 0, 4, "actual_position"),
                (0x6081, 0, 4, "profile_velocity"),
                (0x6083, 0, 4, "profile_acceleration"),
                (0x6084, 0, 4, "profile_deceleration"),
                (0x608F, 1, 4, "encoder_resolution"),
                (0x6091, 1, 4, "gear_motor_revolutions"),
                (0x6091, 2, 4, "gear_shaft_revolutions"),
            ] {
                match sdo_read(i, index, sub, size) {
                    Some(v) => println!("  0x{index:04X}:{sub:<2} {name:<30} = {v}"),
                    None => println!("  0x{index:04X}:{sub:<2} {name:<30} —— 读失败/不存在"),
                }
            }

            // PDO 分配：0x1C12 = RxPDO(SM2)，0x1C13 = TxPDO(SM3)
            for (index, label) in [(0x1C12u16, "RxPDO"), (0x1C13u16, "TxPDO")] {
                let count = sdo_read(i, index, 0, 1).unwrap_or(0);
                println!("  0x{index:04X}:00 {label} 分配对象数 = {count}");
                for sub in 1..=count.min(8) {
                    match sdo_read(i, index, sub as u8, 2) {
                        Some(map) => {
                            println!("       -> 0x{map:04X}");
                            let count = sdo_read(i, map as u16, 0, 1).unwrap_or(0);
                            println!("          0x{map:04X}:00 映射条目数 = {count}");
                            for e in 1..=count.min(16) {
                                match sdo_read(i, map as u16, e as u8, 4) {
                                    Some(v) => println!(
                                        "          [{e}] 0x{:04X}:{} {} bit",
                                        v >> 16,
                                        (v >> 8) & 0xFF,
                                        v & 0xFF
                                    ),
                                    None => println!("          [{e}] 读失败"),
                                }
                            }
                        }
                        None => println!("      sub {sub} 读失败"),
                    }
                }
            }
        }
        unsafe { ea_ecat_shutdown() };
        println!("\nSDO 诊断完成：未进 OP、未使能伺服");
        Ok(())
    }

    impl Backend for SoemBackend {
        fn n_axes(&self) -> usize {
            self.n
        }

        fn apply(&mut self, cmd: Cmd) {
            let value = cmd.value;
            match cmd.op {
                OP_ENABLE => self.each_axis(cmd.axis, |a| {
                    if a.last_error != 0 && !a.enabled {
                        // 先清故障（8 周期 × 4 ms = 32 ms 的 0x0080 脉冲），
                        // 否则 0x06/0x07/0x0F 全被忽略，轴永远停在 Fault。
                        warn!("[soem] 轴有锁存故障 0x{:04X}，先发故障复位", a.last_error);
                        a.reset_cycles = 8;
                    }
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
                    // MOONS 默认 RxPDO 没有 0x60FF，PP 模式的轮廓速度只由 0x6081 决定。
                    // 这里只做标记，真正的 SDO 写在 service() 里发（避免阻塞正常周期）。
                    self.each_axis(cmd.axis, |a| {
                        if value > 0 {
                            a.velocity = value;
                            a.pending_velocity = Some(value);
                        }
                    });
                }
                _ => {}
            }
        }

        fn cycle(&mut self) -> CycleOut {
            let mut out = CycleOut { wkc: 0, ..Default::default() };

            // 1) 写 RxPDO（必须先写再发）。
            for i in 0..self.n {
                let p = self.slaves[i].outputs;
                let ax = &mut self.axes[i];

                let mut cw: u16 = if ax.reset_cycles > 0 {
                    // CiA402 故障复位：bit7 上升沿。驱动器锁存故障（如主站消失
                    // 导致的 0x7500 通讯故障）后，不先复位就永远进不了 Operation enabled。
                    ax.reset_cycles -= 1;
                    if ax.reset_cycles == 0 {
                        ax.enable_step = 0;
                        ax.step_cycles = 0;
                        info!("[soem] slave {i} 故障复位脉冲结束");
                    }
                    0x0080
                } else if ax.enabled {
                    // CiA402 使能序列必须 **0x06 → 0x07 → 0x0F 逐级推进**，
                    // 每档保持 ENABLE_DWELL 个周期（驱动器状态机需要时间响应；
                    // 直接从 Switch on disabled 发 0x07 会被忽略，轴永远停在 0x0250）。
                    if ax.step_cycles >= ENABLE_DWELL {
                        ax.step_cycles = 0;
                        ax.enable_step = ax.enable_step.saturating_add(1).min(2);
                    } else {
                        ax.step_cycles += 1;
                    }
                    match ax.enable_step {
                        0 => 0x0006, // Shutdown      → Ready to switch on (0x21)
                        1 => 0x0007, // Switch on     → Switched on        (0x23)
                        _ => 0x000F, // Enable operation → Operation enabled(0x27)
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
            for i in 0..self.n {
                let p = self.slaves[i].inputs;
                let ax = &mut self.axes[i];
                let (err, sw, raw, vel) = unsafe {
                    (get_u16(p, 0), get_u16(p, 2), get_i32(p, 5), get_i32(p, 9))
                };
                ax.last_raw = raw;
                ax.last_error = err;
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

        /// 把待写的轮廓参数经 SDO 发出去（每个轴每拍最多写一个对象）。
        fn service(&mut self) {
            for i in 0..self.n {
                let Some(v) = self.axes[i].pending_velocity.take() else { continue };
                if v <= 0 {
                    continue;
                }
                let ok = sdo_write_u32((i + 1) as i32, 0x6081, 0, v as u32);
                if ok {
                    self.axes[i].velocity = v;
                    info!("[soem] slave {i} 0x6081 profile_velocity = {v} counts/s");
                } else {
                    warn!("[soem] slave {i} 写 0x6081 失败——速度仍为驱动器内的旧值");
                }
            }
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

        // 3') 低频工作：SOEM 后端在这里把待写的 SDO（0x6081 等）真正发出去。
        backend.service();

        // 首个健康周期标定期望工作计数（见 Shared::expect_wkc 的说明）。
        if out.wkc > 0 && shared.expect_wkc.load(Ordering::Relaxed) == 0 {
            shared.expect_wkc.store(out.wkc, Ordering::Relaxed);
            info!("[rt] 期望工作计数标定为 {}", out.wkc);
        }

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
            // CONFIG_RT_GROUP_SCHED=y 时，systemd 给 user.slice 的 cpu.rt_runtime_us
            // 默认是 0——**即使是 root 也会 EPERM**（chrt -f 80 true 都失败）。
            // 先把本进程迁到根 cgroup（其配额 = /proc/sys/kernel/sched_rt_runtime_us），
            // 与 LinuxWasm 运行时的做法一致。
            let pid = std::process::id().to_string();
            match std::fs::write("/sys/fs/cgroup/cgroup.procs", &pid) {
                Ok(()) => info!("[rt] 已迁移到根 cgroup（解 RT_GROUP_SCHED 配额）"),
                Err(e) => warn!("[rt] 迁移根 cgroup 失败（{e}）——SCHED_FIFO 可能被 cgroup 配额拒绝"),
            }

            let mut param: libc::sched_param = std::mem::zeroed();
            param.sched_priority = RT_PRIO;
            if libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) != 0 {
                warn!(
                    "[rt] sched_setscheduler(SCHED_FIFO, {RT_PRIO}) 失败（需 root/CAP_SYS_NICE，\
                     且 cgroup 的 cpu.rt_runtime_us 不能为 0），降级为普通调度"
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

fn parse_axis(data: &Value, n_axes: usize) -> Result<usize, String> {
    match data.get("axis") {
        None | Some(Value::Null) => Ok(AXIS_ALL),
        Some(v) => match v.as_u64() {
            Some(a) if (a as usize) < n_axes => Ok(a as usize),
            _ => Err(format!("axis 必须是 0..{n_axes} 的整数（总线上 {n_axes} 轴）")),
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

    let n_axes = shared.n_axes;
    let mut last_reached = [false; MAX_AXES];
    let mut last_alarm = false;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                // 10 Hz 状态（在 ignore 列表里，不触发推理）
                let _ = client.emit(event::MOTOR_STATUS, shared.snapshot()).await;

                // 到位沿（只在 false→true 时发一次）
                for a in 0..n_axes {
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
                let err_axis = (0..n_axes).find(|&a| shared.axes[a].error.load(Ordering::Relaxed) != 0);
                let expected = shared.expect_wkc.load(Ordering::Relaxed);
                // 未标定（expected==0）时只把 wkc==0 当异常，避免启动瞬间误报。
                let alarm = wkc == 0
                    || (expected != 0 && wkc != expected)
                    || err_axis.is_some();
                if alarm && !last_alarm {
                    let payload = match err_axis {
                        Some(a) => json!({
                            "kind": "servo",
                            "axis": a,
                            "error": shared.axes[a].error.load(Ordering::Relaxed),
                        }),
                        None => json!({"kind": "wkc", "wkc": wkc, "expected": expected}),
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

                                let axis = match parse_axis(data, n_axes) {
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
    default_speed: u32,
) -> Result<Box<dyn Backend>, Box<dyn std::error::Error>> {
    match ifname {
        Some(name) if !force_mock => {
            info!("后端 = SOEM EtherCAT，接口 {name}");
            Ok(Box::new(soem::SoemBackend::new(name, default_speed)?))
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
    _default_speed: u32,
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

/// `--sdo`：对象字典诊断（PRE_OP，不驱动电机）。
#[cfg(target_os = "linux")]
fn sdo_dump(ifname: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let Some(name) = ifname else {
        return Err("--sdo 需要 --ifname <网口>（如 end1）".into());
    };
    soem::sdo_dump(name)
}

#[cfg(not(target_os = "linux"))]
fn sdo_dump(_ifname: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    Err("--sdo 仅支持 Linux 目标（本机是开发机）".into())
}

/// 等 Ctrl-C 或 SIGTERM。**收到信号必须让 RT 线程正常退出**：
/// 否则 SoemBackend 的 Drop 不跑、从站停在 OP，驱动器会锁存 0x7500
/// 通讯故障并闪红灯（真机踩过，只能断电清）。
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                warn!("注册 SIGTERM 处理器失败（{e}）");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("收到 SIGINT"),
        _ = terminate => info!("收到 SIGTERM"),
    }
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
    let default_speed = flag_value("--speed")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(DEFAULT_SPEED);

    if has_flag("--probe") {
        return probe(ifname.as_deref());
    }
    if has_flag("--sdo") {
        return sdo_dump(ifname.as_deref());
    }

    let backend = build_backend(ifname.as_deref(), has_flag("--mock"), default_speed)?;
    let n_axes = backend.n_axes();
    info!("轴数 = {n_axes}");

    let shared = Arc::new(Shared::new(n_axes));

    // 先起 RT 线程，再做任何可能阻塞的事情。
    let rt_shared = shared.clone();
    let rt_thread = std::thread::Builder::new()
        .name("pdo-rt".to_string())
        .spawn(move || rt_loop(rt_shared, backend))?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    let result = rt.block_on(async {
        tokio::select! {
            r = rpc_loop(shared.clone(), bus_addr) => r,
            _ = shutdown_signal() => {
                info!("[main] 准备停机：等 RT 线程退出后从站回 INIT");
                Ok(())
            }
        }
    });

    // 关键顺序：先让 RT 循环退出（它持有 backend，Drop 里 ea_ecat_shutdown 把
    // 从站切回 INIT），再让进程结束。直接 pkill 会跳过这一步，驱动器锁存 0x7500。
    shared.shutdown.store(true, Ordering::Release);
    if rt_thread.join().is_err() {
        warn!("[main] RT 线程异常退出");
    }
    info!("[main] 已停机");

    result?;
    Ok(())
}
