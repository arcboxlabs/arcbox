import errno
import fcntl
import json
import os
import stat
import subprocess
import tempfile
import time
from pathlib import Path

LOOP_CTL_ADD = 0x4C80
LOOP_CTL_REMOVE = 0x4C81
LOOP_CLR_FD = 0x4C01
control = os.open("/dev/loop-control", os.O_RDWR | os.O_CLOEXEC)
root = Path(tempfile.mkdtemp(prefix="arcbox-loop-detach-probe-"))
index = None
results = []
owned = set()


def run(args):
    completed = subprocess.run(args, text=True, capture_output=True)
    record = {
        "command": args,
        "returncode": completed.returncode,
        "stdout": completed.stdout.strip(),
        "stderr": completed.stderr.strip(),
    }
    print(json.dumps(record), flush=True)
    return record


def backing():
    try:
        return (Path("/sys/block") / f"loop{index}" / "loop/backing_file").read_text().strip()
    except FileNotFoundError:
        return None


def require_backing(expected):
    actual = backing()
    if actual != expected:
        raise RuntimeError(
            f"Unexpected backing identity: expected {expected!r}, received {actual!r}"
        )


def await_release():
    deadline = time.monotonic() + 5
    while backing() is not None:
        if time.monotonic() >= deadline:
            raise RuntimeError("Probe association did not disappear within five seconds")
        time.sleep(0.02)


try:
    fcntl.ioctl(control, LOOP_CTL_ADD, 60000)
    index = 60000
    node = root / "probe-loop"
    os.mknod(node, stat.S_IFBLK | 0o600, os.makedev(7, index))
    print(
        json.dumps({"kernel": os.uname().release, "probe_root": str(root), "reserved_loop": index}),
        flush=True,
    )
    for name, prefix in [
        ("util-linux", ["/usr/sbin/losetup"]),
        ("busybox", ["/usr/bin/busybox", "losetup"]),
    ]:
        original = root / f"{name}-original.img"
        reused = root / f"{name}-reused.img"
        for image in (original, reused):
            with image.open("wb") as image_file:
                image_file.truncate(4 * 1024 * 1024)
            owned.add(str(image))
        require_backing(None)
        assert run([*prefix, str(node), str(original)])["returncode"] == 0
        require_backing(str(original))
        assert run([*prefix, "-d", str(node)])["returncode"] == 0
        await_release()
        require_backing(None)
        idle = run([*prefix, "-d", str(node)])
        assert idle["returncode"] != 0
        require_backing(None)
        descriptor = os.open(node, os.O_RDONLY | os.O_CLOEXEC)
        try:
            try:
                fcntl.ioctl(descriptor, LOOP_CLR_FD, 0)
            except OSError as error:
                idle_errno = error.errno
            else:
                raise RuntimeError("LOOP_CLR_FD unexpectedly succeeded for an idle probe device")
        finally:
            os.close(descriptor)
        assert idle_errno == errno.ENXIO
        assert run([*prefix, str(node), str(reused)])["returncode"] == 0
        require_backing(str(reused))
        replacement = run([*prefix, "-d", str(node)])
        assert replacement["returncode"] == 0
        await_release()
        results.append(
            {
                "tool": name,
                "idle_second_detach_status": idle["returncode"],
                "idle_ioctl_errno": idle_errno,
                "reused_number_detach_status": replacement["returncode"],
            }
        )
    print(json.dumps({"results": results}), flush=True)
finally:
    if index is not None:
        current = backing()
        if current is not None:
            if current not in owned:
                raise RuntimeError(f"Refuse cleanup of non-probe backing {current!r}")
            require_backing(current)
            cleanup = run(["/usr/sbin/losetup", "-d", str(root / "probe-loop")])
            if cleanup["returncode"] != 0:
                raise RuntimeError("Probe detach cleanup failed")
            await_release()
        require_backing(None)
        fcntl.ioctl(control, LOOP_CTL_REMOVE, index)
    os.close(control)
    for child in root.iterdir():
        child.unlink()
    root.rmdir()
    print(
        json.dumps({"cleanup_complete": True, "probe_root_absent": not root.exists()}), flush=True
    )
