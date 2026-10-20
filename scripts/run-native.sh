#!/usr/bin/env bash
# 本机/开发机直接启动全部进程（M1，不用 Docker）。
# 用法: ./scripts/run-native.sh   （Ctrl-C 停止全部）
set -euo pipefail
cd "$(dirname "$0")/.."

BUS="${EA_BUS:-127.0.0.1:7700}"
WEB="${EA_WEB:-0.0.0.0:8080}"

echo "==> 构建 workspace"
cargo build --workspace

PIDS=()
cleanup() {
  echo "==> 停止全部进程"
  for p in "${PIDS[@]:-}"; do
    kill "$p" 2>/dev/null || true
  done
}
trap cleanup EXIT INT TERM

echo "==> 启动 bus / hw-temp / heartbeat / web"
./target/debug/bus --listen "$BUS" --db /tmp/ea_env.db &
PIDS+=($!)
sleep 0.2
./target/debug/hw-temp --bus "$BUS" &
PIDS+=($!)
./target/debug/heartbeat --bus "$BUS" &
PIDS+=($!)
./target/debug/web --bus "$BUS" --listen "$WEB" &
PIDS+=($!)

echo
echo "  bus       : $BUS"
echo "  web 前端  : http://localhost:8080"
echo "  按 Ctrl-C 停止全部进程"
echo
wait
