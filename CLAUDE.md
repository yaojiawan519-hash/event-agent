# event-agent —— 面向 Claude Code 的工作约定

架构唯一依据：`docs/DESIGN.md`（标注【固定】的不可改；改【提案】须在同一次提交更新文档）。
原始需求：`docs/REQUIREMENTS.md`。

## 关键约束

1. `ea-proto` 是协议唯一真源：方法名用常量（`ea_proto::method::*`、`ea_proto::event::*`、`ea_proto::action::*`），任何进程不得手写方法字符串。
2. 技术栈（Rust + tokio / axum / serde / rusqlite / reqwest + WasmEdge）之外不引入运行时依赖；新增 crate 用 `cargo add` 确认当前版本，不凭记忆写版本号。
3. Wasm 来宾（`runtime` / `task-thermostat`）禁止线程、`std::net`、TLS，只能经 `ea-wasmapi` 访问系统。
4. 每个里程碑一个提交；`scripts/smoke.sh` 随里程碑扩展并在提交前通过。
5. 日志用 `tracing`；不把 `api_key`、对话正文写入日志。
6. 先本机原生 + wasmedge 跑通（M1–M3），最后才做 Docker 集成（M4）。

## 与设计文档的已定差异（提案）

- **集成测试位置**：§10.1 的根 `tests/` 改放 `crates/bus/tests/`（M0 需 `CARGO_BIN_EXE_bus`）；跨进程测试（m1.rs）用 `target/{debug,release}/` 路径定位其它 crate 的二进制，依赖 `cargo build --workspace` 先构建（smoke.sh 已按此顺序）。
- **事件广播不含发送者**：总线广播 `event.*` 时跳过发送者自身，避免 `runtime` 收到自己发的 `event.runtime.decision` 造成自环。
- **hw-temp 模型（加热器唯一热源）**：§8.2 改为「环境只作冷源、温度上升只能靠加热器」——`T += h·P − α·max(T−T_amb,0)`，`α=0.3`、`P=25 ℃/s`（为快速触达 high 告警校准）、环境正弦 -8..20 ℃（原 68 ℃ 会让环境直接加热房间，造成「温度涨但加热器关」）。低温告警靠散热、高温告警靠加热器；M1 验收相应改为「开加热器触发 high」。
- **sys.ping 按目标转发**：`sys.ping {target}` 为空/省略时总线自应答，否则总线转发给该进程；每个进程在事件循环里响应 `sys.ping`（对应 §5.5「每个进程必须实现」）。
- **heartbeat 掉线判定**：§8.4 除 ping 软挂检测外，另按注册表差分（进程从注册表消失即发 down），确保「杀进程 15 s 内 down」。
- **ea-sysapi `Client::next(&self)`**：为支持多任务共享（web 同时跑总线接收循环与 HTTP handler），`next` 由 `&mut self` 改为 `&self`（内部 `Mutex<Receiver>`）。
- **`serve(handler)` 与自动重连推迟**：§7 的这两项留到 M2/M3；M1 三个进程在各自 `tokio::select!` 事件循环里内联处理入站请求（`client.next()` + `client.reply()`），行为等价。
- **runtime 规则：默认硬编码 + `--rules` 加载**：§8.6 的「按 `rules.json` 顺序匹配」——M2 把默认规则（low-temp-heat / high-temp-cool / cleared + ignore 列表）以常量数组硬编码在 `crates/runtime/src/main.rs`；M3 增加 `--rules <path>` 从文件加载（WASI fs 读，`wasmedge --dir <parent>` 预打开目录），默认规则作兜底。`when.data.level` 兼容字面点号键与嵌套 `{data:{level}}` 两种写法。
- **`event.runtime.decision` 额外带 `rule` 字段**：§8.6 只定义 `{event_method, mode, actions}`，实现额外写 `rule`（命中的规则 id）便于观测，属增量字段。
- **M3 LLM 测试用 mock 端点**：`llm-gw` 走真实 OpenAI 兼容 HTTP（reqwest，`--config` 指向端点）；`crates/bus/tests/m3.rs` 内置进程内 mock（`/chat/completions`）替代真实 LLM，避免测试依赖外部 key/网络。config 模板 `config/config.json.example`，真实 `config/config.json` 已 gitignore。
- **LLM 工具 = 已登记 `action.*` + 虚拟 `task.start`**：§8.6 的 `task.start {task,params}` 映射到 `action.task.start`；仅执行 `sys.registry.actions` 里登记过的方法（`task.start` 虚拟例外），未登记的一律拒绝。多轮上限 5，工具结果以 `tool` 角色回填。
- **规则 `explain:true` 触发 LLM 一句中文说明**：命中规则执行 `then` 后额外经 `sys.llm.chat` 生成一句解释、以 `action.chat.reply` 发出；无 llm-gw provider 时静默跳过（M2 仍通过）。
- **M4 原生镜像合并**：§10.4 的五张原生镜像合并为单张 `event-agent/native`（含 bus/hw-temp/web/heartbeat/llm-gw 全部二进制，compose 用 `command:` 选进程），两张 wasm 镜像（`event-agent/runtime`、`event-agent/task-thermostat`）各一张。减少 build/push/save/load 次数与传输体积。
- **`platform: wasip1/wasm`（非 `wasi/wasm`）**：§10.2/§10.4 写的是 `wasi/wasm`，但本机 R76S 上 runwasi wasmedge 实测要求 OCI 平台为 `wasip1/wasm`（wasm-web 已按此部署）。构建必须 `docker buildx build --platform wasip1/wasm`，`docker image inspect` 的 Os/Arch 须为 `wasip1/wasm`。
- **原生进程用 CLI flags 而非 config.json**：§9 的 config.json 含 bus/web/hw_temp/heartbeat/runtime 段，但实现只有 `llm-gw` 读 config.json 的 `llm` 段；其余原生进程用 `--bus`/`--listen`/`--db` CLI flags（compose `command:` 传入）。故 config 模板 `config/config.json.example` 只保留 `llm` 段。
- **runtime 规则在设备上 baked-in**：wasm 镜像把 `config/rules.json` 打进 `/rules.json`（runtime 以 `--rules /rules.json` 读取）；不把 config 目录作为 WASI preopen 挂载进 wasm 容器（runwasi shim 对 bind mount 的 preopen 未验证）。读不到时回退硬编码默认（内容一致，行为不变）。
- **wasm 容器不设 `restart`**：shim 停止路径缺陷（`docker stop`/`restart`/`compose restart` 会挂死 docker.service），故 runtime/task-thermostat 不设 restart 策略，崩溃后按全局 CLAUDE.md「wasm 容器硬规则」的安全流程重启。原生进程照常 `restart: unless-stopped`。
- **设备无外网，LLM 验收策略**：R76S 无外网，`llm-gw` 无法访问云端 DeepSeek；M3 的 LLM 推理链路已在本机用 mock 端点（m3.rs）验证。设备上验收 M1/M2 及 M3 的规则/动作/任务本地路径；如需设备上全链路 LLM，可在 Mac 起 mock 端点并让设备 `config.json` 的 `llm.api_url` 指向它。
- **只有聊天事件进 LLM（2026-10-22 收紧）**：`dispatch` 只对 `event.chat.message` 调推理，其余事件仅进 history 作上下文；`rules.json` 的 `explain` 保留解析但不再生效。原因：单线程 runtime 被高频事件牵进 10~30 s 的推理会就地堵死（真机实测聊天要等 2 分钟）。`crates/bus/tests/m3.rs::m3_empty_rules_llm_fallback` 已按新契约改写（先断言事件路径**不**调 LLM，再发聊天触发）。
- **周期快照按 `EA_STATUS_REPORT_MS` 节流（默认 180000 = 3 分钟）**：`event.motor.status`（motor-ecat）与 `event.temperature.updated`（hw-temp）都读这个环境变量；compose 里 `hw-temp` 用 `environment:` 传，`motor-ecat.service` 用 `Environment=` 传。**告警不走这个周期**——它们由 250 ms 的边沿检测抓，且同一 key（`servo:<轴>:<码>` / `wkc:<实得>:<期望>`）只发一次，恢复时发一次 `event.motor.alarm_cleared`。`motor-ecat` 刚启动时 `expect_wkc` 还是 0（未标定），此时**不发 wkc 告警**（否则每次重启都白烧一次推理），宽限 30 s。
- **测试对话链路必须发「通知」而不是「请求」**：`event.chat.message` 走 `client.emit(...)`（无 id 的 notification）；写成 `call(...)` 会因总线上没有该方法的 provider 被拒，表现成「发了消息电机毫无反应」，很容易误判成 LLM 或电机的问题（真机踩过）。`crates/bus/tests/m3.rs` 用 `emit`，`target/ea_chat_probe.py` 已按此修正。
- **网页实时性靠 web 自己轮询**：`web` 每 1 s 调 `action.motor.read` / `action.temperature.read` 推给浏览器（`{"type":"motor"|"temperature_read"}`），所以仪表盘是秒级刷新，与上面的事件节流无关；`motor-ecat` 掉线 5 s 后页面标「无响应」。
- **`event.motor.status` / `event.motor.reached` 必须进 `ignore`**：10 Hz 事件（现已节流）若走 LLM 推理会打爆上游；只有 `event.motor.alarm` 进决策。`rules.json` 与 `runtime` 的 `DEFAULT_IGNORE` 都要列，且 `load_rules` 会把两者**取并集**（rules.json 打包进镜像，容易过期）。
- **web 端口 8090（非 8080）**：§9 写 web 监听 8080，但 R76S 的 8080/8081 已被 wasm-web 演示服务占用（host 网络）；event-agent 的 `web` 改用 `0.0.0.0:8090`，前端访问 `http://192.168.4.90:8090`。
- **`motor-ecat` 是原生实时进程（非容器）**：§4.3 一般约定「所有进程容器化」，但硬实时需要 `SCHED_FIFO` + 绑核 + raw socket + **定向** `mlock`（严禁 `mlockall`，会破坏 AF_PACKET raw socket）。故 `motor-ecat` 以原生进程运行，不进 compose，见 DESIGN §8.8。
- **`motor-ecat` 的 RT/RPC 线程分离**：RT 线程 4 ms PDO 循环只碰 IOmap（不分配/不加锁/不写日志/不做总线 I/O）；RPC 线程跑 tokio，收 `action.motor.*` 后经**无锁 SPSC 环**投递即返回，10 Hz 读快照发 `event.motor.status`。RT 线程永不等待 RPC。
- **`client.connect` 的路由登记是第 4 个参数 `provide`（不是 `actions`）**：`actions` 只是 `sys.registry.actions` 的元数据；漏传 `provide` 时请求报 `-32601 no provider`。
- **M5 双后端**：`MockBackend`（全平台，默认，m5.rs 用它）与 `SoemBackend`（仅 Linux，`--ifname` 启用）。SOEM 2.0.0 已 vendor 进 `crates/motor-ecat/vendor/soem/`，由 `build.rs` + `cc`（build-dependency）编译，非运行时依赖；⚠️ SOEM 是 GPLv3/商业双许可，按 GPLv3 用。接硬件先跑 `--probe --ifname end1`（只读探测，不使能伺服）。
- **M5 真机实测的三个坑（2026-10-22，R76S end1 + 1 台 MOONS）**：
  - **wkc ≠ 从站数+1**：1 从站实测 `wkc=3`。改为 RT 首个健康周期**自标定** `expect_wkc`，未标定前只把 `wkc==0` 当异常。
  - **`CONFIG_RT_GROUP_SCHED=y` 时 root 也拿不到 SCHED_FIFO**：systemd 给 `user.slice` 的 `cpu.rt_runtime_us` 是 0，`chrt -f 80 true` 都失败。RT 线程先把 PID 写 `/sys/fs/cgroup/cgroup.procs` 迁到根 cgroup 再 `sched_setscheduler`。
  - **CiA402 使能必须 `0x06→0x07→0x0F` 逐档推进**（每档 40 ms）；直接发 `0x07` 无效，轴卡在 `0x0250`。主站被 `kill -9` 后 MOONS 锁存 `0x7500` 通讯故障，`motor.enable` 检测到 error≠0 时先发 `0x0080` 复位 32 ms。
  - **轴数运行时由从站数决定**（`MAX_AXES` 只是数组上界）：1 台驱动器按 2 轴算期望 wkc 会每秒刷假 alarm（进 LLM 决策）。
  - 开环步进无编码器：`position` 涨**不代表**轴真的转了，需目视/听声确认。
- **「电机没转」先怀疑速度而不是参数（2026-10-22 第二轮实测）**：驱动器出厂 `0x6081 = 200000 counts/s`（10 圈/s），1000 µstep 只要 20 ms——遥测看是「pos 瞬间跳到目标、vel 恒 0」，肉眼看是「没动」或「动了一下」。用 `--sdo --ifname end1`（PRE_OP，不使能）打印对象字典即可一次看清：`0x6081/83/84` 的实际值、`0x1C12/0x1C13` + `0x1600/0x1A00` 的映射明细（实测与文档逐项吻合：Rx 104 bit / Tx 280 bit）、`0x603F` 锁了什么故障。
- **速度必须真下发（SDO）**：MOONS 默认 RxPDO 没有 0x6081/0x60FF，PP 轮廓速度只能走 CoE。启动时在 **PRE_OP** 读 0x6081/83/84 并**只补零值**；运行期 `action.motor.set_velocity` 由 `Backend::service()` 在命令到达那一拍发一次 SDO（1–10 ms，会让该周期超时一次，可接受）。不写这一步的话，`task-motor` 的 `speed` 参数就是摆设。
- **必须优雅停机**：`SIGINT`/`SIGTERM` → `shared.shutdown` → `join` RT 线程 → `SoemBackend::drop` → `ea_ecat_shutdown()` 把从站切回 INIT。**直接 `kill -9` 会让驱动器锁存 `0x7500` 并闪红灯**（只能断电或下次 enable 时发 `0x0080` 复位）。实测 SIGTERM 后 `0x603F=0`、`0x6041=0x0650`。
- **演示任务用 `action.task.motor_demo`（不是 `task.start`）**：总线前缀路由**单属主**（同前缀第二次注册被拒），`action.task.start` 已归 `task-thermostat`。`task-motor` 是 Wasm 访客，只经 RPC 调 `action.motor.*` 编排「顺 n 圈 → 逆 m 圈 → 停机」，先应答 `{"started":true}`、完成发 `event.task.stopped`，等待期间照常应答 `sys.ping`（否则心跳判软挂），任何分支都以 `motor.stop` 收尾。
- **交叉编译必须隔离 `CARGO_TARGET_DIR`**：`r2s-build` 容器与 Mac 共用 `./target` 会把 Mac 的二进制覆盖成 aarch64 ELF，之后本机 `cargo test` 全部错乱（曾踩）。用 `-e CARGO_TARGET_DIR=/tmp/arm-target`。

## 设备事实（2026-10-20 实测）

- NanoPi R76S，8 核 aarch64，3.8 GiB RAM，eMMC 119 G（另有 58 G SD）。
- 访问：`ssh yao@192.168.4.90`（公钥，**不是** pi/pi）；接口 `end0`(192.168.4.90) / `end1` / `wlan0`(192.168.3.70)。
- 已装 Docker 29.1.3 + Compose 2.40.3 + `/usr/local/bin/containerd-shim-wasmedge-v1`；设备无 rustc、无外网、无 `wasmedge` CLI。
- 交叉编译从 macOS(arm64) 进行；M0–M3 在本机原生构建验证。M4 原生二进制用 `docker buildx` 在 linux/arm64 构建容器内编译（macOS 为 arm64，Docker Desktop 的 Linux VM 同架构，原生构建），wasm 模块仍 `rustup run beta cargo build --target wasm32-wasip1`。
- **开发机（macOS arm64）wasmedge**：`~/.wasmedge/bin/wasmedge`（0.14.1，GitHub 官方预编译包解压而来）。⚠️ 别用 `brew install wasmedge`——其 bottle 要拉 1.5 GB 的 LLVM 23（ghcr 被限速 ~13 KB/s）；官方 tarball 仅 58 MB。运行时把 `~/.wasmedge/bin` 加进 PATH（smoke.sh 已内置），别直接符号链接该二进制到别处——它靠 `@executable_path/../lib` 找 `libwasmedge`，挪位会断 dylib。

## 里程碑状态

- [x] M0 协议与总线（ea-proto / bus / ea-sysapi）
- [x] M1 最小闭环（hw-temp / web / heartbeat）
- [x] M2 运行时 + 规则（ea-wasmapi / runtime）
- [x] M3 LLM + 任务进程（llm-gw / task-thermostat）
- [x] M4 Docker + WasmEdge 部署
- [x] M5 实时运动控制（motor-ecat，RT/RPC 分离 + mock/SOEM 双后端）
