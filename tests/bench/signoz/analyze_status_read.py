"""Check the dynamic procfs comparison produced by probe_status_read.sh."""

import hashlib
import json
from pathlib import Path
import re
import sys

directory = Path(sys.argv[1])
required = {"Name", "State", "Uid", "SigPnd", "SigBlk", "Cpus_allowed_list",
            "voluntary_ctxt_switches", "nonvoluntary_ctxt_switches"}
report = {}
for line in (directory / "SHA256SUMS").read_text().splitlines():
    expected, name = line.split()
    assert hashlib.sha256((directory / Path(name).name).read_bytes()).hexdigest() == expected
for mode in ("direct", "buffered"):
    raw = (directory / (mode + ".txt")).read_text()
    pattern = rf"^begin\t{mode}\t(\d+)\n(.*?)^end\t{mode}\t\1\n"
    blocks = re.findall(pattern, raw, re.M | re.S)
    assert [int(number) for number, _ in blocks] == list(range(10000))
    damaged = []
    for number, body in blocks:
        fields = {line.partition(":")[0] for line in body.splitlines()}
        missing = required - fields
        if missing:
            damaged.append({"sample": int(number), "missing": sorted(missing), "raw": body})
    report[mode] = {"samples": len(blocks), "damaged_records": len(damaged), "examples": damaged[:5]}
assert report["direct"]["damaged_records"] > 0, "The direct-read baseline did not reproduce corruption."
assert report["buffered"]["damaged_records"] == 0, "The buffered read still corrupts status records."
print(json.dumps(report, indent=2))
