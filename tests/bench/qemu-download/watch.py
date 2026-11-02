#!/usr/bin/env python3
"""Prints every SetupStatus update from a Connect JSON stream on stdin.

Feed it `curl -N` output for SystemService/WatchSetupStatus; one line per
update, with seconds since the first:

    3.52 DOWNLOADING TRANSFERRING 2424832/5930315 Downloading QEMU 0.0.0-dev.0169f2b
"""

import json
import struct
import sys
import time

stream = sys.stdin.buffer
start = None
last = None
while True:
    header = stream.read(5)
    if len(header) < 5:
        break
    flags, length = struct.unpack(">BI", header)
    payload = stream.read(length)
    if flags & 0x02:
        print(f"end of stream: {payload.decode(errors='replace')}", flush=True)
        break
    status = json.loads(payload)
    system = status.get("systemRuntime", {})
    download = system.get("download")
    line = system.get("state", "STATE_UNSPECIFIED")
    if download:
        line += " {} {}/{}".format(
            download.get("stage", "STAGE_UNSPECIFIED"),
            download.get("downloadedBytes", "0"),
            download.get("totalBytes", "0"),
        )
    line += " " + system.get("reason", "")
    if line == last:
        continue
    last = line
    now = time.monotonic()
    start = start or now
    print(f"{now - start:6.2f} {line}", flush=True)
