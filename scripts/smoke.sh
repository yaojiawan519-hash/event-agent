#!/usr/bin/env bash
# M0–M2 验收：协议与总线 + 最小闭环（hw-temp / web / heartbeat）+ 运行时规则（runtime.wasm）。
# 验收标准见 docs/DESIGN.md §12。
set -euo pipefail
cd "$(dirname "$0")/.."

# WasmEdge CLI（跑 runtime.wasm）；本机安装于 ~/.wasmedge（见 CLAUDE.md「设备事实」）。
export PATH="$HOME/.wasmedge/bin:$PATH"

echo "== 编译原生 workspace =="
cargo build --workspace

echo "== 编译 wasm 模块（wasm32-wasip1）=="
rustup run beta cargo build --target wasm32-wasip1 --release \
  --manifest-path crates/runtime/Cargo.toml
rustup run beta cargo build --target wasm32-wasip1 --release \
  --manifest-path crates/task-thermostat/Cargo.toml

echo "== 运行全部集成测试 =="
cargo test --workspace -- --nocapture
