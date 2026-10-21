#!/usr/bin/env bash
# M4 构建：编译 wasm 模块 → buildx 打包原生/ wasm 镜像 → 导出 tar 供 scp 传输。
# 产物：dist/event-agent.tar.gz（含 event-agent/native + runtime + task-thermostat + task-motor 四个镜像）。
# 部署：
#   scp dist/event-agent.tar.gz yao@192.168.4.90:/home/yao/
#   ssh yao@192.168.4.90 "docker load < /home/yao/event-agent.tar.gz"
set -euo pipefail
cd "$(dirname "$0")/.."

export PATH="$HOME/.wasmedge/bin:$PATH"

echo "== 1/4 编译 wasm 模块（wasm32-wasip1）=="
rustup run beta cargo build --target wasm32-wasip1 --release \
  --manifest-path crates/runtime/Cargo.toml
rustup run beta cargo build --target wasm32-wasip1 --release \
  --manifest-path crates/task-thermostat/Cargo.toml
rustup run beta cargo build --target wasm32-wasip1 --release \
  --manifest-path crates/task-motor/Cargo.toml

echo "== 2/4 构建原生镜像（linux/arm64）=="
docker buildx build --platform linux/arm64 --provenance=false --load \
  -f deploy/Dockerfile.native -t event-agent/native:latest .

echo "== 3/4 构建 wasm 镜像（wasip1/wasm）=="
docker buildx build --platform wasip1/wasm --provenance=false --load \
  --build-arg MODULE=crates/runtime/target/wasm32-wasip1/release/runtime.wasm \
  -f deploy/Dockerfile.wasm -t event-agent/runtime:latest .
docker buildx build --platform wasip1/wasm --provenance=false --load \
  --build-arg MODULE=crates/task-thermostat/target/wasm32-wasip1/release/task-thermostat.wasm \
  -f deploy/Dockerfile.wasm -t event-agent/task-thermostat:latest .
docker buildx build --platform wasip1/wasm --provenance=false --load \
  --build-arg MODULE=crates/task-motor/target/wasm32-wasip1/release/task-motor.wasm \
  -f deploy/Dockerfile.wasm -t event-agent/task-motor:latest .

echo "== 4/4 导出镜像 tar =="
mkdir -p dist
docker save event-agent/native:latest event-agent/runtime:latest \
  event-agent/task-thermostat:latest event-agent/task-motor:latest \
  | gzip > dist/event-agent.tar.gz

echo
echo "完成：dist/event-agent.tar.gz（$(du -h dist/event-agent.tar.gz | cut -f1)）"
echo "传输：scp dist/event-agent.tar.gz yao@192.168.4.90:/home/yao/"
echo "加载：ssh yao@192.168.4.90 \"docker load < /home/yao/event-agent.tar.gz\""
