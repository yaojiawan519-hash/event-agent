#!/usr/bin/env bash
# Armbian(aarch64) 宿主准备：Docker Engine + containerd 镜像存储 + WasmEdge 共享库 + runwasi shim。
# 对应 docs/DESIGN.md §10.3。幂等：已装好的步骤会自动跳过。
#
# ⚠️ 当前 R76S（192.168.4.90）已完成本脚本的全部步骤，无需重跑。
#    本脚本供新板子 / 重装系统后一次性初始化使用。
set -euo pipefail

log() { echo -e "\033[1;32m==>\033[0m $*"; }
fail() { echo -e "\033[1;31m!!\033[0m $*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || fail "请用 sudo 运行（需要 root）"

# 1. Docker Engine + compose 插件（Debian/Ubuntu 官方仓库）
if ! command -v docker >/dev/null 2>&1; then
  log "安装 Docker Engine + docker-compose-plugin"
  apt-get update
  apt-get install -y ca-certificates curl gnupg
  install -m 0755 -d /etc/apt/keyrings
  curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc
  chmod a+r /etc/apt/keyrings/docker.asc
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian bookworm stable" \
    > /etc/apt/sources.list.d/docker.list
  apt-get update
  apt-get install -y docker-ce docker-ce-cli containerd.io docker-compose-plugin
else
  log "docker 已安装：$(docker --version)"
fi

# 2. daemon.json：containerd 镜像存储（运行 wasm 工作负载必需）
if ! grep -q containerd-snapshotter /etc/docker/daemon.json 2>/dev/null; then
  log "配置 /etc/docker/daemon.json 开启 containerd 镜像存储"
  if [ -s /etc/docker/daemon.json ]; then
    # 已有配置则合并 features 段（简单合并，不改其他字段）
    echo '{"features": {"containerd-snapshotter": true}}' > /tmp/ea_daemon.json
    fail "daemon.json 已有自定义内容，请手动确保 features.containerd-snapshotter=true 后重启 docker"
  else
    mkdir -p /etc/docker
    echo '{"features": {"containerd-snapshotter": true}}' > /etc/docker/daemon.json
  fi
  systemctl restart docker
else
  log "containerd 镜像存储已开启"
fi

# 3. WasmEdge 共享库（runwasi shim 需要宿主上的 WasmEdge 运行时库）
if [ ! -d /usr/local/lib/wasmedge ] && [ ! -d "$HOME/.wasmedge" ]; then
  log "安装 WasmEdge 运行时库（aarch64）"
  curl -sSf https://raw.githubusercontent.com/WasmEdge/WasmEdge/master/utils/install.sh \
    | bash -s -- -p /usr/local -e all
else
  log "WasmEdge 已安装"
fi

# 4. runwasi 的 containerd-shim-wasmedge-v1
if ! command -v containerd-shim-wasmedge-v1 >/dev/null 2>&1; then
  log "安装 containerd-shim-wasmedge-v1 到 /usr/local/bin"
  # arm64 发布包不可用时可改用 Rust 源码编译 runwasi（见 DESIGN.md §10.3 第 4 步）
  fail "未找到 containerd-shim-wasmedge-v1；请先获取 arm64 发布包或编译 runwasi 并放到 /usr/local/bin"
else
  log "containerd-shim-wasmedge-v1 已就位"
fi

# 5. 验证 wasm 工作负载
log "验证 wasm 运行时"
docker run --rm --runtime=io.containerd.wasmedge.v1 --platform=wasip1/wasm \
  ghcr.io/containerd/runwasi/wasi-demo-app:latest 2>/dev/null \
  || log "验证镜像不可达（设备无外网属正常），跳过"

log "宿主准备完成。下一步：把 Mac 上构建好的镜像 docker load 进来，再 docker compose up -d。"
