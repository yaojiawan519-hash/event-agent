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
- **web 端口 8090（非 8080）**：§9 写 web 监听 8080，但 R76S 的 8080/8081 已被 wasm-web 演示服务占用（host 网络）；event-agent 的 `web` 改用 `0.0.0.0:8090`，前端访问 `http://192.168.4.90:8090`。
- **`motor-ecat` 是原生实时进程（非容器）**：§4.3 一般约定「所有进程容器化」，但硬实时需要 `SCHED_FIFO` + 绑核 + raw socket + **定向** `mlock`（严禁 `mlockall`，会破坏 AF_PACKET raw socket）。故 `motor-ecat` 以原生进程运行，不进 compose，见 DESIGN §8.8。
- **`motor-ecat` 的 RT/RPC 线程分离**：RT 线程 4 ms PDO 循环只碰 IOmap（不分配/不加锁/不写日志/不做总线 I/O）；RPC 线程跑 tokio，收 `action.motor.*` 后经**无锁 SPSC 环**投递即返回，10 Hz 读快照发 `event.motor.status`。RT 线程永不等待 RPC。
- **`event.motor.status` / `event.motor.reached` 必须进 `ignore`**：10 Hz 事件若走 LLM 推理会打爆上游；只有 `event.motor.alarm` 进决策。`rules.json` 与 `runtime` 的 `DEFAULT_IGNORE` 都要列。
- **`client.connect` 的路由登记是第 4 个参数 `provide`（不是 `actions`）**：`actions` 只是 `sys.registry.actions` 的元数据；漏传 `provide` 时请求报 `-32601 no provider`。
- **M5 后端当前是 mock**：`motor-ecat` 用 `MockBackend` 软件仿真（不驱动真实硬件），SOEM 后端接入点是 `Backend` trait；接真实 EtherCAT 时复用 LinuxWasm 已验证的 `hal_ecat.c`。

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
- [x] M5 实时运动控制（motor-ecat，RT/RPC 分离 + mock 后端）
