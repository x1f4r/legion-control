#!/usr/bin/env python3
"""High-speed loopback driver for `legion stream` benchmarks with exact kernel wait4 rusage.

Reads one JSON job specification on stdin, executes one measurement run against `legion stream`
(direct or via `op: "relay"` through a relay node), and prints one JSON result line on stdout.
"""

import io
import json
import os
import resource
import select
import subprocess
import sys
import tarfile
import time


CHUNK = 1024 * 1024  # 1 MiB pipe read/write chunks so driver overhead stays minimal.
CLK_TCK = os.sysconf("SC_CLK_TCK")


def clean_env(legion_home, user_home):
    env = dict(os.environ)
    env.pop("XDG_CONFIG_HOME", None)
    env.pop("XDG_STATE_HOME", None)
    env.pop("SSH_ORIGINAL_COMMAND", None)
    env["LEGION_HOME"] = legion_home
    env["HOME"] = user_home
    env["USERPROFILE"] = user_home
    return env


def read_proc_tree_snapshot(root_pid):
    """Best-effort snapshot of VmHWM (MiB) and CPU seconds for root_pid and its direct children."""
    out = {}

    def read_one(pid):
        hwm_mib = None
        cpu_s = None
        try:
            with open(f"/proc/{pid}/status", "r", encoding="utf-8") as f:
                for line in f:
                    if line.startswith("VmHWM:"):
                        hwm_mib = int(line.split()[1]) / 1024.0
                        break
        except OSError:
            pass
        try:
            with open(f"/proc/{pid}/stat", "r", encoding="utf-8") as f:
                raw = f.read()
            rp = raw.rfind(")")
            if rp != -1:
                fields = raw[rp + 2 :].split()
                cpu_s = (int(fields[11]) + int(fields[12])) / float(CLK_TCK)
        except OSError:
            pass
        return {"hwm_mib": hwm_mib, "cpu_s": cpu_s}

    out["root"] = read_one(root_pid)
    child_pids = set()
    try:
        for tid in os.listdir(f"/proc/{root_pid}/task"):
            try:
                with open(f"/proc/{root_pid}/task/{tid}/children", "r", encoding="utf-8") as f:
                    for tok in f.read().split():
                        child_pids.add(int(tok))
            except OSError:
                pass
    except OSError:
        pass
    children = []
    for cpid in sorted(child_pids):
        snap = read_one(cpid)
        if snap["hwm_mib"] is not None or snap["cpu_s"] is not None:
            children.append(snap)
    out["children"] = children
    return out


class BufferedFd:
    def __init__(self, fd, chunk=CHUNK):
        self.fd = fd
        self.chunk = chunk
        self.buf = bytearray()
        self.pos = 0

    def read_line(self):
        while True:
            nl = self.buf.find(b"\n", self.pos)
            if nl != -1:
                line = bytes(self.buf[self.pos : nl + 1])
                self.pos = nl + 1
                if self.pos >= len(self.buf):
                    self.buf.clear()
                    self.pos = 0
                return line
            if self.pos > 0:
                del self.buf[: self.pos]
                self.pos = 0
            data = os.read(self.fd, 65536)
            if not data:
                if self.buf:
                    line = bytes(self.buf)
                    self.buf.clear()
                    return line
                raise EOFError("unexpected EOF reading JSON line from legion stream")
            self.buf.extend(data)

    def read_json_skip_busy(self):
        while True:
            line = self.read_line()
            obj = json.loads(line)
            if isinstance(obj, dict) and obj == {"busy": True}:
                continue
            if isinstance(obj, dict) and "error" in obj:
                raise RuntimeError(f"legion stream error: {obj['error']}")
            return obj

    def drain_exact(self, total_bytes, sample_cb=None):
        remaining = total_bytes
        # Consume buffered bytes first.
        avail = len(self.buf) - self.pos
        if avail > 0:
            take = min(avail, remaining)
            self.pos += take
            remaining -= take
            if self.pos >= len(self.buf):
                self.buf.clear()
                self.pos = 0
        next_sample = time.perf_counter() + 0.05 if sample_cb else None
        while remaining > 0:
            want = min(self.chunk, remaining)
            data = os.read(self.fd, want)
            if not data:
                raise EOFError(f"unexpected EOF with {remaining} bytes remaining")
            remaining -= len(data)
            if sample_cb is not None:
                now = time.perf_counter()
                if now >= next_sample:
                    sample_cb()
                    next_sample = now + 0.05

    def drain_to_eof(self, sample_cb=None):
        total = len(self.buf) - self.pos
        self.buf.clear()
        self.pos = 0
        next_sample = time.perf_counter() + 0.05 if sample_cb else None
        while True:
            data = os.read(self.fd, self.chunk)
            if not data:
                return total
            total += len(data)
            if sample_cb is not None:
                now = time.perf_counter()
                if now >= next_sample:
                    sample_cb()
                    next_sample = now + 0.05


def write_all_fd(fd, data):
    view = memoryview(data)
    written = 0
    n = len(view)
    while written < n:
        k = os.write(fd, view[written:])
        written += k


def spawn_stream(binary, legion_home, user_home):
    proc = subprocess.Popen(
        [binary, "stream"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=clean_env(legion_home, user_home),
        bufsize=0,
    )
    return proc


def finish_proc(proc):
    if proc.stdin is not None:
        try:
            proc.stdin.close()
        except OSError:
            pass
    if proc.stdout is not None:
        try:
            proc.stdout.close()
        except OSError:
            pass
    stderr_text = ""
    if proc.stderr is not None:
        try:
            stderr_text = proc.stderr.read().decode("utf-8", errors="replace")
            proc.stderr.close()
        except OSError:
            pass
    _, status, ru = os.wait4(proc.pid, 0)
    code = os.waitstatus_to_exitcode(status)
    return code, stderr_text, ru


def wrap_header(inner, relay_on):
    if relay_on:
        return {
            "protocol": 1,
            "op": "relay",
            "on": relay_on,
            "request": inner,
        }
    return inner


def run_get(spec):
    ru0 = resource.getrusage(resource.RUSAGE_SELF)
    t0 = time.perf_counter()

    proc = spawn_stream(spec["bin"], spec["entry_legion_home"], spec["entry_user_home"])
    reader = BufferedFd(proc.stdout.fileno())
    greeting = reader.read_json_skip_busy()
    if "protocol" not in greeting:
        raise RuntimeError(f"missing protocol greeting: {greeting}")

    header = wrap_header(
        {"protocol": 1, "op": "get", "path": spec["remote_path"]},
        spec.get("relay_on"),
    )
    write_all_fd(proc.stdin.fileno(), (json.dumps(header) + "\n").encode("utf-8"))
    ready = reader.read_json_skip_busy()
    length = int(ready["length"])

    max_target_hwm = 0.0
    max_target_cpu = 0.0
    max_root_hwm = 0.0
    max_root_cpu = 0.0

    def sample():
        nonlocal max_target_hwm, max_target_cpu, max_root_hwm, max_root_cpu
        snap = read_proc_tree_snapshot(proc.pid)
        if snap["root"]["hwm_mib"] is not None:
            max_root_hwm = max(max_root_hwm, snap["root"]["hwm_mib"])
        if snap["root"]["cpu_s"] is not None:
            max_root_cpu = max(max_root_cpu, snap["root"]["cpu_s"])
        for c in snap["children"]:
            if c["hwm_mib"] is not None:
                max_target_hwm = max(max_target_hwm, c["hwm_mib"])
            if c["cpu_s"] is not None:
                max_target_cpu = max(max_target_cpu, c["cpu_s"])

    sample()
    reader.drain_exact(length, sample_cb=sample if spec.get("relay_on") else None)
    sample()
    code, stderr_text, child_ru = finish_proc(proc)
    wall_s = time.perf_counter() - t0
    ru1 = resource.getrusage(resource.RUSAGE_SELF)

    if code != 0:
        raise RuntimeError(f"legion stream get exited {code}: {stderr_text}")

    client_cpu_s = (ru1.ru_utime - ru0.ru_utime) + (ru1.ru_stime - ru0.ru_stime)
    server_total_cpu_s = child_ru.ru_utime + child_ru.ru_stime
    server_rss_mib = child_ru.ru_maxrss / 1024.0
    res = {
        "bytes": length,
        "wall_ms": wall_s * 1000.0,
        "mib_s": (length / 1048576.0) / max(wall_s, 1e-9),
        "client_cpu_s": client_cpu_s,
        "server_cpu_s": server_total_cpu_s,
        "client_rss_mib": ru1.ru_maxrss / 1024.0,
        "server_rss_mib": server_rss_mib,
    }
    if spec.get("relay_on"):
        res["relay_rss_mib"] = max_root_hwm if max_root_hwm > 0 else server_rss_mib
        res["relay_cpu_s"] = max_root_cpu if max_root_cpu > 0 else server_total_cpu_s
        if max_target_hwm > 0:
            res["target_rss_mib"] = max_target_hwm
        if max_target_cpu > 0:
            res["target_cpu_s"] = max_target_cpu
    return res


def run_put(spec):
    src_path = spec["source_file"]
    size = os.path.getsize(src_path)
    dst_path = spec["dest_path"]
    if os.path.exists(dst_path):
        os.unlink(dst_path)

    ru0 = resource.getrusage(resource.RUSAGE_SELF)
    t0 = time.perf_counter()

    proc = spawn_stream(spec["bin"], spec["entry_legion_home"], spec["entry_user_home"])
    reader = BufferedFd(proc.stdout.fileno())
    greeting = reader.read_json_skip_busy()
    if "protocol" not in greeting:
        raise RuntimeError(f"missing protocol greeting: {greeting}")

    header = wrap_header(
        {
            "protocol": 1,
            "op": "put",
            "path": dst_path,
            "size": size,
            "hash": spec["blake3_hash"],
            "if_exists": "replace",
        },
        spec.get("relay_on"),
    )
    write_all_fd(proc.stdin.fileno(), (json.dumps(header) + "\n").encode("utf-8"))
    ready = reader.read_json_skip_busy()
    offset = int(ready.get("offset", 0))
    if offset != 0:
        raise RuntimeError(f"expected fresh put offset 0, got {offset}")

    max_target_hwm = 0.0
    max_target_cpu = 0.0
    max_root_hwm = 0.0
    max_root_cpu = 0.0

    def sample():
        nonlocal max_target_hwm, max_target_cpu, max_root_hwm, max_root_cpu
        snap = read_proc_tree_snapshot(proc.pid)
        if snap["root"]["hwm_mib"] is not None:
            max_root_hwm = max(max_root_hwm, snap["root"]["hwm_mib"])
        if snap["root"]["cpu_s"] is not None:
            max_root_cpu = max(max_root_cpu, snap["root"]["cpu_s"])
        for c in snap["children"]:
            if c["hwm_mib"] is not None:
                max_target_hwm = max(max_target_hwm, c["hwm_mib"])
            if c["cpu_s"] is not None:
                max_target_cpu = max(max_target_cpu, c["cpu_s"])

    sample()
    next_sample = time.perf_counter() + 0.05
    in_fd = proc.stdin.fileno()
    with open(src_path, "rb", buffering=0) as f:
        while True:
            buf = f.read(CHUNK)
            if not buf:
                break
            write_all_fd(in_fd, buf)
            if spec.get("relay_on"):
                now = time.perf_counter()
                if now >= next_sample:
                    sample()
                    next_sample = now + 0.05
    sample()
    proc.stdin.close()
    proc.stdin = None

    done = reader.read_json_skip_busy()
    if not done.get("ok"):
        raise RuntimeError(f"put did not finish ok: {done}")
    sample()
    code, stderr_text, child_ru = finish_proc(proc)
    wall_s = time.perf_counter() - t0
    ru1 = resource.getrusage(resource.RUSAGE_SELF)

    if code != 0:
        raise RuntimeError(f"legion stream put exited {code}: {stderr_text}")

    client_cpu_s = (ru1.ru_utime - ru0.ru_utime) + (ru1.ru_stime - ru0.ru_stime)
    server_total_cpu_s = child_ru.ru_utime + child_ru.ru_stime
    server_rss_mib = child_ru.ru_maxrss / 1024.0
    res = {
        "bytes": size,
        "wall_ms": wall_s * 1000.0,
        "mib_s": (size / 1048576.0) / max(wall_s, 1e-9),
        "client_cpu_s": client_cpu_s,
        "server_cpu_s": server_total_cpu_s,
        "client_rss_mib": ru1.ru_maxrss / 1024.0,
        "server_rss_mib": server_rss_mib,
    }
    if spec.get("relay_on"):
        res["relay_rss_mib"] = max_root_hwm if max_root_hwm > 0 else server_rss_mib
        res["relay_cpu_s"] = max_root_cpu if max_root_cpu > 0 else server_total_cpu_s
        if max_target_hwm > 0:
            res["target_rss_mib"] = max_target_hwm
        if max_target_cpu > 0:
            res["target_cpu_s"] = max_target_cpu
    return res


def run_pack(spec):
    names = spec["names"]
    payload_bytes = int(spec["payload_bytes"])
    ru0 = resource.getrusage(resource.RUSAGE_SELF)
    t0 = time.perf_counter()

    proc = spawn_stream(spec["bin"], spec["entry_legion_home"], spec["entry_user_home"])
    reader = BufferedFd(proc.stdout.fileno())
    reader.read_json_skip_busy()

    header = wrap_header(
        {"protocol": 1, "op": "pack", "root": spec["remote_root"], "names": names},
        spec.get("relay_on"),
    )
    write_all_fd(proc.stdin.fileno(), (json.dumps(header) + "\n").encode("utf-8"))
    ready = reader.read_json_skip_busy()
    if not ready.get("ready"):
        raise RuntimeError(f"pack not ready: {ready}")

    max_target_hwm = 0.0
    max_target_cpu = 0.0
    max_root_hwm = 0.0
    max_root_cpu = 0.0

    def sample():
        nonlocal max_target_hwm, max_target_cpu, max_root_hwm, max_root_cpu
        snap = read_proc_tree_snapshot(proc.pid)
        if snap["root"]["hwm_mib"] is not None:
            max_root_hwm = max(max_root_hwm, snap["root"]["hwm_mib"])
        if snap["root"]["cpu_s"] is not None:
            max_root_cpu = max(max_root_cpu, snap["root"]["cpu_s"])
        for c in snap["children"]:
            if c["hwm_mib"] is not None:
                max_target_hwm = max(max_target_hwm, c["hwm_mib"])
            if c["cpu_s"] is not None:
                max_target_cpu = max(max_target_cpu, c["cpu_s"])

    sample()
    wire_bytes = reader.drain_to_eof(sample_cb=sample if spec.get("relay_on") else None)
    sample()
    code, stderr_text, child_ru = finish_proc(proc)
    wall_s = time.perf_counter() - t0
    ru1 = resource.getrusage(resource.RUSAGE_SELF)

    if code != 0:
        raise RuntimeError(f"legion stream pack exited {code}: {stderr_text}")

    client_cpu_s = (ru1.ru_utime - ru0.ru_utime) + (ru1.ru_stime - ru0.ru_stime)
    server_total_cpu_s = child_ru.ru_utime + child_ru.ru_stime
    server_rss_mib = child_ru.ru_maxrss / 1024.0
    res = {
        "bytes": payload_bytes,
        "wire_bytes": wire_bytes,
        "files": len(names),
        "wall_ms": wall_s * 1000.0,
        "mib_s": (payload_bytes / 1048576.0) / max(wall_s, 1e-9),
        "client_cpu_s": client_cpu_s,
        "server_cpu_s": server_total_cpu_s,
        "client_rss_mib": ru1.ru_maxrss / 1024.0,
        "server_rss_mib": server_rss_mib,
    }
    if spec.get("relay_on"):
        res["relay_rss_mib"] = max_root_hwm if max_root_hwm > 0 else server_rss_mib
        res["relay_cpu_s"] = max_root_cpu if max_root_cpu > 0 else server_total_cpu_s
        if max_target_hwm > 0:
            res["target_rss_mib"] = max_target_hwm
        if max_target_cpu > 0:
            res["target_cpu_s"] = max_target_cpu
    return res


def run_unpack(spec):
    tar_path = spec["tar_file"]
    entries = spec["entries"]
    payload_bytes = int(spec["payload_bytes"])
    dst_root = spec["dest_root"]
    os.makedirs(dst_root, exist_ok=True)

    ru0 = resource.getrusage(resource.RUSAGE_SELF)
    t0 = time.perf_counter()

    proc = spawn_stream(spec["bin"], spec["entry_legion_home"], spec["entry_user_home"])
    reader = BufferedFd(proc.stdout.fileno())
    reader.read_json_skip_busy()

    header = wrap_header(
        {
            "protocol": 1,
            "op": "unpack",
            "root": dst_root,
            "entries": entries,
            "if_exists": "replace",
        },
        spec.get("relay_on"),
    )
    write_all_fd(proc.stdin.fileno(), (json.dumps(header) + "\n").encode("utf-8"))
    ready = reader.read_json_skip_busy()
    if not ready.get("ready"):
        raise RuntimeError(f"unpack not ready: {ready}")

    max_target_hwm = 0.0
    max_target_cpu = 0.0
    max_root_hwm = 0.0
    max_root_cpu = 0.0

    def sample():
        nonlocal max_target_hwm, max_target_cpu, max_root_hwm, max_root_cpu
        snap = read_proc_tree_snapshot(proc.pid)
        if snap["root"]["hwm_mib"] is not None:
            max_root_hwm = max(max_root_hwm, snap["root"]["hwm_mib"])
        if snap["root"]["cpu_s"] is not None:
            max_root_cpu = max(max_root_cpu, snap["root"]["cpu_s"])
        for c in snap["children"]:
            if c["hwm_mib"] is not None:
                max_target_hwm = max(max_target_hwm, c["hwm_mib"])
            if c["cpu_s"] is not None:
                max_target_cpu = max(max_target_cpu, c["cpu_s"])

    sample()
    next_sample = time.perf_counter() + 0.05
    in_fd = proc.stdin.fileno()
    with open(tar_path, "rb", buffering=0) as f:
        while True:
            buf = f.read(CHUNK)
            if not buf:
                break
            write_all_fd(in_fd, buf)
            if spec.get("relay_on"):
                now = time.perf_counter()
                if now >= next_sample:
                    sample()
                    next_sample = now + 0.05
    sample()
    proc.stdin.close()
    proc.stdin = None

    done = reader.read_json_skip_busy()
    if not done.get("ok"):
        raise RuntimeError(f"unpack did not finish ok: {done}")
    sample()
    code, stderr_text, child_ru = finish_proc(proc)
    wall_s = time.perf_counter() - t0
    ru1 = resource.getrusage(resource.RUSAGE_SELF)

    if code != 0:
        raise RuntimeError(f"legion stream unpack exited {code}: {stderr_text}")

    client_cpu_s = (ru1.ru_utime - ru0.ru_utime) + (ru1.ru_stime - ru0.ru_stime)
    server_total_cpu_s = child_ru.ru_utime + child_ru.ru_stime
    server_rss_mib = child_ru.ru_maxrss / 1024.0
    res = {
        "bytes": payload_bytes,
        "files": len(entries),
        "wall_ms": wall_s * 1000.0,
        "mib_s": (payload_bytes / 1048576.0) / max(wall_s, 1e-9),
        "client_cpu_s": client_cpu_s,
        "server_cpu_s": server_total_cpu_s,
        "client_rss_mib": ru1.ru_maxrss / 1024.0,
        "server_rss_mib": server_rss_mib,
    }
    if spec.get("relay_on"):
        res["relay_rss_mib"] = max_root_hwm if max_root_hwm > 0 else server_rss_mib
        res["relay_cpu_s"] = max_root_cpu if max_root_cpu > 0 else server_total_cpu_s
        if max_target_hwm > 0:
            res["target_rss_mib"] = max_target_hwm
        if max_target_cpu > 0:
            res["target_cpu_s"] = max_target_cpu
    return res


def run_get_keep_open_batch(spec):
    """Downloads many files or ranges over one warm keep_open stream (direct or relayed)."""
    requests = spec["requests"]  # list of {path, offset?, length?}
    ru0 = resource.getrusage(resource.RUSAGE_SELF)
    t0 = time.perf_counter()

    proc = spawn_stream(spec["bin"], spec["entry_legion_home"], spec["entry_user_home"])
    reader = BufferedFd(proc.stdout.fileno())
    reader.read_json_skip_busy()
    in_fd = proc.stdin.fileno()

    total_bytes = 0
    latencies_ms = []
    ttfb_ms = None
    relay_on = spec.get("relay_on")

    for idx, req in enumerate(requests):
        req_t0 = time.perf_counter()
        inner = {
            "protocol": 1,
            "op": "get",
            "path": req["path"],
            "keep_open": True,
        }
        if "offset" in req and req["offset"] is not None:
            inner["offset"] = int(req["offset"])
        if "length" in req and req["length"] is not None:
            inner["length"] = int(req["length"])
        header = wrap_header(inner, relay_on)
        write_all_fd(in_fd, (json.dumps(header) + "\n").encode("utf-8"))
        ready = reader.read_json_skip_busy()
        length = int(ready["length"])
        if idx == 0:
            # First byte arrival is right after ready header or first payload byte.
            ttfb_ms = (time.perf_counter() - req_t0) * 1000.0
        reader.drain_exact(length)
        done = reader.read_json_skip_busy()
        if not done.get("done"):
            raise RuntimeError(f"keep_open get missing done trailer: {done}")
        total_bytes += length
        latencies_ms.append((time.perf_counter() - req_t0) * 1000.0)

    proc.stdin.close()
    proc.stdin = None
    code, stderr_text, child_ru = finish_proc(proc)
    wall_s = time.perf_counter() - t0
    ru1 = resource.getrusage(resource.RUSAGE_SELF)

    if code != 0:
        raise RuntimeError(f"legion stream keep_open get exited {code}: {stderr_text}")

    latencies_ms.sort()
    p50_ms = latencies_ms[len(latencies_ms) // 2] if latencies_ms else 0.0
    p95_ms = latencies_ms[int(len(latencies_ms) * 0.95)] if latencies_ms else 0.0

    return {
        "bytes": total_bytes,
        "requests": len(requests),
        "wall_ms": wall_s * 1000.0,
        "ttfb_ms": ttfb_ms or 0.0,
        "req_p50_ms": p50_ms,
        "req_p95_ms": p95_ms,
        "mib_s": (total_bytes / 1048576.0) / max(wall_s, 1e-9),
        "client_cpu_s": (ru1.ru_utime - ru0.ru_utime) + (ru1.ru_stime - ru0.ru_stime),
        "server_cpu_s": child_ru.ru_utime + child_ru.ru_stime,
        "client_rss_mib": ru1.ru_maxrss / 1024.0,
        "server_rss_mib": child_ru.ru_maxrss / 1024.0,
    }


def run_stat_hash(spec):
    proc = spawn_stream(spec["bin"], spec["entry_legion_home"], spec["entry_user_home"])
    reader = BufferedFd(proc.stdout.fileno())
    reader.read_json_skip_busy()
    header = {"protocol": 1, "op": "stat", "path": spec["remote_path"], "checksum": True}
    write_all_fd(proc.stdin.fileno(), (json.dumps(header) + "\n").encode("utf-8"))
    reply = reader.read_json_skip_busy()
    code, stderr_text, _ = finish_proc(proc)
    if code != 0:
        raise RuntimeError(f"stat exited {code}: {stderr_text}")
    return reply


def run_build_tar(spec):
    """Builds a deterministic GNU tar archive from a directory of files for `unpack` benchmarks."""
    src_dir = spec["source_dir"]
    tar_path = spec["tar_file"]
    names = spec["names"]
    with tarfile.open(tar_path, "w", format=tarfile.GNU_FORMAT) as tf:
        for name in names:
            tf.add(os.path.join(src_dir, name), arcname=name, recursive=False)
    return {"tar_bytes": os.path.getsize(tar_path)}


def main():
    spec = json.loads(sys.stdin.read())
    mode = spec["mode"]
    if mode == "get":
        out = run_get(spec)
    elif mode == "put":
        out = run_put(spec)
    elif mode == "pack":
        out = run_pack(spec)
    elif mode == "unpack":
        out = run_unpack(spec)
    elif mode == "get_keep_open_batch":
        out = run_get_keep_open_batch(spec)
    elif mode == "stat_hash":
        out = run_stat_hash(spec)
    elif mode == "build_tar":
        out = run_build_tar(spec)
    else:
        raise ValueError(f"unknown mode: {mode}")
    sys.stdout.write(json.dumps(out) + "\n")


if __name__ == "__main__":
    main()
