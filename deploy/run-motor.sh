#!/usr/bin/env bash
# 启动 motor-ecat —— 原生实时进程（**不进 compose**，原因见 docs/DESIGN.md §8.8）。
#
# 为什么不用容器：硬实时需要 SCHED_FIFO + 绑核 + raw socket + 定向 mlock，
# 容器化会额外引入 cgroup 调度配额与 CONFIG_RT_GROUP_SCHED 的 EPERM 问题。
#
# 权限：
#   - SCHED_FIFO 需要 root 或 CAP_SYS_NICE；
#   - raw socket 需要 CAP_NET_RAW；
#   - 定向 mlock 需要 CAP_IPC_LOCK 或足够的 RLIMIT_MEMLOCK。
#   推荐给该二进制单独配 sudoers NOPASSWD（只授权这一条命令）。
#
# 用法：
#   ./deploy/run-motor.sh                     # 用默认路径与总线地址
#   EA_BUS=127.0.0.1:7700 ./deploy/run-motor.sh
#   EA_RT=0 ./deploy/run-motor.sh             # 关闭实时调度（排查用）
#
# 交叉编译（aarch64，在 Mac 上用本机已有的 r2s-build 容器）：
#   docker run --rm -v "$(pwd):/workspace" -w /workspace r2s-build:latest \
#     bash -c "cargo build --release -p motor-ecat"
#   scp target/release/motor-ecat yao@192.168.4.90:/home/yao/event-agent/bin/
set -euo pipefail

BIN="${EA_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release/motor-ecat}"
BUS="${EA_BUS:-127.0.0.1:7700}"

if [[ ! -x "$BIN" ]]; then
    echo "找不到可执行文件：$BIN（先 cargo build --release -p motor-ecat，或用 EA_BIN 指定）" >&2
    exit 1
fi

echo "启动 motor-ecat：bus=$BUS bin=$BIN" >&2
echo "⚠️  后端为 mock 时不会驱动真实电机；接入 SOEM 后端后此处应打印 ifname。" >&2

exec "$BIN" --bus "$BUS"
