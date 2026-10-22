# event-agent

运行在 NanoPi R76S（aarch64，Armbian）上的**事件驱动 Agent 演示系统**。

系统里的硬件进程不断产生结构化事件，AI 代理层决定这些事件该怎么处理——
可以是规则，也可以是大模型现场推理——然后把对应的操作派发给真正干活的进程。
架构依据见 [`docs/DESIGN.md`](docs/DESIGN.md)，原始需求见
[`Documents/事件驱动型智能体.md`](Documents/事件驱动型智能体.md)。

一句话概括：**把「事件 → 操作」的状态机交给 LLM，把功能块写成一个个普通进程。**

---

## 三层架构

| 层 | 进程 | 形态 | 面对的限制 |
| --- | --- | --- | --- |
| ① 系统进程层 | `bus` `hw-temp` `web` `heartbeat` `llm-gw` | 原生 Linux 程序，Docker 容器 | 要秩序：谁在线、消息送谁、密钥归谁 |
| ② AI 代理层 | `runtime` `task-thermostat` `task-motor` | Wasm（WasmEdge），由 Docker 管理 | 要安全：代码由 AI 生成，关进沙盒 |
| ③ 实时进程层 | `motor-ecat` | 原生进程，**不进容器** | 要时间：4 ms 一个周期 |

九个进程全部连到 `bus`，任何两个进程都不直接对话。

| 进程 | 职责 | 发什么事件 | 提供什么操作 |
| --- | --- | --- | --- |
| `bus` | 消息总线 + 环境存储（SQLite） | — | `sys.*`、`env.*` |
| `hw-temp` | 硬件进程：温度模拟、加热器 | 温度快照、告警、告警解除 | `action.heater.*`、`action.temperature.*` |
| `web` | 网页后端：HTTP + WebSocket，协议翻译 | 用户对话 | `action.chat.reply` |
| `heartbeat` | 心跳与掉线检测 | `event.heartbeat.tick`、进程上下线 | — |
| `llm-gw` | 大模型网关，唯一持有 api_key | — | `sys.llm.chat` |
| `runtime` | 事件运行时：规则匹配 / LLM 推理 | `event.runtime.decision` | — |
| `task-thermostat` | 恒温任务：循环读温度、开关加热器 | 任务开始/结束 | `action.task.start`、`action.task.stop` |
| `task-motor` | 电机演示任务：顺 n 圈 → 逆 m 圈 → 停机 | 任务开始/结束 | `action.task.motor_demo` |
| `motor-ecat` | 实时电机控制（EtherCAT / SOEM） | 电机状态、到位、告警 | `action.motor.*` |

## 消息模型

所有进程之间只走一条通道：到 `bus` 的 TCP 连接（`127.0.0.1:7700`），
NDJSON 一行一帧，格式是 JSON-RPC 2.0。

| 类型 | JSON-RPC 形式 | 路由 | 含义 |
| --- | --- | --- | --- |
| **事件** | Notification（无 `id`） | 前缀匹配，**广播**给所有订阅者 | 谁爱听谁听 |
| **操作** | Request（有 `id`） | 前缀匹配，投给**唯一**提供者 | 点名，必须有人接 |
| 操作结果 | Response | 按 `id` 原路返回 | — |

方法名统一 `<类别>.<域>.<名称>`，类别只有 `event.` / `action.` / `sys.` / `env.` 四种，
全部定义在 `ea-proto`（协议唯一真源，任何进程不得手写方法名字面量）。

```json
// 事件
{"method":"event.temperature.alarm",
 "params":{"source":"hw-temp","data":{"level":"high","value":61.4,"threshold":60}}}
// 操作
{"id":"runtime-17","method":"action.heater.stop",
 "params":{"source":"runtime","data":{}}}
```

跨边界的连接只有三处：浏览器 ↔ `web` 用 WebSocket（普通 JSON，由 `web` 翻译成内部协议）；
Wasm 进程 ↔ `bus` 只允许那一条 TCP 连接；`llm-gw` ↔ 云端用 HTTPS。

---

## 快速开始

### 依赖

- Rust（stable，需 `wasm32-wasip1` 目标：`rustup target add wasm32-wasip1`）
- WasmEdge CLI 0.14.x：装到 `~/.wasmedge`，`scripts/smoke.sh` 会自动把它加进 `PATH`
- Docker + buildx（只有打镜像/部署到板子时才需要）

### 本机原生跑（推荐用于开发）

```bash
./scripts/run-native.sh
# 打开 http://localhost:8080
```

这个脚本构建 workspace 并拉起 `bus` / `hw-temp` / `heartbeat` / `web`。
Wasm 进程可用 wasmedge 直接加进来：

```bash
rustup run beta cargo build --target wasm32-wasip1 --release --manifest-path crates/runtime/Cargo.toml
wasmedge --dir config:config crates/runtime/target/wasm32-wasip1/release/runtime.wasm \
  --bus 127.0.0.1:7700 --rules config/rules.json
```

### 跑验收

```bash
./scripts/smoke.sh
```

构建全部原生 crate 与三个 wasm 模块，然后跑 `cargo test --workspace`。
`crates/bus/tests/` 下是跨进程集成测试（`m0`–`m5`），验收标准见
[`docs/DESIGN.md` §12](docs/DESIGN.md)。

```bash
# 只跑某一部分
cargo test -p bus --test m5              # 电机 RPC 契约 + 演示任务
cargo test -p bus --test m3              # LLM 推理（内置 mock 端点，不需要 key）
```

---

## 部署到 NanoPi R76S

设备没有外网，镜像必须在 Mac 上构建、`docker save` 传过去再 `docker load`。

```bash
./scripts/build.sh                       # → dist/event-agent.tar.gz（native + 三个 wasm 镜像）

scp dist/event-agent.tar.gz yao@192.168.4.90:/home/yao/
ssh yao@192.168.4.90 "docker load < /home/yao/event-agent.tar.gz"
```

首次在板子上启用需要 `deploy/host-setup.sh`（Docker + WasmEdge + runwasi shim）。

启动全部容器：

```bash
cd /home/yao/event-agent && docker compose up -d
# 前端：http://192.168.4.90:8090
```

`motor-ecat` **不在 compose 里**，由 systemd 单元单独管理：

```bash
sudo systemctl enable --now motor-ecat    # deploy/motor-ecat.service
```

### 几条不能违反的运行规矩

- ⚠️ **绝不 `docker stop` / `restart` / `compose restart` 任何 wasm 容器**
  （shim 停止路径缺陷会挂死 `docker.service`）。安全换版：
  `docker load` 新镜像 → `docker update --restart=no <容器>` →
  `sudo pkill -9 -f "containerd-shim-wasmedg[e]"` → `docker compose up -d`。
- ⚠️ **wasm 容器不设 `restart` 策略**。三要素缺一不可：`platform: wasip1/wasm`、
  `runtime: io.containerd.wasmedge.v1`、`network_mode: host`。
- ⚠️ **停 `motor-ecat` 一律用 `SIGTERM`**，它会优雅停机、把从站切回 INIT。
  `kill -9` 会让驱动器锁存通讯故障并闪红灯。

---

## 目录结构

```
├── crates/
│   ├── ea-proto/         JSON-RPC 消息类型 + 全部方法名常量（无 I/O，双目标可编译）
│   ├── ea-sysapi/        原生客户端（tokio）
│   ├── ea-wasmapi/       Wasm 客户端（同步阻塞，接口形状与原生版一致）
│   ├── bus/              消息总线 + 环境存储        ← 系统进程
│   ├── hw-temp/          温度模拟硬件进程
│   ├── web/              网页后端 + static/index.html（单文件前端）
│   ├── heartbeat/        心跳与掉线检测
│   ├── llm-gw/           大模型网关
│   ├── runtime/          事件运行时（wasm32-wasip1） ← AI 代理层
│   ├── task-thermostat/  恒温任务进程（wasm32-wasip1）
│   ├── task-motor/       电机演示任务（wasm32-wasip1）
│   └── motor-ecat/       实时电机控制（原生；vendor/soem + csrc/ea_ecat.c）
├── config/
│   ├── config.json.example   大模型端点模板（api_key 留空；真实 config.json 已 gitignore）
│   └── rules.json            运行时规则与 ignore 列表
├── deploy/               Dockerfile / compose / 宿主安装脚本 / systemd 单元
├── docs/DESIGN.md        架构唯一依据
├── scripts/              build.sh · run-native.sh · smoke.sh · 电机控制探针
└── dist/                 镜像 tar 产物（gitignored）
```

## 配置

`llm-gw` 读 `config/config.json`（从 `config.json.example` 复制并填入 `api_key`）：

```json
{"llm":{"api_url":"https://api.deepseek.com/v1","api_key":"","model":"deepseek-v4-pro",
        "timeout_s":60,"temperature":0.2,"max_tokens":1024}}
```

其余原生进程用命令行参数（`--bus`、`--listen`、`--db`），由 compose 的 `command:` 传入。

| 环境变量 | 默认 | 作用 |
| --- | --- | --- |
| `EA_STATUS_REPORT_MS` | `180000` | 周期状态快照的上报间隔（`event.motor.status`、`event.temperature.updated`）。告警不受此限 |
| `EA_RT` | `1` | `motor-ecat` 设为 `0` 关闭实时调度（排查抖动用） |
| `EA_MOCK` | `0` | `motor-ecat` 设为 `1` 走软件后端，不驱动真实电机 |

## 调试工具

```bash
# 只读探测 EtherCAT 从站，不使能伺服
./deploy/run-motor.sh --probe --ifname end1
# 打印对象字典（PRE_OP，不使能）
./deploy/run-motor.sh --sdo --ifname end1

# 用 WebSocket 控制电机（只用 Python 标准库，任何 agent 都能照抄）
python3 scripts/ws_motor_probe.py --host 192.168.4.90 --wait 4 cmd motor.move_rel --delta -60000

# 在设备本机经总线 JSON-RPC 控制电机
python3 scripts/bus_motor_probe.py -20000
```

网页控制面：连 `ws://<设备>:8090/ws`，发
`{"type":"control","cmd":"motor.move_rel","delta":-60000}` 即可（20000 微步 = 一圈），
回执是 `control_result`，实时状态是每秒一次的 `{"type":"motor"}`。
**该端口没有鉴权**——局域网实验可以，对外开放前请先加 token。

## 里程碑

| | 内容 | 状态 |
| --- | --- | --- |
| M0 | 协议与总线（`ea-proto` / `bus` / `ea-sysapi`） | ✅ |
| M1 | 最小闭环（`hw-temp` / `web` / `heartbeat`） | ✅ |
| M2 | 运行时 + 规则（`ea-wasmapi` / `runtime`） | ✅ |
| M3 | LLM + 任务进程（`llm-gw` / `task-thermostat`） | ✅ |
| M4 | Docker + WasmEdge 部署 | ✅ |
| M5 | 实时运动控制（`motor-ecat`，RT/RPC 分离 + mock/SOEM 双后端） | ✅ |

## 文档

- [`docs/DESIGN.md`](docs/DESIGN.md) —— 架构唯一依据（【固定】不可改，【提案】改动须同步更新）
- [`Documents/事件驱动型智能体.md`](Documents/事件驱动型智能体.md) —— 原始需求
- [`Documents/event-agent-架构说明.html`](Documents/event-agent-架构说明.html) —— 面向读者的三层架构与消息流转说明
- [`CLAUDE.md`](CLAUDE.md) —— 开发约定与真机踩坑记录

## 许可

`crates/motor-ecat/vendor/soem/` 是 vendored 的 SOEM 2.0.0，
采用 GPLv3 / 商业双许可。本项目按 **GPLv3** 使用；分发含该目录的产物即受 GPLv3 约束。
