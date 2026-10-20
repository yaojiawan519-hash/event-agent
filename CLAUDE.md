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

## 设备事实（2026-10-20 实测）

- NanoPi R76S，8 核 aarch64，3.8 GiB RAM，eMMC 119 G（另有 58 G SD）。
- 访问：`ssh yao@192.168.4.90`（公钥，**不是** pi/pi）；接口 `end0`(192.168.4.90) / `end1` / `wlan0`(192.168.3.70)。
- 已装 Docker 29.1.3 + Compose 2.40.3 + `/usr/local/bin/containerd-shim-wasmedge-v1`；设备无 rustc、无外网、无 `wasmedge` CLI。
- 交叉编译从 macOS(arm64) 进行；M0–M3 在本机原生构建验证，M4 再交叉到 `aarch64-unknown-linux-gnu`。
- **开发机（macOS arm64）wasmedge**：`~/.wasmedge/bin/wasmedge`（0.14.1，GitHub 官方预编译包解压而来）。⚠️ 别用 `brew install wasmedge`——其 bottle 要拉 1.5 GB 的 LLVM 23（ghcr 被限速 ~13 KB/s）；官方 tarball 仅 58 MB。运行时把 `~/.wasmedge/bin` 加进 PATH（smoke.sh 已内置），别直接符号链接该二进制到别处——它靠 `@executable_path/../lib` 找 `libwasmedge`，挪位会断 dylib。

## 里程碑状态

- [x] M0 协议与总线（ea-proto / bus / ea-sysapi）
- [x] M1 最小闭环（hw-temp / web / heartbeat）
- [x] M2 运行时 + 规则（ea-wasmapi / runtime）
- [x] M3 LLM + 任务进程（llm-gw / task-thermostat）
- [ ] M4 Docker + WasmEdge 部署
