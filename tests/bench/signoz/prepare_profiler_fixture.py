"""Copy a verified restart fixture and disable selected global profilers.

Usage: python3 prepare_profiler_fixture.py FIXTURE NEW_DIRECTORY --disable cpu|real|both
"""

import argparse
import hashlib
import json
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("fixture", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--disable", choices=("cpu", "real", "both"), required=True)
    args = parser.parse_args()
    fixture = json.loads(args.fixture.read_text())
    source = Path(fixture["config_root"])
    output = args.output.resolve()
    output.mkdir()
    config_root = output / "configs"
    modes = ("cpu", "real") if args.disable == "both" else (args.disable,)
    settings = {f"global_profiler_{mode}_time_period_ns": 0 for mode in modes}
    changes = []
    for row in fixture["configs"]:
        kind, relative, target, expected = row
        data = (source / relative).read_bytes()
        if hashlib.sha256(data).hexdigest() != expected:
            raise ValueError(f"Source config hash changed: {relative}.")
        if kind == "ch" and target == "/etc/clickhouse-server/config-0-0.yaml":
            if not data.endswith(b"\n") or any(name.encode() in data for name in settings):
                raise ValueError("The source must end with a newline and use defaults for the selected profilers.")
            data += "".join(f"{name}: {value}\n" for name, value in settings.items()).encode()
        destination = config_root / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)
        row[3] = hashlib.sha256(data).hexdigest()
        if row[3] != expected:
            changes.append({"relative": relative, "before_sha256": expected,
                            "after_sha256": row[3], "added_settings": settings})
    if len(changes) != 1:
        raise ValueError("Expected exactly one changed ClickHouse configuration.")
    fixture["config_root"] = str(config_root)
    (output / "fixture.json").write_text(json.dumps(fixture, indent=2) + "\n")
    (output / "config-diff.json").write_text(json.dumps(changes, indent=2) + "\n")
    print(json.dumps({"fixture": str(output / "fixture.json"), "changes": changes}))


if __name__ == "__main__":
    main()
