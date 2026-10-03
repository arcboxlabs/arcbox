#!/usr/bin/env python3
"""Precise vsock RPC latency probe: N MachineService/Ping calls over one Connect
(HTTP/1.1 JSON) connection to the daemon's unix socket. Prints p50/p90/p99/max ms."""
import http.client, json, socket, sys, time
sock_path, label, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
class UnixConn(http.client.HTTPConnection):
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); self.sock.connect(sock_path)
c = UnixConn("localhost")
body = json.dumps({"id": "default"})
hdr = {"Content-Type": "application/json", "Connect-Protocol-Version": "1"}
def once():
    t = time.perf_counter()
    c.request("POST", "/arcbox.v1.MachineService/Ping", body=body, headers=hdr)
    r = c.getresponse(); data = r.read()
    if r.status != 200: raise SystemExit(f"status {r.status}: {data[:200]}")
    return (time.perf_counter() - t) * 1000
once()  # warm the connection
xs = sorted(once() for _ in range(n))
q = lambda p: round(xs[min(len(xs)-1, int(len(xs)*p))], 3)
print(json.dumps({"label": label, "n": n, "rpc_ping_ms": {"p50": q(0.5), "p90": q(0.9), "p99": q(0.99), "max": round(xs[-1], 3), "min": round(xs[0], 3)}}))
