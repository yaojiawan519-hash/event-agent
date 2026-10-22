#!/usr/bin/env python3
# 证明「设备本机上的任意 agent 进程」也能直接经总线 JSON-RPC 控制电机（不经过 web / WS）。
# 设备上运行：python3 bus_motor_probe.py [delta]
import json, socket, sys, time

DELTA = int(sys.argv[1]) if len(sys.argv) > 1 else 0

s = socket.create_connection(("127.0.0.1", 7700), timeout=10)
s.settimeout(0.2)
buf = b""
inbox = []


def pump(deadline):
    global buf
    while time.time() < deadline:
        try:
            d = s.recv(65536)
            if not d:
                return
            buf += d
        except socket.timeout:
            pass
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            try:
                m = json.loads(line)
            except Exception:
                continue
            if "id" in m:
                inbox.append(m)


def call(i, method, data, timeout=10):
    s.sendall((json.dumps({"jsonrpc": "2.0", "id": i, "method": method,
                           "params": {"source": "bus-probe", "ts": "0", "data": data}}) + "\n").encode())
    dl = time.time() + timeout
    while time.time() < dl:
        for m in inbox:
            if m.get("id") == i:
                inbox.remove(m)
                return m
        pump(min(dl, time.time() + 0.2))
    return None


# 注册到总线（名字唯一；纯客户端订阅空、不提供任何方法）
r = call("r0", "sys.register", {"name": f"bus-probe-{int(time.time())}",
                                "subscribe": [], "provide": [], "actions": []})
print("register:", json.dumps(r.get("result") if r else None, ensure_ascii=False))


def axis(tag):
    r = call("s" + tag, "action.motor.read", {}, timeout=3)
    a = r["result"]["axes"][0]
    print(f"  {tag:<8} pos={a['position']:>7} ({(a['position'] or 0)/20000:+.2f} 圈) "
          f"vel={a['velocity']:>6} err=0x{a['error']:04X} en={a['enabled']} reached={a['reached']}")


axis("before")
if DELTA:
    print("action.motor.enable ->", json.dumps(call("e", "action.motor.enable", {}).get("result")))
    time.sleep(1.5)
    print(f"action.motor.move_rel({DELTA}) ->",
          json.dumps(call("m", "action.motor.move_rel", {"axis": 0, "delta": DELTA}).get("result")))
    t0 = time.time()
    while time.time() - t0 < 20:
        time.sleep(1.0)
        r = call("p", "action.motor.read", {}, timeout=3)
        a = r["result"]["axes"][0]
        if a["reached"] or abs(a["velocity"]) == 0 and time.time() - t0 > 2:
            break
axis("after")
