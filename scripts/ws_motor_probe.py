#!/usr/bin/env python3
"""最小 WebSocket 客户端（只用 Python 标准库，无第三方依赖）。

用途：证明**任意 agent/脚本**都能通过 `ws://<设备>:8090/ws` 控制电机，
不需要 websocket-client / Node / 浏览器。

协议：
  发送 {"type":"control","cmd":"<命令>", ...参数}
  收到 {"type":"control_result","cmd":..,"ok":true,"result":{..}}  ← 命令回执
       {"type":"motor","state":{..}}                            ← 1 s 一次的实时状态

用法：
  python3 ws_motor_probe.py --host 192.168.3.70 read
  python3 ws_motor_probe.py --host 192.168.3.70 cmd motor.enable
  python3 ws_motor_probe.py --host 192.168.3.70 cmd motor.move_rel --delta -60000
  python3 ws_motor_probe.py --host 192.168.3.70 cmd motor.demo --turns 3
"""

import argparse
import base64
import json
import os
import socket
import struct
import time


def ws_connect(host, port=8090, path="/ws", timeout=15):
    s = socket.create_connection((host, port), timeout=timeout)
    key = base64.b64encode(os.urandom(16)).decode()
    req = (
        f"GET {path} HTTP/1.1\r\n"
        f"Host: {host}:{port}\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        "Sec-WebSocket-Version: 13\r\n\r\n"
    )
    s.sendall(req.encode())
    head = b""
    while b"\r\n\r\n" not in head:
        chunk = s.recv(4096)
        if not chunk:
            raise RuntimeError("握手失败：连接被关闭")
        head += chunk
    if b"101" not in head.split(b"\r\n")[0]:
        raise RuntimeError("握手被拒：" + head.split(b"\r\n")[0].decode())
    return s, head.split(b"\r\n\r\n", 1)[1]


def ws_send(s, text):
    payload = text.encode()
    n = len(payload)
    mask = os.urandom(4)
    if n < 126:
        hdr = struct.pack("!BB", 0x81, 0x80 | n)
    elif n < 65536:
        hdr = struct.pack("!BBH", 0x81, 0x80 | 126, n)
    else:
        hdr = struct.pack("!BBQ", 0x81, 0x80 | 127, n)
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    s.sendall(hdr + mask + masked)


def _read_exact(s, n, buf):
    while len(buf[0]) < n:
        chunk = s.recv(65536)
        if not chunk:
            raise RuntimeError("连接被关闭")
        buf[0] += chunk
    out, buf[0] = buf[0][:n], buf[0][n:]
    return out


def ws_recv(s, buf):
    b0, b1 = _read_exact(s, 2, buf)
    opcode = b0 & 0x0F
    n = b1 & 0x7F
    if n == 126:
        n = struct.unpack("!H", _read_exact(s, 2, buf))[0]
    elif n == 127:
        n = struct.unpack("!Q", _read_exact(s, 8, buf))[0]
    mask = _read_exact(s, 4, buf) if b1 & 0x80 else None
    data = _read_exact(s, n, buf)
    if mask:
        data = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
    if opcode == 0x8:
        raise RuntimeError("服务端关闭连接")
    return data.decode()


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--host", default="192.168.3.70")
    p.add_argument("--port", type=int, default=8090)
    p.add_argument("--wait", type=float, default=4.0, help="命令后收多少秒的状态帧")
    sub = p.add_subparsers(dest="mode", required=True)
    sub.add_parser("read")
    c = sub.add_parser("cmd")
    c.add_argument("name", help="motor.enable | motor.stop | motor.home | "
                               "motor.move_rel | motor.move_abs | motor.set_velocity | motor.demo")
    c.add_argument("--delta", type=int, default=None)
    c.add_argument("--position", type=int, default=None)
    c.add_argument("--velocity", type=int, default=None)
    c.add_argument("--turns", type=float, default=None)
    a = p.parse_args()

    s, leftover = ws_connect(a.host, a.port)
    buf = [leftover]
    msg = {"type": "control", "cmd": "motor.read" if a.mode == "read" else a.name}
    for k in ("delta", "position", "velocity", "turns"):
        v = getattr(a, k, None)
        if v is not None:
            msg[k] = v
    ws_send(s, json.dumps(msg))
    print("→", json.dumps(msg, ensure_ascii=False))

    deadline = time.time() + a.wait
    s.settimeout(1.0)
    last_status = None
    while time.time() < deadline:
        try:
            m = json.loads(ws_recv(s, buf))
        except socket.timeout:
            continue
        if m.get("type") == "control_result":
            print("← control_result:", json.dumps(m, ensure_ascii=False))
        elif m.get("type") == "motor":
            st = m.get("state") or {}
            last_status = st
        elif m.get("type") == "reply":
            print("← reply:", m.get("text"))
    if last_status:
        for ax in last_status.get("axes", []):
            print(f"  轴{ax.get('axis')}: pos={ax.get('position')} "
                  f"({(ax.get('position') or 0)/20000:.2f} 圈) "
                  f"vel={ax.get('velocity')} err=0x{(ax.get('error') or 0):04X} "
                  f"sw=0x{(ax.get('statusword') or 0):04X} "
                  f"en={ax.get('enabled')} reached={ax.get('reached')}")
    s.close()


if __name__ == "__main__":
    main()
