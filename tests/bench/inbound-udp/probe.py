"""Check published UDP association identity through a disposable container."""

import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time

docker = os.environ.get("DOCKER", "docker")
name = f"arcbox-inbound-udp-{os.getpid()}"


def run(*arguments):
    return subprocess.run(
        [docker, *arguments], check=True, capture_output=True, text=True
    ).stdout.strip()


with tempfile.TemporaryDirectory(prefix="arcbox-inbound-udp-") as directory:
    Path(directory, "echo.cjs").write_text("""
const socket = require('node:dgram').createSocket('udp4');
socket.on('message', (data, peer) => {
  console.log(JSON.stringify({size:data.length, marker:data.at(-1), port:peer.port}));
  socket.send(data, peer.port, peer.address, error => { if (error) throw error; });
});
socket.bind(18081, '0.0.0.0', () => console.log('ready'));
""")
    run(
        "create",
        "--name",
        name,
        "-p",
        "127.0.0.1::18081/udp",
        "node:24-bookworm-slim",
        "node",
        "/fixture/echo.cjs",
    )
    try:
        run("cp", directory, f"{name}:/fixture")
        run("start", name)
        port = int(run("port", name, "18081/udp").rsplit(":", 1)[1])
        deadline = time.monotonic() + 5
        while "ready" not in run("logs", name):
            if time.monotonic() > deadline:
                raise TimeoutError("UDP echo did not listen")
            time.sleep(0.05)
        with (
            socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as first,
            socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as second,
        ):
            clients = [first, second]
            for client in clients:
                client.settimeout(2)
                client.connect(("127.0.0.1", port))
            for size, flag in [
                (1200, 0xC0),
                (64, 0x60),
                (64, 0x20),
                (0, 0),
                (17, 0x22),
                (1200, 0xC0),
            ]:
                packets = [
                    bytes([flag]) * (size - 1) + bytes([marker]) if size else b""
                    for marker in [1, 2]
                ]
                for client, packet in zip(clients, packets):
                    assert client.send(packet) == len(packet)
                for client, packet in zip(clients, packets):
                    assert client.recv(65535) == packet
        events = [
            json.loads(line)
            for line in run("logs", name).splitlines()
            if line.startswith("{")
        ]
        assert len(events) == 12, events
        ports = [
            {event["port"] for event in events if event.get("marker") == marker}
            for marker in [1, 2]
        ]
        assert all(len(group) == 1 for group in ports), events
        assert ports[0].isdisjoint(ports[1]), events
        assert {event["port"] for event in events} == ports[0] | ports[1], events
        print(
            json.dumps(
                {
                    "hostname": socket.gethostname(),
                    "datagrams": len(events),
                    "client_ports": [list(group) for group in ports],
                }
            ),
            flush=True,
        )
    finally:
        print(run("logs", name), flush=True)
        run("rm", "-f", name)
