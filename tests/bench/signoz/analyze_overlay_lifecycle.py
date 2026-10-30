#!/usr/bin/env python3
"""Parse saved ftrace lifecycle probes. Read evidence; never operate the guest."""

import argparse
import json
import re
from collections import Counter, defaultdict
from pathlib import Path

ARGS = {
    "fill_super": ("sb", "fc"),
    "trylock": ("dentry",),
    "unlock": ("dentry",),
    "free_fs": ("ofs",),
    "put_super": ("sb",),
    "cleanup_mnt": ("mount",),
    "deactivate_super": ("sb",),
    "put_mnt_ns": ("ns",),
    "mntput": ("mount",),
    "namespace_unlock": (),
    "exit_namespaces": ("task",),
}
PREFIX = re.compile(r"^\s*(.+)-(\d+)\s+\[(\d+)\]\s+\S+\s+(\d+\.\d+):\s*(.*)$")
FIELDS = re.compile(r"\b(\w+)=(-?0x[\da-fA-F]+|-?\d+)\b")


def parse(source):
    events, errors, last, stack, marker = [], [], {}, None, None
    for line, raw in enumerate(source.splitlines(), 1):
        if raw.lstrip().startswith("=>"):
            if stack is None:
                errors.append({"line": line, "error": "orphan stack frame"})
            else:
                stack.setdefault("stack", []).append(raw.strip()[3:])
            continue
        stack = None
        if not raw.strip() or raw.startswith("#"):
            continue
        match = PREFIX.match(raw)
        if not match:
            errors.append({"line": line, "error": "unparsed line", "raw": raw})
            continue
        comm, tid, cpu, timestamp, body = match.groups()
        sec, fraction = timestamp.split(".")
        if len(fraction) > 9:
            raise ValueError("Timestamp precision exceeds nanoseconds")
        name, _, payload = body.partition(": ")
        if name == body:
            name = "stack" if body == "<stack trace>" else body.split("(")[0].split(" ->")[0]
            payload = body
        syscall = re.match(r"sys_(mount|umount|unshare)(\(| -> )", body)
        if syscall:
            direction = "enter" if syscall[2] == "(" else "exit"
            name, payload = f"sys_{direction}_{syscall[1]}", body
        event = {
            "line": line,
            "comm": comm.strip(),
            "tid": int(tid),
            "cpu": int(cpu),
            "timestamp": timestamp,
            "ns": int(sec) * 10**9 + int(fraction.ljust(9, "0")),
            "resolution_ns": 10 ** (9 - len(fraction)),
            "name": name,
            "payload": payload,
            "args": {k: hex(int(v, 16)) if v.startswith("0x") else v for k, v in FIELDS.findall(payload)},
            "marker": marker,
        }
        if name == "tracing_mark_write" and payload.strip():
            marker = payload.strip()
            event["marker"] = marker
        if name == "stack":
            previous = last.get((event["tid"], event["cpu"]))
            if previous and previous["name"] == "unlock":
                stack = previous
                stack["stack_line"] = line
            else:
                errors.append({"line": line, "error": "stack without preceding unlock"})
        else:
            last[event["tid"], event["cpu"]] = event
        events.append(event)
    return events, errors


def pair(events):
    calls, stacks, issues = [], defaultdict(list), []
    for event in events:
        returning = event["name"].endswith("_ret")
        name = event["name"][:-4] if returning else event["name"]
        if name not in ARGS:
            continue
        stack = stacks[event["tid"]]
        signature = tuple(event["args"].get(key) for key in ARGS[name])
        if returning and name in ("fill_super", "trylock") and "ret" not in event["args"]:
            issues.append({"line": event["line"], "error": "missing return value"})
        if None in signature:
            issues.append({"line": event["line"], "error": "missing original argument"})
        if not returning:
            call = {
                "id": len(calls),
                "function": name,
                "entry": event,
                "exit": None,
                "ancestors": [c["id"] for c in stack],
                "status": "right_censored",
            }
            calls.append(call)
            stack.append(call)
            continue
        top = stack[-1] if stack else None
        if (
            top
            and top["function"] == name
            and None not in signature
            and signature == tuple(top["entry"]["args"].get(key) for key in ARGS[name])
        ):
            top["exit"], top["status"] = event, "complete"
            stack.pop()
        else:
            status = "left_censored" if not stack else "mismatch"
            issues.append(
                {
                    "line": event["line"],
                    "error": status,
                    "stack": [c["id"] for c in stack],
                    "return": event,
                }
            )
            # A gap invalidates every open context. Never pair a deeper frame by force.
            for call in stack:
                call["status"] = "gap_censored"
            stack.clear()
            calls.append(
                {
                    "id": len(calls),
                    "function": name,
                    "entry": None,
                    "exit": event,
                    "ancestors": [],
                    "status": status,
                }
            )
    return calls, issues


def analyze(source):
    events, parse_errors = parse(source)
    calls, pairing_issues = pair(events)
    complete = [c for c in calls if c["status"] == "complete"]
    cleanups, fills = defaultdict(list), defaultdict(list)
    for call in complete:
        args = call["entry"]["args"]
        if call["function"] == "cleanup_mnt":
            cleanups[args["mount"]].append(call)
        if call["function"] == "fill_super":
            fills[args["sb"]].append(call)
    for group in cleanups.values():
        group.sort(key=lambda c: c["exit"]["line"])
    context, relations, lifetime_issues, released = {}, [], [], set()
    for call in complete:
        name, event = call["function"], call["entry"]
        args, scope = event["args"], {}
        ancestors = [calls[i] for i in call["ancestors"]]
        valid = all(c["status"] == "complete" for c in ancestors)
        if not valid:
            continue
        if "mount" in args:
            prior = [c for c in cleanups[args["mount"]] if c["exit"]["line"] < event["line"]]
            scope["mount"] = {
                "address": args["mount"],
                "segment": len(prior) + 1,
                "after_cleanup": prior[-1]["id"] if prior else None,
            }
        if name == "fill_super":
            scope["sb"] = {"address": args["sb"], "fill_call": call["id"]}
        elif name == "put_super":
            candidates = [c for c in fills[args["sb"]] if c["entry"]["line"] < event["line"]]
            owner = candidates[-1] if candidates else None
            if (
                owner
                and owner["id"] in context
                and owner["exit"]["args"].get("ret") == "0"
                and owner["id"] not in released
            ):
                scope["sb"] = {"address": args["sb"], "fill_call": owner["id"]}
                released.add(owner["id"])
            else:
                if owner and owner["id"] in released:
                    lifetime_issues.append(
                        {
                            "call": call["id"],
                            "error": "repeated teardown without a new observed sb creation",
                        }
                    )
                scope["sb"] = {"address": args["sb"], "unobserved_creation": call["id"]}
        if name in ("free_fs", "put_mnt_ns"):
            key = "ofs" if name == "free_fs" else "ns"
            scope[key] = {"address": args[key], "release_call": call["id"]}
        inherited = {}
        for ancestor in ancestors:
            inherited.update(context.get(ancestor["id"], {}))
        inherited.update(scope)
        context[call["id"]] = inherited
        if name in ("put_super", "free_fs", "unlock") or (name == "mntput" and "ns" in inherited):
            relations.append({"call": call["id"], "context": inherited})
    locks, chains = [], []
    for call in complete:
        if call["function"] != "trylock" or call["exit"]["args"].get("ret") != "1":
            continue
        scope = context.get(call["id"], {})
        dentry = call["entry"]["args"]["dentry"]
        releases = [
            u
            for u in complete
            if u["function"] == "unlock"
            and u["entry"]["args"]["dentry"] == dentry
            and u["entry"]["line"] > call["exit"]["line"]
            and "sb" in scope
            and context.get(u["id"], {}).get("sb") == scope["sb"]
        ]
        release = releases[0] if len(releases) == 1 else None
        lease = {
            "id": call["id"],
            "dentry": dentry,
            "lock_call": call["id"],
            "context": scope,
            "unlock_call": release["id"] if release else None,
            "status": "bounded" if release else "unresolved_release",
        }
        locks.append(lease)
        if not release:
            continue
        for failed in complete:
            if failed["function"] != "trylock" or failed["exit"]["args"].get("ret") != "0":
                continue
            if failed["entry"]["args"]["dentry"] != dentry or failed["id"] not in context:
                continue
            if (
                call["exit"]["ns"] + call["exit"]["resolution_ns"] <= failed["entry"]["ns"]
                and failed["exit"]["ns"] + failed["exit"]["resolution_ns"] <= release["entry"]["ns"]
            ):
                related = set(call["ancestors"] + failed["ancestors"] + release["ancestors"])
                related.update((call["id"], failed["id"], release["id"]))
                release_scope = context.get(release["id"], {})
                namespace_releases = [
                    r
                    for r in relations
                    if "ns" in r["context"]
                    and "mount" in release_scope
                    and r["context"].get("mount") == release_scope["mount"]
                    and calls[r["call"]]["exit"]["line"] < release["entry"]["line"]
                ]
                for relation in namespace_releases:
                    related.update(calls[relation["call"]]["ancestors"] + [relation["call"]])
                chains.append(
                    {
                        "dentry": dentry,
                        "lease": lease["id"],
                        "old_lock": call["id"],
                        "new_failed_lock": failed["id"],
                        "old_unlock": release["id"],
                        "old_context": scope,
                        "new_context": context.get(failed["id"], {}),
                        "release_context": release_scope,
                        "namespace_releases": namespace_releases,
                        "failure_to_unlock_entry_ns": release["entry"]["ns"] - failed["exit"]["ns"],
                        "calls": [calls[i] for i in sorted(related)],
                    }
                )
    header = re.search(r"entries-in-buffer/entries-written:\s*(\d+)/(\d+)\s+#P:(\d+)", source)
    integrity = {
        "parse_errors": parse_errors,
        "pairing_issues": pairing_issues,
        "lifetime_issues": lifetime_issues,
        "calls_by_status": dict(Counter(c["status"] for c in calls)),
        "events_by_name": dict(Counter(e["name"] for e in events)),
        "parsed_events": len(events),
    }
    if header:
        buffered, written, cpus = map(int, header.groups())
        integrity.update(
            header={"buffered": buffered, "written": written, "cpus": cpus},
            header_count_matches=buffered == written == len(events),
        )
    return {
        "integrity": integrity,
        "chains": chains,
        "lock_leases": locks,
        "nested_relations": relations,
        "calls": calls,
        "markers": [e for e in events if e["name"] == "tracing_mark_write"],
    }


def evidence_integrity(directory, cpus):
    stats, errors, profile = {}, [], {}
    for cpu in range(cpus):
        for phase in ("before", "after"):
            path = directory / f"cpu{cpu}.stats-{phase}"
            if not path.is_file():
                errors.append(f"Missing {path.name}")
                continue
            fields = dict(line.split(":", 1) for line in path.read_text().splitlines() if ":" in line)
            stats[path.name] = {k: int(fields[k]) for k in ("overrun", "commit overrun", "dropped events")}
    profile_path = directory / "kprobe_profile"
    if profile_path.is_file():
        for line in profile_path.read_text().splitlines():
            name, hits, misses = line.split()
            profile[name] = {"hits": int(hits), "misses": int(misses)}
    expected = {name + suffix for name in ARGS for suffix in ("", "_ret")}
    missing = sorted(expected - profile.keys())
    loss_free = bool(stats) and not errors and all(v == 0 for row in stats.values() for v in row.values())
    return {
        "cpu_stats": stats,
        "errors": errors,
        "loss_free": loss_free,
        "kprobe_profile": profile,
        "missing_profiles": missing,
        "zero_misses": bool(profile) and not missing and all(p["misses"] == 0 for p in profile.values()),
    }


def self_test():
    def event(i, name, args="", tid=7, cpu=0):
        return f" task-{tid} [{cpu:03}] ..... 1.{i:06}: {name}: {args}"

    source = "\n".join(
        [
            event(1, "mntput", "mount=0x01"),
            event(2, "mntput", "mount=0x1"),
            event(3, "mntput_ret", "mount=0x1", cpu=1),
            event(4, "mntput_ret", "mount=0x1", cpu=2),
        ]
    )
    result = analyze(source)
    assert result["integrity"]["calls_by_status"] == {"complete": 2}
    assert result["calls"][1]["ancestors"] == [0] and result["calls"][0]["exit"]["cpu"] == 2
    result = analyze(
        "\n".join(
            [
                event(1, "mntput", "mount=0x1"),
                event(2, "mntput", "mount=0x2"),
                event(3, "mntput_ret", "mount=0x1"),
            ]
        )
    )
    assert result["integrity"]["calls_by_status"] == {"gap_censored": 2, "mismatch": 1}
    result = analyze("\n".join([event(1, "mntput_ret", "mount=0x1"), event(2, "mntput", "mount=0x2")]))
    assert result["integrity"]["calls_by_status"] == {
        "left_censored": 1,
        "right_censored": 1,
    }
    result = analyze(
        "\n".join(
            [
                event(1, "cleanup_mnt", "mount=0x1"),
                event(2, "cleanup_mnt_ret", "mount=0x1"),
                event(3, "put_mnt_ns", "ns=0x2"),
                event(4, "mntput", "mount=0x1"),
                event(5, "mntput_ret", "mount=0x1"),
                event(6, "put_mnt_ns_ret", "ns=0x2"),
            ]
        )
    )
    assert result["nested_relations"][0]["context"]["mount"]["segment"] == 2
    return {"passed": ["recursion", "CPU migration", "pointer mismatch", "truncation", "mount address reuse"]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("trace", nargs="?", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--self-test", action="store_true")
    options = parser.parse_args()
    if options.self_test:
        print(json.dumps(self_test()))
        return
    if not options.trace:
        parser.error("Supply a trace path or --self-test")
    result = analyze(options.trace.read_text())
    result["trace"] = str(options.trace.resolve())
    result["integrity"]["capture"] = evidence_integrity(
        options.trace.parent, result["integrity"].get("header", {}).get("cpus", 0)
    )
    rendered = json.dumps(result, indent=2) + "\n"
    if options.output:
        with options.output.open("x") as stream:
            stream.write(rendered)
    else:
        print(rendered, end="")


if __name__ == "__main__":
    main()
