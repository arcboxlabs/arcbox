"""Writes a gzipped newc cpio without root: mkcpio.py OUT BUSYBOX INIT
UDHCPC BENCH [DOCKER_BIN_DIR]."""
import gzip
import os
import stat
import sys

out, busybox, init, udhcpc, bench = sys.argv[1:6]
entries = []


def directory(name):
    entries.append((name, stat.S_IFDIR | 0o755, b"", 0))


def file(name, path):
    with open(path, "rb") as f:
        entries.append((name, stat.S_IFREG | 0o755, f.read(), 0))


for name in ["bin", "dev", "proc", "sys", "etc", "tmp", "sbin", "usr", "usr/bin", "usr/sbin"]:
    directory(name)
entries.append(("dev/console", stat.S_IFCHR | 0o600, b"", (5 << 8) | 1))
file("bin/busybox", busybox)
file("init", init)
file("udhcpc.sh", udhcpc)
file("bin/bench", bench)
if len(sys.argv) > 6:
    directory("usr/local")
    directory("usr/local/bin")
    for name in ["dockerd", "containerd", "containerd-shim-runc-v2", "runc", "docker-init"]:
        file("usr/local/bin/" + name, os.path.join(sys.argv[6], name))

archive = bytearray()
for ino, (name, mode, data, rdev) in enumerate(entries + [("TRAILER!!!", 0, b"", 0)], 1):
    encoded = name.encode() + b"\0"
    fields = [ino, mode, 0, 0, 1, 0, len(data), 0, 0, rdev >> 8, rdev & 0xFF, len(encoded), 0]
    archive += ("070701" + "".join("%08x" % v for v in fields)).encode() + encoded
    archive += b"\0" * (-len(archive) % 4) + data
    archive += b"\0" * (-len(archive) % 4)
with open(out, "wb") as f:
    f.write(gzip.compress(bytes(archive)))
