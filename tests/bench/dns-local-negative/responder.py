#!/usr/bin/env python3
"""Stand-in for the daemon's DNS listener on 127.0.0.1:<port>.

Answers every query the same way, chosen by MODE, so `lookup.sh` can time
what a getaddrinfo client does with each kind of unicast answer for a
`.local` name. Prints one line per received query.

MODE:
  silent        bind only, never answer
  nxdomain      RCODE 3, no records, no SOA
  nxdomain-soa  RCODE 3 with an SOA in the authority section
  servfail      RCODE 2
  refused       RCODE 5
  nodata        NOERROR, no records (what the daemon answers for unknown names)
  positive      A 127.0.0.2 and AAAA ::1
  a-only        A 127.0.0.2, NODATA for every other type
  a-silent      A 127.0.0.2, no answer for every other type
"""

import socket
import struct
import sys
import time


def name(labels):
    return b"".join(bytes([len(label)]) + label.encode() for label in labels) + b"\0"


def soa_rr(zone):
    rdata = (
        name(["ns", *zone])
        + name(["hostmaster", *zone])
        + struct.pack("!IIIII", 1, 3600, 600, 86400, 5)
    )
    return name(zone) + struct.pack("!HHIH", 6, 1, 5, len(rdata)) + rdata


def main():
    mode, port = sys.argv[1], int(sys.argv[2]) if len(sys.argv) > 2 else 5553
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("127.0.0.1", port))
    print(f"bound 127.0.0.1:{port} mode={mode}", flush=True)
    started = time.monotonic()
    while True:
        data, src = sock.recvfrom(512)
        offset, labels = 12, []
        while data[offset]:
            labels.append(data[offset + 1 : offset + 1 + data[offset]].decode(errors="replace"))
            offset += 1 + data[offset]
        qtype = int.from_bytes(data[offset + 1 : offset + 3], "big")
        question_end = offset + 5
        print(f"{time.monotonic() - started:6.2f}s {'.'.join(labels)} type={qtype}", flush=True)
        if mode == "silent" or (mode == "a-silent" and qtype != 1):
            continue
        header = bytearray(data[:12])
        header[6:12] = b"\0" * 6
        body = b""
        if mode in ("positive", "a-only", "a-silent"):
            header[2], header[3] = 0x85, 0x80
            if qtype == 1:
                header[7] = 1
                body = b"\xc0\x0c" + struct.pack("!HHIH", 1, 1, 30, 4) + bytes([127, 0, 0, 2])
            elif qtype == 28 and mode == "positive":
                header[7] = 1
                body = b"\xc0\x0c" + struct.pack("!HHIH", 28, 1, 30, 16) + b"\0" * 15 + b"\1"
        elif mode == "nodata":
            header[2], header[3] = 0x85, 0x80
        elif mode.startswith("nxdomain"):
            header[2], header[3] = 0x85, 0x83
            if mode == "nxdomain-soa":
                header[9] = 1
                body = soa_rr(labels[1:])
        else:
            header[2], header[3] = 0x81, 0x80 | {"servfail": 2, "refused": 5}[mode]
        sock.sendto(bytes(header) + data[12:question_end] + body, src)


if __name__ == "__main__":
    main()
